//! Release server: serves an app's releases from its release store and
//! pushes "channel moved" events.
//!
//! The release store ([`store`]) is a bare git repository: a release is a
//! commit of a built interface, a channel (`dev`, `alpha`, `beta`,
//! `stable`, …) is a branch.  Below whatever prefix a reverse proxy gives
//! it (for example a vigil-watch serve mount, reached over Iroh), the
//! server answers:
//!
//! - `GET /channels/<channel>`: where the channel points ([`ChannelHead`]);
//! - `GET /<commit>/<path>`: a file of a release (immutable, cacheable
//!   for good);
//! - `GET /api/control` + `GET /sse/control?sub=…`: the CONTROL CHANNEL, a
//!   `kal-sse` snapshot + cursor channel.  The snapshot lists every
//!   channel's head; each event says that one channel moved, to which
//!   release, and which files differ.  It is separate from any data
//!   stream of the app.
//!
//! A channel move (publish, promotion, rollback) is noticed through a
//! watch on the store's refs; the server only relays.

pub mod store;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use kal_iroh_shell_protocol::{
    CHANNELS_PREFIX, CONTROL_SNAPSHOT_PATH, CONTROL_STREAM_PATH, ChannelHead, ControlSnapshot,
    ReleaseEvent, SNAPSHOT_INDEX_HEADER, SUB_ID_HEADER, valid_channel, valid_commit,
    valid_release_path,
};
use kal_sse::core::EventLog;
use serde::Deserialize;

pub use store::Store;

/// A subscription not attached within this delay is forgotten.
const SUB_TTL: Duration = Duration::from_secs(15 * 60);
/// Hard cap on retained control events (one per channel move).
const MAX_EVENTS: usize = 256;
const RELEASE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
/// Safety net: re-read the refs this often even without a watch event.
pub const REFRESH_FALLBACK: Duration = Duration::from_secs(30);

/// Shared state of the server.
#[derive(Clone)]
pub struct ReleaseServer {
    store: Arc<Store>,
    heads: Arc<Mutex<BTreeMap<String, ChannelHead>>>,
    log: Arc<Mutex<EventLog<ReleaseEvent>>>,
    /// Refreshes triggered by the refs watch (diagnostics, tests).
    watch_refreshes: Arc<AtomicU64>,
}

/// Whether `event` writes a ref: a channel moved.
///
/// Only writes count — git's rename of `refs/heads/<x>.lock` onto
/// `refs/heads/<x>`, a creation, a direct write, `packed-refs` being
/// rewritten.  Opens and reads do NOT: [`ReleaseServer::refresh`] itself
/// reads the refs, so reacting to reads would re-trigger it forever.
pub fn is_ref_write(event: &notify::Event) -> bool {
    use notify::EventKind;
    use notify::event::{AccessKind, AccessMode, ModifyKind};
    matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any)
            | EventKind::Access(AccessKind::Close(AccessMode::Write))
    ) && event
        .paths
        .iter()
        .any(|p| !p.extension().is_some_and(|e| e == "lock"))
}

impl ReleaseServer {
    pub fn new(store: Store) -> Self {
        let heads = store
            .channels()
            .unwrap_or_default()
            .into_iter()
            .map(|h| (h.channel.clone(), h))
            .collect();
        Self {
            store: Arc::new(store),
            heads: Arc::new(Mutex::new(heads)),
            log: Arc::new(Mutex::new(EventLog::new(SUB_TTL, MAX_EVENTS, 64))),
            watch_refreshes: Arc::new(AtomicU64::new(0)),
        }
    }

    /// How many refreshes the refs watch has triggered.
    pub fn watch_refreshes(&self) -> u64 {
        self.watch_refreshes.load(Ordering::Relaxed)
    }

    /// Re-read the channels; append one event per channel that moved.
    pub fn refresh(&self) -> Vec<ReleaseEvent> {
        let now = match self.store.channels() {
            Ok(h) => h,
            Err(e) => {
                log::warn!("release-server: cannot read channels: {e}");
                return vec![];
            }
        };
        let mut heads = self.heads.lock().expect("heads poisoned");
        let mut events = Vec::new();
        for head in now {
            let previous = heads.get(&head.channel).map(|h| h.commit.clone());
            if previous.as_deref() == Some(head.commit.as_str()) {
                continue;
            }
            let diff = self
                .store
                .diff(previous.as_deref(), &head.commit)
                .unwrap_or_default();
            log::info!(
                "release-server: channel {} → release {} ({} changed, {} removed)",
                head.channel,
                head.build_id,
                diff.changed.len(),
                diff.removed.len()
            );
            events.push(ReleaseEvent {
                channel: head.channel.clone(),
                commit: head.commit.clone(),
                build_id: head.build_id.clone(),
                published_at: head.published_at.clone(),
                previous,
                changed: diff.changed,
                removed: diff.removed,
            });
            heads.insert(head.channel.clone(), head);
        }
        if !events.is_empty() {
            let mut log = self.log.lock().expect("event log poisoned");
            log.compact(Instant::now());
            for e in &events {
                log.append(e.clone());
            }
        }
        events
    }

