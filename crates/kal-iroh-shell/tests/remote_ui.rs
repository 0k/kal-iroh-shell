//! The client shell against a real release store and server behind a
//! fake upstream.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures::future::BoxFuture;
use http_body_util::BodyExt;
use kal_iroh_shell::{EmbeddedIndex, Host, RemoteUi, RemoteUiConfig, ShellCommand, prepare_script};
use kal_iroh_shell_server::{ReleaseServer, Store};
use tower::ServiceExt;

const RELEASES_BASE: &str = "/serve/my-app";
const RELEASE_INDEX: &str =
    r#"<script type="module" src="./assets/main.js"></script><div id="app"></div>"#;

/// The app side: upstream URL, setting, channel, admission verdict.
struct TestHost {
    upstream: tokio::sync::watch::Receiver<String>,
    enabled: AtomicBool,
    admitted: AtomicBool,
    channel: std::sync::Mutex<String>,
}

impl Host for TestHost {
    fn upstream(&self) -> tokio::sync::watch::Receiver<String> {
        self.upstream.clone()
    }
    fn client(&self) -> reqwest::Client {
        reqwest::Client::new()
    }
    fn remote_ui_enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }
    fn channel(&self) -> String {
        self.channel.lock().unwrap().clone()
    }
    fn authorized(&self, _grace: Duration) -> BoxFuture<'_, Result<(), String>> {
        let ok = self.admitted.load(Ordering::SeqCst);
        Box::pin(async move {
            if ok {
                Ok(())
            } else {
                Err("device not admitted".into())
            }
        })
    }
}

/// A finished build `id`: an index with a relative asset, and a manifest.
fn build(root: &Path, id: &str) -> PathBuf {
    let dist = root.join("builds").join(id);
    std::fs::create_dir_all(dist.join("assets")).unwrap();
    std::fs::write(dist.join("index.html"), RELEASE_INDEX).unwrap();
    std::fs::write(dist.join("assets/main.js"), format!("// {id}")).unwrap();
    std::fs::write(
        dist.join("ui-manifest.json"),
        format!(r#"{{"build_id":"{id}","files":[]}}"#),
    )
    .unwrap();
    dist
}

struct Fixture {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    store: Store,
    /// Keeps the release server's refs watch alive.
    _watcher: Box<dyn std::any::Any + Send>,
    /// Keeps the upstream URL channel open.
    _upstream_tx: tokio::sync::watch::Sender<String>,
    host: Arc<TestHost>,
    ui: Arc<RemoteUi>,
    shell_rx: tokio::sync::mpsc::UnboundedReceiver<ShellCommand>,
}

/// A release store (`r1` on `dev` and `stable` when `published`), its
/// server behind an upstream at [`RELEASES_BASE`], and a shell following
/// `stable`.
async fn fixture(published: bool) -> Fixture {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_path_buf();
    let store = Store::open_or_init(root.join("releases.git")).unwrap();
    if published {
        store.publish("dev", &build(&root, "r1"), "s").unwrap();
        store.promote("dev", "stable").unwrap();
    }
    let releases = ReleaseServer::new(store.clone());
    let watcher = releases.watch().unwrap();
    let upstream_app = axum::Router::new().nest_service(RELEASES_BASE, releases.router());
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, upstream_app).await.unwrap() });

    let (upstream_tx, upstream) = tokio::sync::watch::channel(url);
    let host = Arc::new(TestHost {
        upstream,
        enabled: AtomicBool::new(true),
        admitted: AtomicBool::new(true),
        channel: std::sync::Mutex::new("stable".into()),
    });
    let (shell_tx, shell_rx) = tokio::sync::mpsc::unbounded_channel();
    let ui = RemoteUi::new(
        RemoteUiConfig {
            releases_base: RELEASES_BASE.into(),
            admission_grace: Duration::from_millis(50),
        },
        host.clone(),
        Some(shell_tx),
    );
    Fixture {
        _tmp: tmp,
        root,
        store,
        _watcher: Box::new(watcher),
        _upstream_tx: upstream_tx,
        host,
        ui,
        shell_rx,
    }
}

fn embedded() -> EmbeddedIndex {
    EmbeddedIndex {
        html: "<div id=\"app\"></div><!-- embedded -->".into(),
        build_id: "built-in".into(),
    }
}

fn stable_commit(f: &Fixture) -> String {
    f.store
        .channels()
        .unwrap()
        .into_iter()
        .find(|h| h.channel == "stable")
        .unwrap()
        .commit
}

