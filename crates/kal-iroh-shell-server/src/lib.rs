//! Release server: serves the published interface releases of one app
//! and pushes "new release" events.
//!
//! It owns a publish directory written by the page kit's Vite plugin
//! (npm `@0k/kal-iroh-shell`, `vite` entry):
//!
//! ```text
//! <dir>/current.json            {"build_id": …, "published_at": …}
//! <dir>/<build-id>/ui-manifest.json, index.html, assets/…
//! ```
//!
//! and serves, below whatever prefix a reverse proxy gives it (for
//! example a vigil-watch serve mount, which carries it over Iroh):
//!
//! - `GET /current.json` and `GET /<build-id>/<path>`: the release files
//!   (release files are immutable, so cacheable for good);
//! - `GET /api/control` + `GET /sse/control?sub=…`: the CONTROL CHANNEL,
//!   a `kal-sse` snapshot + cursor channel.  The snapshot is the current
//!   release; each event is a newly published release with the paths
//!   that changed since the previous one.  It is separate from any data
//!   stream of the app: it only says which release is current.
//!
//! A publish is noticed through the directory watch on `current.json`,
//! which the publisher replaces by a rename: the server only relays.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use kal_iroh_shell_protocol::{
    CONTROL_SNAPSHOT_PATH, CONTROL_STREAM_PATH, ControlSnapshot, MANIFEST_FILE, Manifest,
    POINTER_FILE, Pointer, ReleaseEvent, SNAPSHOT_INDEX_HEADER, SUB_ID_HEADER, valid_build_id,
    valid_release_path,
};
use kal_sse::core::EventLog;
use serde::Deserialize;

/// A subscription not attached within this delay is forgotten.
const SUB_TTL: Duration = Duration::from_secs(15 * 60);
/// Hard cap on retained control events (one per publish).
const MAX_EVENTS: usize = 256;
const RELEASE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

fn read_pointer(dir: &Path) -> Option<Pointer> {
    let bytes = std::fs::read(dir.join(POINTER_FILE)).ok()?;
    match serde_json::from_slice::<Pointer>(&bytes) {
        Ok(p) if valid_build_id(&p.build_id) => Some(p),
        Ok(p) => {
            log::warn!("release-server: invalid build id {:?}", p.build_id);
            None
        }
        Err(e) => {
            log::warn!("release-server: unreadable {POINTER_FILE}: {e}");
            None
        }
    }
}

fn read_manifest(dir: &Path, id: &str) -> Option<BTreeMap<String, String>> {
    let bytes = std::fs::read(dir.join(id).join(MANIFEST_FILE)).ok()?;
    let manifest: Manifest = serde_json::from_slice(&bytes).ok()?;
    Some(
        manifest
            .files
            .into_iter()
            .map(|f| (f.path, f.sha256))
            .collect(),
    )
}

/// The event announcing `next`, diffed against `previous`'s manifest.
pub fn release_event(dir: &Path, previous: Option<&Pointer>, next: &Pointer) -> ReleaseEvent {
    let new = read_manifest(dir, &next.build_id).unwrap_or_default();
    let old = previous.and_then(|p| read_manifest(dir, &p.build_id));
    let (changed, removed) = match old {
        Some(old) => (
            new.iter()
                .filter(|(path, hash)| old.get(*path) != Some(hash))
                .map(|(path, _)| path.clone())
                .collect(),
            old.keys()
                .filter(|p| !new.contains_key(*p))
                .cloned()
                .collect(),
        ),
        None => (new.keys().cloned().collect(), Vec::new()),
    };
    ReleaseEvent {
        build_id: next.build_id.clone(),
        published_at: next.published_at.clone(),
        previous: previous.map(|p| p.build_id.clone()),
        changed,
        removed,
    }
}

/// Shared state of the server.
#[derive(Clone)]
pub struct ReleaseServer {
    dir: Arc<PathBuf>,
    current: Arc<Mutex<Option<Pointer>>>,
    log: Arc<Mutex<EventLog<ReleaseEvent>>>,
    /// Refreshes triggered by the directory watch (diagnostics, tests).
    watch_refreshes: Arc<AtomicU64>,
}

