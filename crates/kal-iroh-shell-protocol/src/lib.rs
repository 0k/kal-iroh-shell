//! Wire formats of `kal-iroh-shell`, defined once.
//!
//! Three parties speak them:
//!
//! - the **release server** (`kal-iroh-shell-server`), on the machine
//!   that publishes an app's interface;
//! - the **client shell** (`kal-iroh-shell`), the local server inside
//!   the app on each device;
//! - the **page kit** (npm `@0k/kal-iroh-shell`), inside the page.
//!
//! This crate has no HTTP or async dependency on purpose: every party
//! can depend on it.  The TypeScript twins of these names live in the
//! page kit's `src/protocol.ts` and must stay identical.

use serde::{Deserialize, Serialize};

// ---- Releases ---------------------------------------------------------

/// Legacy publish-directory pointer file (before the git release store).
/// Kept so the page kit's copy of these names stays in step.
pub const POINTER_FILE: &str = "current.json";
/// File, at the root of every release, listing its files (written by the
/// page kit's Vite plugin).
pub const MANIFEST_FILE: &str = "ui-manifest.json";

/// Where one channel points.
///
/// Releases live in a git repository (the release store): a release is a
/// commit of a built interface, a channel is a branch.  `commit` is the
/// release's full commit id; it is also the path segment its files are
/// served under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelHead {
    pub channel: String,
    pub commit: String,
    pub build_id: String,
    pub published_at: String,
}

/// Channel names are git branch names restricted to one safe segment:
/// lowercase letters, digits, `.`, `_`, `-`, starting with a letter or
/// digit (`dev`, `alpha`, `beta`, `stable`, `rc`, …).
pub fn valid_channel(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_lowercase() || b.is_ascii_digit())
        && name.len() <= 64
        && !name.ends_with(".lock")
        && !name.contains("..")
        && bytes.all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// A full commit id: 40 lowercase hex digits.
pub fn valid_commit(id: &str) -> bool {
    id.len() == 40
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One file of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// `ui-manifest.json`: every file of one release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub build_id: String,
    pub files: Vec<ManifestEntry>,
}

/// Build ids are one directory name and one URL path segment.
pub fn valid_build_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// A relative path inside a release: no empty, `.` or `..` segment.
pub fn valid_release_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && path
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

// ---- Release server (below the app's releases base URL) -------------

/// Where one channel points: `GET /channels/<channel>` → [`ChannelHead`].
pub const CHANNELS_PREFIX: &str = "/channels";
/// REST snapshot of the control channel: [`ControlSnapshot`].
pub const CONTROL_SNAPSHOT_PATH: &str = "/api/control";
/// SSE stream of the control channel (`?sub=<id>&last_event_id=<n>`).
pub const CONTROL_STREAM_PATH: &str = "/sse/control";
/// Response header of the snapshot carrying the subscription id.
pub const SUB_ID_HEADER: &str = "x-sub-id";
/// Response header of the snapshot carrying its cursor (`kal-sse`).
pub const SNAPSHOT_INDEX_HEADER: &str = "x-snapshot-index";

/// Body of [`CONTROL_SNAPSHOT_PATH`]: where every channel points.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlSnapshot {
    pub channels: Vec<ChannelHead>,
}

/// One control event: a channel moved to another release (a publish, a
/// promotion, or a rollback).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseEvent {
    pub channel: String,
    pub commit: String,
    pub build_id: String,
    pub published_at: String,
    /// The commit the channel pointed to before; `None` for a new channel.
    pub previous: Option<String>,
    /// Paths added or changed since `previous` (all paths when unknown).
    pub changed: Vec<String>,
    /// Paths of `previous` absent from this release.
    pub removed: Vec<String>,
}

// ---- Client shell local server (same origin as the page) ------------

/// Prefix of every route the client shell adds to the local server.
pub const SHELL_PREFIX: &str = "/__shell";
/// `GET`: setting, served release, current release, reload state.
pub const STATUS_PATH: &str = "/__shell/status";
/// `POST`: the page's answer to "prepare to reload" ([`ReloadAck`]).
pub const RELOAD_ACK_PATH: &str = "/__shell/reload-ack";
/// `GET` / `POST`: boot timing marks.
pub const BOOT_MARK_PATH: &str = "/__shell/boot-mark";
/// Release files are served at `/__shell/ui/<commit>/<path>`.
pub const RELEASE_FILES_PREFIX: &str = "/__shell/ui";

/// Attribute set on the page's root element: `remote` or `embedded`.
pub const UI_SOURCE_ATTR: &str = "data-ui-source";
/// Attribute set on the page's root element: the running build id.
pub const UI_BUILD_ATTR: &str = "data-ui-build";

/// Global object the page kit installs; the shell calls
/// `window.<PAGE_GLOBAL>.prepareReload(request)`.
pub const PAGE_GLOBAL: &str = "__kalIrohShell";

/// The page's answer to a "prepare to reload".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Ack {
    /// Saved; reload now.
    Ready,
    /// The user is in the middle of something; renewed while it lasts.
    Busy { reason: String },
}

/// Body of [`RELOAD_ACK_PATH`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadAck {
    pub request: u64,
    #[serde(flatten)]
    pub ack: Ack,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_answers_have_the_page_kit_shape() {
        let ready: ReloadAck = serde_json::from_str(r#"{"request":3,"status":"ready"}"#).unwrap();
        let busy: ReloadAck =
            serde_json::from_str(r#"{"request":3,"status":"busy","reason":"microphone on"}"#)
                .unwrap();

        assert_eq!(
            ready,
            ReloadAck {
                request: 3,
                ack: Ack::Ready
            }
        );
        assert_eq!(
            busy.ack,
            Ack::Busy {
                reason: "microphone on".into()
            }
        );
    }

    #[test]
    fn release_paths_cannot_leave_the_release() {
        for bad in ["", "../x", "a/../b", "a//b", "./a", "a\\b"] {
            assert!(!valid_release_path(bad), "{bad:?}");
        }
        assert!(valid_release_path("assets/main.js"));
        assert!(valid_build_id("20261025T061020-62e172"));
        assert!(!valid_build_id("../x"));
    }

    #[test]
    fn channel_names_are_single_safe_branch_names() {
        for good in ["dev", "alpha", "beta", "stable", "rc", "rc-2", "v1.2"] {
            assert!(valid_channel(good), "{good}");
        }
        for bad in ["", "Dev", "-x", "a/b", "a..b", "x.lock", "a b", "../x"] {
            assert!(!valid_channel(bad), "{bad:?}");
        }
        assert!(valid_commit("47675ee0819e4c942e9861dc7b32b832e877b47d"));
        assert!(!valid_commit("47675ee"));
    }
}