    /// Watch the store's refs and refresh on every write.  The returned
    /// watcher must be kept alive.
    pub fn watch(&self) -> notify::Result<notify::RecommendedWatcher> {
        use notify::{RecursiveMode, Watcher};
        let server = self.clone();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
                Ok(event) if is_ref_write(&event) => {
                    server.watch_refreshes.fetch_add(1, Ordering::Relaxed);
                    server.refresh();
                }
                Ok(_) => {}
                Err(e) => log::warn!("release-server: watch error: {e}"),
            })?;
        let heads = self.store.dir().join("refs").join("heads");
        std::fs::create_dir_all(&heads).map_err(notify::Error::io)?;
        watcher.watch(&heads, RecursiveMode::NonRecursive)?;
        // `packed-refs` lives at the root (after `git gc`).
        watcher.watch(self.store.dir(), RecursiveMode::NonRecursive)?;
        Ok(watcher)
    }

    pub fn router(&self) -> axum::Router {
        axum::Router::new()
            .route(CONTROL_SNAPSHOT_PATH, get(control_snapshot))
            .route(CONTROL_STREAM_PATH, get(control_stream))
            .route(&format!("{CHANNELS_PREFIX}/{{channel}}"), get(channel_head))
            .route("/{commit}/{*path}", get(release_file))
            .with_state(self.clone())
    }
}

#[derive(Default, Deserialize)]
struct ControlParams {
    sub: Option<String>,
    last_event_id: Option<String>,
}

/// `GET /api/control` — every channel's head; registers a subscription.
async fn control_snapshot(
    State(server): State<ReleaseServer>,
    Query(params): Query<ControlParams>,
) -> Response {
    // Register and read under the same order the appends use (heads
    // then log), so no move falls between snapshot and cursor.
    let heads = server.heads.lock().expect("heads poisoned");
    let (sub, index) = server
        .log
        .lock()
        .expect("event log poisoned")
        .register(params.sub);
    let body = ControlSnapshot {
        channels: heads.values().cloned().collect(),
    };
    drop(heads);
    let mut headers = HeaderMap::new();
    headers.insert(
        SUB_ID_HEADER,
        HeaderValue::from_str(&sub).expect("uuid header"),
    );
    headers.insert(SNAPSHOT_INDEX_HEADER, HeaderValue::from(index));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    (headers, Json(body)).into_response()
}

/// `GET /sse/control?sub=…` — channel moves, resumable.
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