/// Whether `event` is a write to the pointer file: a publish.
///
/// Only writes count — the publisher's rename onto `current.json`, its
/// creation, a direct write.  Opens, reads and metadata changes do NOT:
/// [`ReleaseServer::refresh`] itself opens `current.json`, so reacting
/// to opens re-triggers the refresh forever (one core spinning after the
/// first publish).
pub fn is_publish_event(event: &notify::Event) -> bool {
    use notify::EventKind;
    use notify::event::{AccessKind, AccessMode, ModifyKind};
    let writes = matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any)
            | EventKind::Access(AccessKind::Close(AccessMode::Write))
    );
    writes
        && event
            .paths
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == POINTER_FILE))
}

impl ReleaseServer {
    pub fn new(dir: PathBuf) -> Self {
        let current = read_pointer(&dir);
        Self {
            dir: Arc::new(dir),
            current: Arc::new(Mutex::new(current)),
            log: Arc::new(Mutex::new(EventLog::new(SUB_TTL, MAX_EVENTS, 64))),
            watch_refreshes: Arc::new(AtomicU64::new(0)),
        }
    }

    /// How many refreshes the directory watch has triggered.
    pub fn watch_refreshes(&self) -> u64 {
        self.watch_refreshes.load(Ordering::Relaxed)
    }

    /// Re-read `current.json`; append an event when the release changed.
    pub fn refresh(&self) -> Option<ReleaseEvent> {
        let next = read_pointer(&self.dir)?;
        let mut current = self.current.lock().expect("current poisoned");
        if current.as_ref().map(|c| &c.build_id) == Some(&next.build_id) {
            return None;
        }
        let event = release_event(&self.dir, current.as_ref(), &next);
        log::info!(
            "release-server: release {} is current ({} changed, {} removed)",
            event.build_id,
            event.changed.len(),
            event.removed.len()
        );
        *current = Some(next);
        let mut log = self.log.lock().expect("event log poisoned");
        log.compact(Instant::now());
        log.append(event.clone());
        Some(event)
    }

    /// Watch the publish directory and refresh on every change of
    /// `current.json`.  The returned watcher must be kept alive.
    pub fn watch(&self) -> notify::Result<notify::RecommendedWatcher> {
        use notify::{RecursiveMode, Watcher};
        let server = self.clone();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
                Ok(event) if is_publish_event(&event) => {
                    server.watch_refreshes.fetch_add(1, Ordering::Relaxed);
                    server.refresh();
                }
                Ok(_) => {}
                Err(e) => log::warn!("release-server: watch error: {e}"),
            })?;
        watcher.watch(&self.dir, RecursiveMode::NonRecursive)?;
        Ok(watcher)
    }

    pub fn router(&self) -> axum::Router {
        axum::Router::new()
            .route(CONTROL_SNAPSHOT_PATH, get(control_snapshot))
            .route(CONTROL_STREAM_PATH, get(control_stream))
            .route("/current.json", get(current_json))
            .route("/{build_id}/{*path}", get(release_file))
            .with_state(self.clone())
    }
}

#[derive(Default, Deserialize)]
struct ControlParams {
    sub: Option<String>,
    last_event_id: Option<String>,
}

/// `GET /api/control` — the current release; registers a subscription.
async fn control_snapshot(
    State(server): State<ReleaseServer>,
    Query(params): Query<ControlParams>,
) -> Response {
    // Register and read under the same order the appends use (current
    // then log), so no publish falls between snapshot and cursor.
    let current = server.current.lock().expect("current poisoned");
    let (sub, index) = server
        .log
        .lock()
        .expect("event log poisoned")
        .register(params.sub);
    let body = ControlSnapshot {
        current: current.clone(),
    };
    drop(current);
    let mut headers = HeaderMap::new();
    headers.insert(
        SUB_ID_HEADER,
        HeaderValue::from_str(&sub).expect("uuid header"),
    );
    headers.insert(SNAPSHOT_INDEX_HEADER, HeaderValue::from(index));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    (headers, Json(body)).into_response()
}

