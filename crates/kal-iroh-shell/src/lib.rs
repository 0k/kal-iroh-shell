//! Client shell: the app on a device shows the interface its server
//! publishes, and reloads it — state saved — when a new one is published.
//!
//! The app (a Tauri or GTK program) runs a local HTTP server that its
//! WebView loads from, and reaches its server through an upstream base
//! URL (typically an Iroh tunnel's local port).  This crate plugs into
//! that local server:
//!
//! - [`RemoteUi::index`] decides which `index.html` the page gets: the
//!   current release published by the server, rewritten to load its
//!   files from [`RELEASE_FILES_PREFIX`], or the copy built into the app
//!   when the remote one cannot be used (setting off, device not admitted,
//!   server unreachable, nothing published).  The page keeps the local
//!   origin either way, so storage and native bridges are unchanged.
//! - [`RemoteUi::routes`] adds the `/__shell/…` routes: release files,
//!   status, boot timing marks, and the page's reload answers.
//! - [`RemoteUi::spawn`] follows the server's control stream and runs the
//!   reload handshake (see [`reload`]), driving the app's view through
//!   [`ShellCommand`]s.
//!
//! The app supplies what only it knows through [`Host`]: where its
//! server is, how to authenticate, whether the device is admitted, and
//! whether the user turned the remote interface on.
//!
//! [`RELEASE_FILES_PREFIX`]: kal_iroh_shell_protocol::RELEASE_FILES_PREFIX

mod control;
pub mod reload;
#[cfg(feature = "tauri")]
pub mod tauri;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures::future::BoxFuture;
pub use kal_iroh_shell_protocol as protocol;
use kal_iroh_shell_protocol::{
    BOOT_MARK_PATH, PAGE_GLOBAL, POINTER_FILE, Pointer, RELEASE_FILES_PREFIX, RELOAD_ACK_PATH,
    ReloadAck, STATUS_PATH, UI_BUILD_ATTR, UI_SOURCE_ATTR, valid_build_id, valid_release_path,
};

/// Longest wait for the admission verdict before serving the embedded
/// interface.  On a cold start the WebView asks for the page before the
/// tunnel is up, so this covers dialing plus the admission handshake.
const REMOTE_BOOT_BUDGET: Duration = Duration::from_secs(10);
/// Timeout of each request to the server for a release file.
const REMOTE_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// A release directory never changes once published.
const RELEASE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
/// Boot marks kept for `GET /__shell/boot-mark`.
const MAX_BOOT_MARKS: usize = 32;
/// How often the handshake checks its deadlines.
const HANDSHAKE_TICK: Duration = Duration::from_millis(250);

/// What the app knows and the shell needs.
pub trait Host: Send + Sync + 'static {
    /// Base URL of the selected server (`""` when none), and its changes.
    fn upstream(&self) -> tokio::sync::watch::Receiver<String>;
    /// The HTTP client used to reach the server (reused across requests).
    fn client(&self) -> reqwest::Client;
    /// Add the app's credentials to a request to the server.
    fn authenticate(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        req
    }
    /// Whether the user wants the remote interface (a device setting).
    fn remote_ui_enabled(&self) -> bool;
    /// `Ok` once the server admitted this device, waiting at most `grace`
    /// for its verdict; `Err` explains why not.
    fn authorized(&self, grace: Duration) -> BoxFuture<'_, Result<(), String>>;
}

/// Where the app's server publishes its releases.
#[derive(Debug, Clone)]
pub struct RemoteUiConfig {
    /// Path, on the server, below which the release server is reachable
    /// (e.g. a vigil-watch serve mount: `/serve/my-app`).
    pub releases_base: String,
    /// Longest wait for an admission verdict when serving a release file.
    pub admission_grace: Duration,
}

/// The `index.html` built into the app, and its build id.
#[derive(Debug, Clone)]
pub struct EmbeddedIndex {
    pub html: String,
    pub build_id: String,
}

/// Which interface the page got.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ServedUi {
    /// `"remote"` or `"embedded"`.
    pub source: &'static str,
    pub build_id: String,
    /// Why the remote release was not used, for an embedded page.
    pub fallback_reason: Option<String>,
}

impl ServedUi {
    /// Attributes to add to the page's root element (leading space
    /// included); the page kit reads them.
    pub fn root_attributes(&self) -> String {
        let esc = |s: &str| s.replace('&', "&amp;").replace('"', "&quot;");
        format!(
            r#" {UI_SOURCE_ATTR}="{}" {UI_BUILD_ATTR}="{}""#,
            esc(self.source),
            esc(&self.build_id)
        )
    }
}