/// `GET /channels/{channel}` — where one channel points, read from the
/// store (not from the cached heads, which lag a move by the watch delay).
async fn channel_head(
    State(server): State<ReleaseServer>,
    UrlPath(channel): UrlPath<String>,
) -> Response {
    if !valid_channel(&channel) {
        return (StatusCode::BAD_REQUEST, "invalid channel").into_response();
    }
    let store = server.store.clone();
    let heads = tokio::task::spawn_blocking(move || store.channels()).await;
    match heads {
        Ok(Ok(heads)) => match heads.into_iter().find(|h| h.channel == channel) {
            Some(head) => ([(header::CACHE_CONTROL, "no-store")], Json(head)).into_response(),
            None => (StatusCode::NOT_FOUND, format!("no channel {channel:?}")).into_response(),
        },
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `GET /{commit}/{*path}` — one file of a release.
async fn release_file(
    State(server): State<ReleaseServer>,
    UrlPath((commit, path)): UrlPath<(String, String)>,
) -> Response {
    if !valid_commit(&commit) || !valid_release_path(&path) {
        return (StatusCode::BAD_REQUEST, "invalid release path").into_response();
    }
    let store = server.store.clone();
    let read = tokio::task::spawn_blocking({
        let path = path.clone();
        move || store.read(&commit, &path)
    })
    .await;
    match read {
        Ok(Ok(bytes)) => {
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
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use kal_iroh_shell_protocol::MANIFEST_FILE;
    use std::path::{Path, PathBuf};
    use tower::ServiceExt;

    fn build(root: &Path, build_id: &str, index: &str) -> PathBuf {
        let dist = root.join(build_id);
        std::fs::create_dir_all(dist.join("assets")).unwrap();
        std::fs::write(dist.join("index.html"), index).unwrap();
        std::fs::write(dist.join("assets/a.js"), "a").unwrap();
        std::fs::write(
            dist.join(MANIFEST_FILE),
            serde_json::json!({"build_id": build_id, "files": []}).to_string(),
        )
        .unwrap();
        dist
    }

    fn fixture() -> (tempfile::TempDir, Store, ReleaseServer) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_or_init(tmp.path().join("releases.git")).unwrap();
        store
            .publish("dev", &build(tmp.path(), "b1", "1"), "s")
            .unwrap();
        store.promote("dev", "stable").unwrap();
        let server = ReleaseServer::new(store.clone());
        (tmp, store, server)
    }

    async fn get(router: &axum::Router, uri: &str) -> (StatusCode, HeaderMap, Vec<u8>) {
        let resp = router
            .clone()
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        (
            status,
            headers,
            resp.into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
    }

    #[tokio::test]
    async fn serves_each_channel_head_and_its_release_files() {
        let (_tmp, _store, server) = fixture();
        let router = server.router();

        let (status, _, body) = get(&router, "/channels/stable").await;
        let head: ChannelHead = serde_json::from_slice(&body).unwrap();
        let (fstatus, fheaders, file) = get(&router, &format!("/{}/index.html", head.commit)).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            (head.channel.as_str(), head.build_id.as_str()),
            ("stable", "b1")
        );
        assert_eq!(
            (fstatus, file.as_slice()),
            (StatusCode::OK, b"1".as_slice())
        );
        assert_eq!(fheaders[header::CACHE_CONTROL], RELEASE_CACHE_CONTROL);
        assert_eq!(
            get(&router, "/channels/beta").await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn release_files_never_leave_the_release() {
        let (_tmp, _store, server) = fixture();
        let router = server.router();
        let commit = server.heads.lock().unwrap()["dev"].commit.clone();

        for uri in [
            format!("/{commit}/../HEAD"),
            format!("/{commit}/assets//a.js"),
            "/HEAD/index.html".to_string(),
            format!("/{}/index.html", &commit[..7]),
        ] {
            assert_ne!(get(&router, &uri).await.0, StatusCode::OK, "{uri}");
        }
    }

    #[test]
    fn a_publish_announces_its_channel_and_only_the_changed_files() {
        let (tmp, store, server) = fixture();

        store
            .publish("dev", &build(tmp.path(), "b2", "2"), "s")
            .unwrap();
        let events = server.refresh();

        assert_eq!(events.len(), 1, "stable did not move");
        let e = &events[0];
        assert_eq!((e.channel.as_str(), e.build_id.as_str()), ("dev", "b2"));
        assert_eq!(
            e.changed,
            vec!["index.html".to_string(), MANIFEST_FILE.to_string()]
        );
        assert!(server.refresh().is_empty(), "nothing moved since");
    }

    #[test]
    fn a_promotion_or_a_rollback_is_announced_like_a_publish() {
        let (tmp, store, server) = fixture();
        store
            .publish("dev", &build(tmp.path(), "b2", "2"), "s")
            .unwrap();
        server.refresh();

        store.promote("dev", "stable").unwrap();
        let forward = server.refresh();
        store.promote("stable~1", "stable").unwrap();
        let back = server.refresh();

        assert_eq!(
            (forward[0].channel.as_str(), forward[0].build_id.as_str()),
            ("stable", "b2")
        );
        assert_eq!(
            (back[0].channel.as_str(), back[0].build_id.as_str()),
            ("stable", "b1")
        );
    }

    #[tokio::test]
    async fn the_watch_pushes_a_move_and_then_goes_quiet() {
        let (_tmp, store, server) = fixture();
        let _watcher = server.watch().unwrap();
        let router = server.router();
        let (_, headers, body) = get(&router, "/api/control").await;
        let snapshot: ControlSnapshot = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            snapshot
                .channels
                .iter()
                .map(|h| h.channel.as_str())
                .collect::<Vec<_>>(),
            ["dev", "stable"]
        );
        let sub = headers[SUB_ID_HEADER].to_str().unwrap().to_string();
        let stream = router
            .oneshot(
                Request::get(format!("/sse/control?sub={sub}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        store.promote("dev", "beta").unwrap();

        let mut body = stream.into_body();
        let mut text = String::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
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
        assert!(text.contains(r#""channel":"beta""#), "{text}");
        let after_move = server.watch_refreshes();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            (1..=6).contains(&after_move),
            "{after_move} refreshes for one move"
        );
        assert_eq!(
            server.watch_refreshes(),
            after_move,
            "the watch must go quiet"
        );
    }

    #[test]
    fn only_ref_writes_trigger_a_refresh() {
        use notify::event::{AccessKind, AccessMode, ModifyKind, RenameMode};
        use notify::{Event, EventKind};
        let on =
            |kind, name: &str| Event::new(kind).add_path(PathBuf::from("/s/refs/heads").join(name));

        assert!(is_ref_write(&on(
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            "beta"
        )));
        assert!(!is_ref_write(&on(
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            "beta"
        )));
        assert!(!is_ref_write(&on(
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            "beta"
        )));
        assert!(!is_ref_write(&on(
            EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            "beta.lock"
        )));
    }
}