/// `GET /sse/control?sub=…` — new releases, resumable.
async fn control_stream(
    State(server): State<ReleaseServer>,
    headers: HeaderMap,
    Query(params): Query<ControlParams>,
) -> Response {
    let Some(sub) = params.sub else {
        return (StatusCode::BAD_REQUEST, "sub is required").into_response();
    };
    let resume = match kal_sse::axum::last_event_id(&headers, params.last_event_id.as_deref()) {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    match kal_sse::axum::sse_response_from(
        server.log.clone(),
        sub,
        resume,
        |e: &ReleaseEvent| serde_json::to_string(e).expect("serializable event"),
        "resync via GET /api/control",
    ) {
        Ok(sse) => sse.into_response(),
        Err(e) => e.into_response(),
    }
}

async fn current_json(State(server): State<ReleaseServer>) -> Response {
    match server.current.lock().expect("current poisoned").clone() {
        Some(p) => ([(header::CACHE_CONTROL, "no-store")], Json(p)).into_response(),
        None => (StatusCode::NOT_FOUND, "nothing published").into_response(),
    }
}

async fn release_file(
    State(server): State<ReleaseServer>,
    UrlPath((build_id, path)): UrlPath<(String, String)>,
) -> Response {
    if !valid_build_id(&build_id) || !valid_release_path(&path) {
        return (StatusCode::BAD_REQUEST, "invalid release path").into_response();
    }
    let file = server.dir.join(&build_id).join(&path);
    match tokio::fs::read(&file).await {
        Ok(bytes) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                [
                    (header::CONTENT_TYPE, mime.to_string()),
                    (header::CACHE_CONTROL, RELEASE_CACHE_CONTROL.to_string()),
                ],
                bytes,
            )
                .into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// Write release `id` with `files` (path → content) into `dir`.
    fn write_release(dir: &Path, id: &str, files: &[(&str, &str)]) {
        use sha2::{Digest, Sha256};
        let root = dir.join(id);
        let entries: Vec<_> = files
            .iter()
            .map(|(path, content)| {
                let full = root.join(path);
                std::fs::create_dir_all(full.parent().unwrap()).unwrap();
                std::fs::write(&full, content).unwrap();
                serde_json::json!({"path": path, "size": content.len(),
                    "sha256": format!("{:x}", Sha256::digest(content.as_bytes()))})
            })
            .collect();
        std::fs::write(
            root.join(MANIFEST_FILE),
            serde_json::json!({"build_id": id, "files": entries}).to_string(),
        )
        .unwrap();
    }

    fn make_current(dir: &Path, id: &str) {
        let tmp = dir.join("current.json.tmp");
        std::fs::write(
            &tmp,
            serde_json::json!({"build_id": id, "published_at": "2026-10-24T00:00:00Z"}).to_string(),
        )
        .unwrap();
        std::fs::rename(tmp, dir.join(POINTER_FILE)).unwrap();
    }

    #[test]
    fn a_publish_announces_only_the_files_that_changed() {
        let dir = tempfile::tempdir().unwrap();
        write_release(
            dir.path(),
            "r1",
            &[
                ("index.html", "a"),
                ("assets/x.js", "x"),
                ("assets/old.js", "o"),
            ],
        );
        write_release(
            dir.path(),
            "r2",
            &[
                ("index.html", "a"),
                ("assets/x.js", "x2"),
                ("assets/new.js", "n"),
            ],
        );
        make_current(dir.path(), "r1");
        let server = ReleaseServer::new(dir.path().to_path_buf());

        make_current(dir.path(), "r2");
        let event = server.refresh().expect("r2 is announced");

        assert_eq!(
            event,
            ReleaseEvent {
                build_id: "r2".into(),
                published_at: "2026-10-24T00:00:00Z".into(),
                previous: Some("r1".into()),
                changed: vec!["assets/new.js".into(), "assets/x.js".into()],
                removed: vec!["assets/old.js".into()],
            }
        );
        assert_eq!(
            server.refresh(),
            None,
            "an unchanged pointer announces nothing"
        );
    }

    #[tokio::test]
    async fn one_publish_triggers_a_bounded_number_of_refreshes() {
        // Regression: the watch reacted to ANY event on `current.json`,
        // including the open done by `refresh()` itself, so one publish
        // started an endless open → refresh → open loop (one core, forever).
        let dir = tempfile::tempdir().unwrap();
        write_release(dir.path(), "r1", &[("index.html", "a")]);
        write_release(dir.path(), "r2", &[("index.html", "b")]);
        make_current(dir.path(), "r1");
        let server = ReleaseServer::new(dir.path().to_path_buf());
        let _watcher = server.watch().unwrap();

        make_current(dir.path(), "r2");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let after_publish = server.watch_refreshes();
        tokio::time::sleep(Duration::from_millis(500)).await;

        assert_eq!(
            server.current.lock().unwrap().as_ref().unwrap().build_id,
            "r2"
        );
        assert!(
            (1..=3).contains(&after_publish),
            "{after_publish} refreshes for one publish"
        );
        assert_eq!(
            server.watch_refreshes(),
            after_publish,
            "the watch must go quiet"
        );
    }

    #[test]
    fn only_writes_to_the_pointer_are_publish_events() {
        use notify::event::{
            AccessKind, AccessMode, CreateKind, DataChange, MetadataKind, ModifyKind, RenameMode,
        };
        use notify::{Event, EventKind};
        let on = |kind, name: &str| Event::new(kind).add_path(PathBuf::from("/pub").join(name));

        for kind in [
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
        ] {
            assert!(
                is_publish_event(&on(kind, POINTER_FILE)),
                "{kind:?} must count"
            );
        }
        for kind in [
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
        ] {
            assert!(
                !is_publish_event(&on(kind, POINTER_FILE)),
                "{kind:?} must be ignored"
            );
        }
        assert!(!is_publish_event(&on(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            "other.json"
        )));
    }

    #[tokio::test]
    async fn the_watch_pushes_a_publish_to_a_subscribed_stream() {
        let dir = tempfile::tempdir().unwrap();
        write_release(dir.path(), "r1", &[("index.html", "a")]);
        write_release(dir.path(), "r2", &[("index.html", "b")]);
        make_current(dir.path(), "r1");
        let server = ReleaseServer::new(dir.path().to_path_buf());
        let _watcher = server.watch().unwrap();
        let router = server.router();

        let snap = router
            .clone()
            .oneshot(Request::get("/api/control").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let sub = snap.headers()["x-sub-id"].to_str().unwrap().to_string();
        let body: serde_json::Value =
            serde_json::from_slice(&snap.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["current"]["build_id"], "r1");

        let stream = router
            .oneshot(
                Request::get(format!("/sse/control?sub={sub}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(stream.status(), StatusCode::OK);
        make_current(dir.path(), "r2");

        let mut body = stream.into_body();
        let mut text = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        // Skip comments (`: connected`, keep-alives) up to a full event.
        while !text.split("\n\n").any(|block| block.contains("data:")) {
            let frame = tokio::time::timeout_at(deadline, body.frame())
                .await
                .expect("event within 5 s")
                .unwrap()
                .unwrap();
            if let Ok(data) = frame.into_data() {
                text.push_str(&String::from_utf8_lossy(&data));
            }
        }
        assert!(text.contains(r#""build_id":"r2""#), "{text}");
        assert!(text.contains(r#""changed":["index.html"]"#), "{text}");
    }

    #[tokio::test]
    async fn release_files_never_leave_the_publish_directory() {
        let dir = tempfile::tempdir().unwrap();
        write_release(dir.path(), "r1", &[("index.html", "a")]);
        let router = ReleaseServer::new(dir.path().to_path_buf()).router();

        for uri in [
            "/r1/../current.json",
            "/r1/assets//x.js",
            "/..%2F/index.html",
        ] {
            let resp = router
                .clone()
                .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_ne!(resp.status(), StatusCode::OK, "{uri}");
        }
        let ok = router
            .oneshot(Request::get("/r1/index.html").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(ok.headers()[header::CACHE_CONTROL], RELEASE_CACHE_CONTROL);
    }
}