/// An order for the app's view (GTK WebView, Tauri window), to execute
/// on the view's own thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellCommand {
    /// Evaluate this script in the page.
    Eval(String),
    /// Reload the page natively (works even when its script is stuck).
    Reload,
}

/// Script asking the page to prepare reload `request`.  A page without
/// the page kit (older release, broken script) never answers and is
/// reloaded after [`reload::ANSWER_TIMEOUT`].
pub fn prepare_script(request: u64) -> String {
    format!(
        "window.{PAGE_GLOBAL} && window.{PAGE_GLOBAL}.prepareReload && \
         window.{PAGE_GLOBAL}.prepareReload({request});"
    )
}

/// Point a release's `index.html` at its files under
/// `/__shell/ui/<build-id>/`.
///
/// A release is built with relative asset URLs (`./assets/…`); one
/// without any would silently load the EMBEDDED assets under a remote
/// index, so it is refused.
pub fn rewrite_release_index(html: &str, build_id: &str) -> Result<String, String> {
    const RELATIVE: &str = "\"./assets/";
    if !html.contains(RELATIVE) {
        return Err("index.html has no relative ./assets/ reference".to_string());
    }
    Ok(html.replace(
        RELATIVE,
        &format!("\"{RELEASE_FILES_PREFIX}/{build_id}/assets/"),
    ))
}

/// The remote-interface part of the app's local server.
pub struct RemoteUi {
    config: RemoteUiConfig,
    host: Arc<dyn Host>,
    served: Mutex<Option<ServedUi>>,
    boot_marks: Mutex<Vec<serde_json::Value>>,
    shell_tx: Option<tokio::sync::mpsc::UnboundedSender<ShellCommand>>,
    handshake: Mutex<reload::Handshake>,
}

