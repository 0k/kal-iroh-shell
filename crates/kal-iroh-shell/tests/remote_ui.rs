//! The client shell against a real release server behind a fake upstream.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use futures::future::BoxFuture;
use http_body_util::BodyExt;
use kal_iroh_shell::{EmbeddedIndex, Host, RemoteUi, RemoteUiConfig, ShellCommand, prepare_script};
use kal_iroh_shell_server::ReleaseServer;
use tower::ServiceExt;

const RELEASES_BASE: &str = "/serve/my-app";
const RELEASE_INDEX: &str =
    r#"<script type="module" src="./assets/main.js"></script><div id="app"></div>"#;

/// The app side: upstream URL, setting, admission verdict.
struct TestHost {
    upstream: tokio::sync::watch::Receiver<String>,
    enabled: AtomicBool,
    admitted: AtomicBool,
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

/// Write release `id` (an index with a relative asset) and make it
/// current, as the publisher does.
fn publish(dir: &Path, id: &str) {
    let root = dir.join(id);
    std::fs::create_dir_all(root.join("assets")).unwrap();
    std::fs::write(root.join("index.html"), RELEASE_INDEX).unwrap();
    std::fs::write(root.join("assets/main.js"), format!("// {id}")).unwrap();
    std::fs::write(
        root.join("ui-manifest.json"),
        format!(r#"{{"build_id":"{id}","files":[]}}"#),
    )
    .unwrap();
    let tmp = dir.join("current.json.tmp");
    std::fs::write(&tmp, format!(r#"{{"build_id":"{id}","published_at":"t"}}"#)).unwrap();
    std::fs::rename(tmp, dir.join("current.json")).unwrap();
}

struct Fixture {
    _dir: tempfile::TempDir,
    dir: std::path::PathBuf,
    /// Keeps the release server's directory watch alive.
    _watcher: Box<dyn std::any::Any + Send>,
    /// Keeps the upstream URL channel open.
    _upstream_tx: tokio::sync::watch::Sender<String>,
    host: Arc<TestHost>,
    ui: Arc<RemoteUi>,
    shell_rx: tokio::sync::mpsc::UnboundedReceiver<ShellCommand>,
}

/// A release server (publishing `r1`) behind an upstream at
/// [`RELEASES_BASE`], and a shell pointed at it.
async fn fixture(published: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    if published {
        publish(&path, "r1");
    }
    let releases = ReleaseServer::new(path.clone());
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
        _dir: dir,
        dir: path,
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

#[tokio::test]
async fn the_page_gets_the_published_release_pointing_at_its_own_files() {
    let f = fixture(true).await;

    let (html, served) = f.ui.index(embedded()).await;
    let (status, headers, js) = get(&f.ui.routes(), "/__shell/ui/r1/assets/main.js").await;

    assert_eq!(served.source, "remote");
    assert_eq!(served.build_id, "r1");
    assert!(
        html.contains(r#"src="/__shell/ui/r1/assets/main.js""#),
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
        (&nothing_published, "GET /current.json: HTTP 404"),
    ] {
        let (html, served) = f.ui.index(embedded()).await;
        assert_eq!(served.source, "embedded");
        assert_eq!(served.fallback_reason.as_deref(), Some(reason));
        assert!(html.ends_with("<!-- embedded -->"));
    }
    let (status, _, _) = get(&not_admitted.ui.routes(), "/__shell/ui/r1/assets/main.js").await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "no release file for a device not admitted"
    );
}

#[tokio::test]
async fn release_file_paths_cannot_leave_the_release() {
    let f = fixture(true).await;
    for uri in [
        "/__shell/ui/r1/assets/../../current.json",
        "/__shell/ui/..%2F..%2Fapi/main.js",
        "/__shell/ui/r1/assets//main.js",
    ] {
        let (status, _, _) = get(&f.ui.routes(), uri).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
    }
}

#[tokio::test]
async fn a_publish_reloads_the_page_once_it_answers_ready() {
    let mut f = fixture(true).await;
    f.ui.index(embedded()).await;
    f.ui.spawn();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        f.shell_rx.try_recv().is_err(),
        "the page already runs the current release"
    );

    publish(&f.dir, "r2");

    assert_eq!(
        next_command(&mut f.shell_rx).await,
        ShellCommand::Eval(prepare_script(1))
    );
    let ack =
        f.ui.routes()
            .oneshot(
                Request::post("/__shell/reload-ack")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"request":1,"status":"ready"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(ack.status(), StatusCode::NO_CONTENT);
    assert_eq!(next_command(&mut f.shell_rx).await, ShellCommand::Reload);
}

#[tokio::test]
async fn the_status_reports_the_current_release_and_a_pending_reload() {
    let f = fixture(true).await;
    f.ui.index(embedded()).await;

    let (_, _, body) = get(&f.ui.routes(), "/__shell/status").await;

    let status: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status["enabled"], true);
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
    assert!(
        kal_iroh_shell::rewrite_release_index(r#"<script src="/assets/main.js">"#, "r1").is_err()
    );
}