async fn get(router: &axum::Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let resp = router
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

async fn next_command(rx: &mut tokio::sync::mpsc::UnboundedReceiver<ShellCommand>) -> ShellCommand {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("a shell command within 5 s")
        .expect("channel open")
}

async fn answer(ui: &Arc<RemoteUi>, body: &'static str) {
    let ack = ui
        .routes()
        .oneshot(
            Request::post("/__shell/reload-ack")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ack.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn the_page_gets_its_channel_release_pointing_at_its_own_files() {
    let f = fixture(true).await;

    let (html, served) = f.ui.index(embedded()).await;
    let commit = served.commit.clone().expect("a remote page has a commit");
    let (status, headers, js) = get(
        &f.ui.routes(),
        &format!("/__shell/ui/{commit}/assets/main.js"),
    )
    .await;

    assert_eq!(served.source, "remote");
    assert_eq!(
        (served.build_id.as_str(), served.channel.as_deref()),
        ("r1", Some("stable"))
    );
    assert!(
        html.contains(&format!(r#"src="/__shell/ui/{commit}/assets/main.js""#)),
        "{html}"
    );
    assert_eq!(
        served.root_attributes(),
        r#" data-ui-source="remote" data-ui-build="r1""#
    );
    assert_eq!((status, js.as_str()), (StatusCode::OK, "// r1"));
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
}

#[tokio::test]
async fn the_built_in_interface_is_served_when_the_remote_one_cannot_be() {
    let off = fixture(true).await;
    off.host.enabled.store(false, Ordering::SeqCst);
    let not_admitted = fixture(true).await;
    not_admitted.host.admitted.store(false, Ordering::SeqCst);
    let nothing_published = fixture(false).await;

    for (f, reason) in [
        (&off, "remote interface setting is off"),
        (&not_admitted, "device not admitted"),
        (&nothing_published, "GET /channels/stable: HTTP 404"),
    ] {
        let (html, served) = f.ui.index(embedded()).await;
        assert_eq!(served.source, "embedded");
        assert_eq!(served.fallback_reason.as_deref(), Some(reason));
        assert!(html.ends_with("<!-- embedded -->"));
    }
    let commit = stable_commit(&not_admitted);
    let (status, _, _) = get(
        &not_admitted.ui.routes(),
        &format!("/__shell/ui/{commit}/assets/main.js"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "no release file for a device not admitted"
    );
}

#[tokio::test]
async fn release_file_paths_cannot_leave_the_release() {
    let f = fixture(true).await;
    let commit = stable_commit(&f);
    for uri in [
        format!("/__shell/ui/{commit}/assets/../../HEAD"),
        "/__shell/ui/..%2F..%2Fapi/main.js".to_string(),
        format!("/__shell/ui/{commit}/assets//main.js"),
        "/__shell/ui/r1/assets/main.js".to_string(),
    ] {
        let (status, _, _) = get(&f.ui.routes(), &uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
}

#[tokio::test]
async fn a_move_of_the_followed_channel_reloads_the_page_once_it_answers_ready() {
    let mut f = fixture(true).await;
    f.ui.index(embedded()).await;
    f.ui.spawn();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        f.shell_rx.try_recv().is_err(),
        "the page already runs stable"
    );

    f.store.publish("dev", &build(&f.root, "r2"), "s").unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        f.shell_rx.try_recv().is_err(),
        "a publish to dev leaves a stable device alone"
    );

    f.store.promote("dev", "stable").unwrap();
    assert_eq!(
        next_command(&mut f.shell_rx).await,
        ShellCommand::Eval(prepare_script(1))
    );
    answer(&f.ui, r#"{"request":1,"status":"ready"}"#).await;
    assert_eq!(next_command(&mut f.shell_rx).await, ShellCommand::Reload);
}

#[tokio::test]
async fn switching_channel_reloads_the_page_onto_that_channel() {
    let mut f = fixture(true).await;
    f.store.publish("dev", &build(&f.root, "r2"), "s").unwrap();
    f.ui.index(embedded()).await;

    *f.host.channel.lock().unwrap() = "dev".into();
    f.ui.recheck().await;

    assert_eq!(
        next_command(&mut f.shell_rx).await,
        ShellCommand::Eval(prepare_script(1))
    );
    let (_, served) = f.ui.index(embedded()).await;
    assert_eq!(
        (served.build_id.as_str(), served.channel.as_deref()),
        ("r2", Some("dev"))
    );
}

#[tokio::test]
async fn the_status_reports_the_channel_and_where_it_points() {
    let f = fixture(true).await;
    f.ui.index(embedded()).await;

    let (_, _, body) = get(&f.ui.routes(), "/__shell/status").await;

    let status: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status["enabled"], true);
    assert_eq!(status["channel"], "stable");
    assert_eq!(status["current"]["build_id"], "r1");
    assert_eq!(status["served"]["build_id"], "r1");
    assert_eq!(status["reload"]["pending"], false);
}

#[tokio::test]
async fn boot_marks_are_measured_from_process_start() {
    let f = fixture(false).await;
    let start = kal_iroh_shell::process_start_epoch_ms().expect("/proc is available on Linux");
    let mark = serde_json::json!({"phase": "data", "epoch_ms": start + 1234});

    let resp =
        f.ui.routes()
            .oneshot(
                Request::post("/__shell/boot-mark")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(mark.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
    let (_, _, marks) = get(&f.ui.routes(), "/__shell/boot-mark").await;

    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let marks: serde_json::Value = serde_json::from_str(&marks).unwrap();
    let measured = marks["marks"][0]["since_process_start_ms"]
        .as_i64()
        .unwrap();
    // `/proc` start time has 10 ms resolution.
    assert!((1224..=1244).contains(&measured), "{measured}");
}

#[test]
fn a_release_without_relative_assets_is_refused() {
    let commit = "47675ee0819e4c942e9861dc7b32b832e877b47d";
    assert!(
        kal_iroh_shell::rewrite_release_index(r#"<script src="/assets/main.js">"#, commit).is_err()
    );
}