impl RemoteUi {
    /// `shell_tx` reaches the app's view; `None` disables the update
    /// handshake (the page is still served remotely).
    pub fn new(
        config: RemoteUiConfig,
        host: Arc<dyn Host>,
        shell_tx: Option<tokio::sync::mpsc::UnboundedSender<ShellCommand>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            host,
            served: Mutex::new(None),
            boot_marks: Mutex::new(Vec::new()),
            shell_tx,
            handshake: Mutex::new(reload::Handshake::default()),
        })
    }

    /// The `index.html` the page must get now, and which one it is.
    /// The app then injects its own attributes plus
    /// [`ServedUi::root_attributes`] into the root element.
    pub async fn index(&self, embedded: EmbeddedIndex) -> (String, ServedUi) {
        let (html, served) = match self.fetch_remote_index().await {
            Ok((html, build_id)) => (
                html,
                ServedUi {
                    source: "remote",
                    build_id,
                    fallback_reason: None,
                },
            ),
            Err(reason) => {
                log::debug!("kal-iroh-shell: serving the embedded interface: {reason}");
                (
                    embedded.html,
                    ServedUi {
                        source: "embedded",
                        build_id: embedded.build_id,
                        fallback_reason: Some(reason),
                    },
                )
            }
        };
        if let Ok(mut g) = self.served.lock() {
            *g = Some(served.clone());
        }
        (html, served)
    }

    /// The `/__shell/…` routes, to merge into the app's router.
    pub fn routes(self: &Arc<Self>) -> axum::Router {
        use axum::routing::{get, post};
        axum::Router::new()
            .route(STATUS_PATH, get(status_handler))
            .route(BOOT_MARK_PATH, get(get_boot_marks).post(post_boot_mark))
            .route(
                &format!("{RELEASE_FILES_PREFIX}/{{build_id}}/{{*path}}"),
                get(release_file_handler),
            )
            .route(RELOAD_ACK_PATH, post(ack_handler))
            .with_state(self.clone())
    }

    /// Follow the control stream and run the handshake clock.  Does
    /// nothing without a view to drive.  Needs a tokio runtime.
    pub fn spawn(self: &Arc<Self>) {
        if self.shell_tx.is_none() {
            return;
        }
        let ticker = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(HANDSHAKE_TICK);
            loop {
                tick.tick().await;
                ticker.apply(|h, now| h.on_tick(now));
            }
        });
        tokio::spawn(control::run(self.clone()));
    }

    /// Feed the handshake and execute what it asks for.
    fn apply(&self, step: impl FnOnce(&mut reload::Handshake, Instant) -> Vec<reload::Action>) {
        let actions = {
            let Ok(mut h) = self.handshake.lock() else {
                return;
            };
            step(&mut h, Instant::now())
        };
        for action in actions {
            let cmd = match action {
                reload::Action::Prepare { request } => {
                    log::info!("kal-iroh-shell: asking the page to prepare reload {request}");
                    ShellCommand::Eval(prepare_script(request))
                }
                reload::Action::Reload => {
                    log::info!("kal-iroh-shell: page ready, reloading");
                    ShellCommand::Reload
                }
                reload::Action::ForceReload { why } => {
                    log::warn!("kal-iroh-shell: forcing the reload: {why}");
                    ShellCommand::Reload
                }
            };
            match &self.shell_tx {
                Some(tx) => {
                    if tx.send(cmd).is_err() {
                        log::warn!("kal-iroh-shell: the view is gone");
                    }
                }
                None => log::debug!("kal-iroh-shell: no view to drive"),
            }
        }
    }

    /// Release `build_id` is current: start the handshake when the page
    /// does not run it.  A page that fell back to the embedded interface
    /// is reloaded too, so it gets the remote release once reachable.
    fn consider_release(&self, build_id: &str) {
        let served = self.served.lock().ok().and_then(|g| g.clone());
        let stale = match &served {
            Some(s) if s.source == "remote" => s.build_id != build_id,
            Some(_) => true,
            None => false,
        };
        if stale {
            self.apply(|h, now| h.on_new_release(now));
        }
    }

    /// `GET <server><releases_base><path>`; anything but `200` is an error.
    async fn server_get(&self, path: &str) -> Result<reqwest::Response, String> {
        let base = self.host.upstream().borrow().clone();
        if base.is_empty() {
            return Err("no server selected".to_string());
        }
        let req = self
            .host
            .client()
            .get(format!("{base}{}{path}", self.config.releases_base))
            .timeout(REMOTE_FETCH_TIMEOUT);
        let resp = self
            .host
            .authenticate(req)
            .send()
            .await
            .map_err(|e| format!("GET {path}: {e}"))?;
        if resp.status() != reqwest::StatusCode::OK {
            return Err(format!("GET {path}: HTTP {}", resp.status().as_u16()));
        }
        Ok(resp)
    }

    async fn fetch_pointer(&self) -> Result<Pointer, String> {
        let pointer: Pointer = self
            .server_get(&format!("/{POINTER_FILE}"))
            .await?
            .json()
            .await
            .map_err(|e| format!("{POINTER_FILE}: {e}"))?;
        if !valid_build_id(&pointer.build_id) {
            return Err(format!(
                "{POINTER_FILE}: invalid build id {:?}",
                pointer.build_id
            ));
        }
        Ok(pointer)
    }

    async fn fetch_remote_index(&self) -> Result<(String, String), String> {
        if !self.host.remote_ui_enabled() {
            return Err("remote interface setting is off".to_string());
        }
        self.host.authorized(REMOTE_BOOT_BUDGET).await?;
        let id = self.fetch_pointer().await?.build_id;
        let html = self
            .server_get(&format!("/{id}/index.html"))
            .await?
            .text()
            .await
            .map_err(|e| format!("index.html: {e}"))?;
        Ok((rewrite_release_index(&html, &id)?, id))
    }
}

type Shared = State<Arc<RemoteUi>>;

/// `GET /__shell/ui/{build_id}/{*path}` — one file of a published release.
async fn release_file_handler(
    State(ui): Shared,
    Path((build_id, path)): Path<(String, String)>,
) -> Response {
    if !valid_build_id(&build_id) || !valid_release_path(&path) {
        return (StatusCode::BAD_REQUEST, "invalid release path").into_response();
    }
    if let Err(reason) = ui.host.authorized(ui.config.admission_grace).await {
        return (StatusCode::SERVICE_UNAVAILABLE, reason).into_response();
    }
    let resp = match ui.server_get(&format!("/{build_id}/{path}")).await {
        Ok(r) => r,
        Err(reason) => {
            log::warn!("kal-iroh-shell: {reason}");
            return (StatusCode::BAD_GATEWAY, reason).into_response();
        }
    };
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| {
            mime_guess::from_path(&path)
                .first_or_octet_stream()
                .to_string()
        });
    let mut headers = axum::http::HeaderMap::new();
    if let Ok(v) = content_type.parse() {
        headers.insert(header::CONTENT_TYPE, v);
    }
    if let Some(len) = resp.headers().get(header::CONTENT_LENGTH).cloned() {
        headers.insert(header::CONTENT_LENGTH, len);
    }
    headers.insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static(RELEASE_CACHE_CONTROL),
    );
    let body = axum::body::Body::from_stream(resp.bytes_stream());
    (StatusCode::OK, headers, body).into_response()
}

