//! Control channel consumer: learns, by push, that a new release of the
//! app is current, and starts the reload handshake.
//!
//! Follows the release server's `kal-sse` channel through the selected
//! server (`<releases_base>/api/control` snapshot, then
//! `<releases_base>/sse/control?sub=…`).  This channel is separate from
//! the app's own data streams: it only carries which release is current.
//! Each (re)connection re-takes the snapshot, which carries the current
//! release, so a publish made while disconnected is not missed.

use std::sync::Arc;
use std::time::Duration;

use kal_iroh_shell_protocol::{
    CONTROL_SNAPSHOT_PATH, CONTROL_STREAM_PATH, ControlSnapshot, ReleaseEvent,
    SNAPSHOT_INDEX_HEADER, SUB_ID_HEADER,
};

use super::RemoteUi;

/// Delay before the first reconnection; doubles up to [`MAX_BACKOFF`].
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// How often a disabled setting or a non-admitted device is rechecked.
const IDLE_RECHECK: Duration = Duration::from_secs(2);

/// One parsed SSE event.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub id: Option<u64>,
    pub data: String,
}

/// Remove and parse every complete event at the start of `buf`.
/// Comment-only blocks (keep-alives) yield nothing.
pub(crate) fn drain_events(buf: &mut String) -> Vec<SseEvent> {
    if buf.contains('\r') {
        *buf = buf.replace("\r\n", "\n");
    }
    let mut events = Vec::new();
    while let Some(end) = buf.find("\n\n") {
        let block: String = buf.drain(..end + 2).collect();
        let mut id = None;
        let mut data: Vec<&str> = Vec::new();
        for line in block.lines() {
            if let Some(v) = line.strip_prefix("id:") {
                id = v.trim().parse().ok();
            } else if let Some(v) = line.strip_prefix("data:") {
                data.push(v.strip_prefix(' ').unwrap_or(v));
            }
        }
        if !data.is_empty() {
            events.push(SseEvent {
                id,
                data: data.join("\n"),
            });
        }
    }
    events
}

fn get(ui: &RemoteUi, base: &str, path: &str) -> reqwest::RequestBuilder {
    let req = ui
        .host
        .client()
        .get(format!("{base}{}{path}", ui.config.releases_base));
    ui.host.authenticate(req)
}

/// Take the snapshot, then follow the stream until it ends or fails.
async fn follow_once(ui: &RemoteUi, base: &str, sub: &mut Option<String>) -> Result<(), String> {
    let mut req = get(ui, base, CONTROL_SNAPSHOT_PATH).timeout(Duration::from_secs(15));
    if let Some(s) = sub.as_deref() {
        req = req.query(&[("sub", s)]);
    }
    let resp = req.send().await.map_err(|e| format!("snapshot: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("snapshot: HTTP {}", resp.status().as_u16()));
    }
    let header = |name: &str| {
        resp.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let sub_id = header(SUB_ID_HEADER).ok_or("snapshot: no subscription id")?;
    let last_id = header(SNAPSHOT_INDEX_HEADER).unwrap_or_else(|| "0".into());
    *sub = Some(sub_id.clone());
    let snapshot: ControlSnapshot = resp.json().await.map_err(|e| format!("snapshot: {e}"))?;
    if let Some(current) = snapshot.current {
        ui.consider_release(&current.build_id);
    }

    let resp = get(ui, base, CONTROL_STREAM_PATH)
        .query(&[
            ("sub", sub_id.as_str()),
            ("last_event_id", last_id.as_str()),
        ])
        .send()
        .await
        .map_err(|e| format!("stream: {e}"))?;
    if resp.status() == reqwest::StatusCode::CONFLICT {
        log::info!("kal-iroh-shell: control stream needs a new snapshot");
        return Ok(());
    }
    if !resp.status().is_success() {
        return Err(format!("stream: HTTP {}", resp.status().as_u16()));
    }
    log::info!("kal-iroh-shell: control stream open");
    use futures::StreamExt;
    let mut body = resp.bytes_stream();
    let mut buf = String::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|e| format!("stream: {e}"))?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        for event in drain_events(&mut buf) {
            match serde_json::from_str::<ReleaseEvent>(&event.data) {
                Ok(release) => {
                    log::info!(
                        "kal-iroh-shell: release {} published ({} files changed)",
                        release.build_id,
                        release.changed.len()
                    );
                    ui.consider_release(&release.build_id);
                }
                Err(e) => log::warn!("kal-iroh-shell: bad control event: {e}"),
            }
        }
    }
    Err("stream ended".to_string())
}

/// Follow the control channel for as long as the process lives.
pub(crate) async fn run(ui: Arc<RemoteUi>) {
    let mut daemon_url = ui.host.upstream();
    let mut backoff = MIN_BACKOFF;
    let mut sub: Option<String> = None;
    loop {
        if !ui.host.remote_ui_enabled() || ui.host.authorized(IDLE_RECHECK).await.is_err() {
            tokio::time::sleep(IDLE_RECHECK).await;
            continue;
        }
        let base = daemon_url.borrow_and_update().clone();
        let outcome = tokio::select! {
            r = follow_once(&ui, &base, &mut sub) => r,
            _ = daemon_url.changed() => {
                log::info!("kal-iroh-shell: daemon changed, restarting the control stream");
                sub = None;
                backoff = MIN_BACKOFF;
                continue;
            }
        };
        match outcome {
            Ok(()) => backoff = MIN_BACKOFF,
            Err(e) => {
                log::info!("kal-iroh-shell: control stream: {e}; retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_events_are_drained_and_keep_alives_skipped() {
        let mut buf =
            ": ping\n\nid: 7\ndata: {\"build_id\":\"r2\"}\n\nid: 8\ndata: {\"bu".to_string();

        let events = drain_events(&mut buf);

        assert_eq!(
            events,
            vec![SseEvent {
                id: Some(7),
                data: "{\"build_id\":\"r2\"}".into()
            }]
        );
        assert_eq!(
            buf, "id: 8\ndata: {\"bu",
            "the partial event waits for more bytes"
        );
    }

    #[test]
    fn crlf_framing_is_accepted() {
        let mut buf = "id: 1\r\ndata: x\r\n\r\n".to_string();
        assert_eq!(
            drain_events(&mut buf),
            vec![SseEvent {
                id: Some(1),
                data: "x".into()
            }]
        );
    }
}