/// `GET /__shell/status` — setting, served interface, current release,
/// reload state.  The server is asked only when the setting is on.
async fn status_handler(State(ui): Shared) -> Response {
    let enabled = ui.host.remote_ui_enabled();
    let served = ui.served.lock().ok().and_then(|g| g.clone());
    let (current, current_error) = if enabled {
        match ui.fetch_pointer().await {
            Ok(p) => (Some(p), None),
            Err(e) => (None, Some(e)),
        }
    } else {
        (None, None)
    };
    let reload = ui
        .handshake
        .lock()
        .ok()
        .map(|h| serde_json::json!({"pending": h.pending(), "busy_reason": h.busy_reason()}));
    axum::Json(serde_json::json!({
        "enabled": enabled,
        "served": served,
        "reload": reload,
        "current": current,
        "current_error": current_error,
    }))
    .into_response()
}

/// `POST /__shell/reload-ack` — the page's answer.
async fn ack_handler(State(ui): Shared, axum::Json(body): axum::Json<ReloadAck>) -> Response {
    log::info!(
        "kal-iroh-shell: page answered reload {}: {:?}",
        body.request,
        body.ack
    );
    ui.apply(|h, now| h.on_ack(body.request, body.ack, now));
    StatusCode::NO_CONTENT.into_response()
}

/// Wall-clock time this process started, in ms since the epoch.
///
/// From `/proc`: the process start (`stat` field 22, clock ticks since
/// boot) against the current uptime.  Linux and Android both use 100
/// ticks per second.  `None` where `/proc` is unavailable or unreadable.
pub fn process_start_epoch_ms() -> Option<u64> {
    const TICKS_PER_SEC: f64 = 100.0;
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The command name (field 2) is parenthesised and may hold spaces.
    let after_comm = &stat[stat.rfind(')')? + 2..];
    let start_ticks: f64 = after_comm.split(' ').nth(19)?.parse().ok()?;
    let uptime: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split(' ')
        .next()?
        .parse()
        .ok()?;
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as f64;
    let age_ms = (uptime - start_ticks / TICKS_PER_SEC) * 1000.0;
    Some((now_ms - age_ms) as u64)
}

/// `POST /__shell/boot-mark` — the page reached a boot phase.
///
/// Body: `{phase, epoch_ms, page_ms, source, build_id}` from the page
/// kit.  Logged with the time elapsed since this process started: the
/// cold-start figure.
async fn post_boot_mark(
    State(ui): Shared,
    axum::Json(mut mark): axum::Json<serde_json::Value>,
) -> Response {
    let Some(epoch_ms) = mark.get("epoch_ms").and_then(|v| v.as_u64()) else {
        return (StatusCode::UNPROCESSABLE_ENTITY, "epoch_ms is required").into_response();
    };
    let since_start = process_start_epoch_ms().map(|start| epoch_ms as i64 - start as i64);
    mark["since_process_start_ms"] = serde_json::json!(since_start);
    log::info!(
        "boot-mark: phase={} source={} build={} since_process_start_ms={} page_ms={}",
        mark["phase"],
        mark["source"],
        mark["build_id"],
        since_start.map_or_else(|| "?".to_string(), |v| v.to_string()),
        mark["page_ms"],
    );
    if let Ok(mut marks) = ui.boot_marks.lock() {
        if marks.len() == MAX_BOOT_MARKS {
            marks.remove(0);
        }
        marks.push(mark);
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `GET /__shell/boot-mark` — this process's start time and boot marks.
async fn get_boot_marks(State(ui): Shared) -> Response {
    let marks = ui.boot_marks.lock().map(|g| g.clone()).unwrap_or_default();
    axum::Json(serde_json::json!({
        "process_start_epoch_ms": process_start_epoch_ms(),
        "marks": marks,
    }))
    .into_response()
}
