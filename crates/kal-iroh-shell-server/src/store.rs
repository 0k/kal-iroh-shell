//! The release store: a bare git repository of built interfaces.
//!
//! - A *release* is a commit whose tree is one finished build (the Vite
//!   output, with its `ui-manifest.json` at the root).  Its message
//!   carries a `build-id:` and a `source:` trailer.
//! - A *channel* is a branch (`dev`, `alpha`, `beta`, `stable`, …; the
//!   set is open).  Publishing adds a commit on top of a channel;
//!   promoting moves a channel to any existing release, forwards or
//!   backwards.  Every move is an atomic git ref update and is kept in
//!   the reflog, so it can be undone.
//!
//! Everything goes through the `git` command (no working tree is ever
//! touched: builds are committed through a private index).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use kal_iroh_shell_protocol::{ChannelHead, MANIFEST_FILE, Manifest, valid_channel, valid_commit};

/// A failed store operation, with a message for a human.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreError {}

type Result<T> = std::result::Result<T, StoreError>;

fn err(msg: impl Into<String>) -> StoreError {
    StoreError(msg.into())
}

/// What changed between two releases.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeDiff {
    /// Paths added or modified.
    pub changed: Vec<String>,
    /// Paths that disappeared.
    pub removed: Vec<String>,
}

/// One entry of a channel's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub commit: String,
    pub build_id: String,
    pub published_at: String,
    pub subject: String,
}

/// A release store on disk.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// Open an existing store.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let store = Self { dir: dir.into() };
        let bare = store.git(&["rev-parse", "--is-bare-repository"])?;
        if bare.trim() != "true" {
            return Err(err(format!(
                "{} is not a release store (bare git repository)",
                store.dir.display()
            )));
        }
        Ok(store)
    }

    /// Open the store, creating it first when `dir` does not exist.
    pub fn open_or_init(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        if !dir.exists() {
            run(Command::new("git")
                .args(["init", "--quiet", "--bare", "--initial-branch=dev"])
                .arg(&dir))?;
            let store = Self { dir };
            // Bare repositories keep no reflog by default; every channel
            // move must stay undoable.
            store.git(&["config", "core.logAllRefUpdates", "always"])?;
            return Ok(store);
        }
        Self::open(dir)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn git_cmd(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.env("GIT_DIR", &self.dir)
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        cmd
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        let mut cmd = self.git_cmd();
        cmd.args(args);
        run(&mut cmd)
    }

    fn git_bytes(&self, args: &[&str]) -> Result<Vec<u8>> {
        let out = self
            .git_cmd()
            .args(args)
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| err(format!("git: {e}")))?;
        if !out.status.success() {
            return Err(err(String::from_utf8_lossy(&out.stderr).trim().to_string()));
        }
        Ok(out.stdout)
    }

    /// Commit the build in `dist` on top of `channel` and move the
    /// channel to it.  Returns the new release's commit id.  `source`
    /// says what the build was made from (e.g. a source commit).
    pub fn publish(&self, channel: &str, dist: &Path, source: &str) -> Result<String> {
        check_channel(channel)?;
        let manifest: Manifest =
            serde_json::from_slice(&std::fs::read(dist.join(MANIFEST_FILE)).map_err(|_| {
                err(format!(
                    "no {MANIFEST_FILE} in {}: not a finished build",
                    dist.display()
                ))
            })?)
            .map_err(|e| err(format!("{MANIFEST_FILE}: {e}")))?;
        let index = tempfile_path(&self.dir, "publish-index");
        let mut add = self.git_cmd();
        add.env("GIT_INDEX_FILE", &index)
            .env("GIT_WORK_TREE", dist)
            .args(["add", "--all", "--force", "."])
            .current_dir(dist);
        let added = run(&mut add);
        let tree = added.and_then(|_| {
            let mut wt = self.git_cmd();
            wt.env("GIT_INDEX_FILE", &index).args(["write-tree"]);
            run(&mut wt)
        });
        let _ = std::fs::remove_file(&index);
        let tree = tree?.trim().to_string();
        let parent = self.resolve_branch(channel)?;
        let message = format!(
            "release {}\n\nbuild-id: {}\nsource: {}\n",
            manifest.build_id, manifest.build_id, source
        );
        let mut args = vec!["commit-tree", tree.as_str(), "-m", message.as_str()];
        if let Some(p) = parent.as_deref() {
            args.extend(["-p", p]);
        }
        let mut commit_cmd = self.git_cmd();
        commit_cmd.args(&args);
        with_identity(&mut commit_cmd);
        let commit = run(&mut commit_cmd)?.trim().to_string();
        self.move_branch(
            channel,
            &commit,
            parent.as_deref(),
            &format!("publish {}", manifest.build_id),
        )?;
        Ok(commit)
    }

    /// Move `channel` to the release `target` names: another channel
    /// (`dev`), a git revision (`stable~1`, a commit id), or a build id.
    /// Returns the release's commit id.
    pub fn promote(&self, target: &str, channel: &str) -> Result<String> {
        check_channel(channel)?;
        let commit = self.resolve_release(target)?;
        let old = self.resolve_branch(channel)?;
        self.move_branch(
            channel,
            &commit,
            old.as_deref(),
            &format!("promote {target}"),
        )?;
        Ok(commit)
    }

    /// The commit `target` names; it must be a release (have a manifest).
    pub fn resolve_release(&self, target: &str) -> Result<String> {
        let by_rev = self
            .git(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{target}^{{commit}}"),
            ])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let commit = match by_rev {
            Some(c) => c,
            None => {
                let found = self.git(&[
                    "log",
                    "--all",
                    "--format=%H",
                    "--fixed-strings",
                    &format!("--grep=build-id: {target}"),
                ])?;
                let mut hits: Vec<&str> = found.lines().collect();
                hits.dedup();
                match hits.as_slice() {
                    [one] => one.to_string(),
                    [] => {
                        return Err(err(format!(
                            "no channel, revision or build id {target:?} in the store"
                        )));
                    }
                    _ => return Err(err(format!("build id {target:?} matches several releases"))),
                }
            }
        };
        self.manifest(&commit)?;
        Ok(commit)
    }

    fn resolve_branch(&self, channel: &str) -> Result<Option<String>> {
        Ok(self
            .git(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{channel}"),
            ])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()))
    }

    fn move_branch(&self, channel: &str, new: &str, old: Option<&str>, why: &str) -> Result<()> {
        // The expected old value makes the move atomic: a concurrent move
        // of the same channel fails instead of being silently overwritten.
        let zero = "0".repeat(40);
        let reference = format!("refs/heads/{channel}");
        self.git(&[
            "update-ref",
            "-m",
            why,
            &reference,
            new,
            old.unwrap_or(&zero),
        ])?;
        Ok(())
    }

    /// The manifest of release `commit`.
    pub fn manifest(&self, commit: &str) -> Result<Manifest> {
        let bytes = self
            .git_bytes(&["cat-file", "blob", &format!("{commit}:{MANIFEST_FILE}")])
            .map_err(|_| err(format!("{commit} is not a release (no {MANIFEST_FILE})")))?;
        serde_json::from_slice(&bytes).map_err(|e| err(format!("{MANIFEST_FILE} of {commit}: {e}")))
    }

    /// Where every channel points, sorted by channel name.
    pub fn channels(&self) -> Result<Vec<ChannelHead>> {
        let refs = self.git(&[
            "for-each-ref",
            "--format=%(refname:short) %(objectname) %(committerdate:iso-strict)",
            "refs/heads",
        ])?;
        let mut heads = Vec::new();
        for line in refs.lines() {
            let mut parts = line.splitn(3, ' ');
            let (Some(channel), Some(commit), Some(date)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if !valid_channel(channel) {
                continue;
            }
            let build_id = self
                .manifest(commit)
                .map(|m| m.build_id)
                .unwrap_or_default();
            heads.push(ChannelHead {
                channel: channel.to_string(),
                commit: commit.to_string(),
                build_id,
                published_at: date.to_string(),
            });
        }
        Ok(heads)
    }

    /// Paths that differ between two releases (`old` `None`: all paths).
    pub fn diff(&self, old: Option<&str>, new: &str) -> Result<TreeDiff> {
        let mut diff = TreeDiff::default();
        match old {
            Some(old) => {
                let out =
                    self.git(&["diff-tree", "-r", "--no-renames", "--name-status", old, new])?;
                for line in out.lines() {
                    let Some((status, path)) = line.split_once('\t') else {
                        continue;
                    };
                    if status == "D" {
                        diff.removed.push(path.to_string());
                    } else {
                        diff.changed.push(path.to_string());
                    }
                }
            }
            None => {
                let out = self.git(&["ls-tree", "-r", "--name-only", new])?;
                diff.changed = out.lines().map(str::to_string).collect();
            }
        }
        Ok(diff)
    }

    /// The bytes of `path` in release `commit`.
    pub fn read(&self, commit: &str, path: &str) -> Result<Vec<u8>> {
        if !valid_commit(commit) {
            return Err(err("invalid commit id"));
        }
        self.git_bytes(&["cat-file", "blob", &format!("{commit}:{path}")])
    }

    /// The history of `channel`, newest first, at most `limit` entries.
    pub fn log(&self, channel: &str, limit: usize) -> Result<Vec<LogEntry>> {
        check_channel(channel)?;
        let out = self.git(&[
            "log",
            &format!("--max-count={limit}"),
            "--format=%H%x1f%cI%x1f%s%x1f%(trailers:key=build-id,valueonly,separator=)",
            &format!("refs/heads/{channel}"),
        ])?;
        Ok(out
            .lines()
            .filter_map(|line| {
                let f: Vec<&str> = line.split('\x1f').collect();
                Some(LogEntry {
                    commit: f.first()?.to_string(),
                    published_at: f.get(1)?.to_string(),
                    subject: f.get(2)?.to_string(),
                    build_id: f.get(3).map(|s| s.trim().to_string()).unwrap_or_default(),
                })
            })
            .collect())
    }

    /// Path → blob id of every file of release `commit`.
    pub fn files(&self, commit: &str) -> Result<BTreeMap<String, String>> {
        let out = self.git(&["ls-tree", "-r", commit])?;
        Ok(out
            .lines()
            .filter_map(|line| {
                let (meta, path) = line.split_once('\t')?;
                Some((path.to_string(), meta.split(' ').nth(2)?.to_string()))
            })
            .collect())
    }
}

fn check_channel(channel: &str) -> Result<()> {
    if valid_channel(channel) {
        Ok(())
    } else {
        Err(err(format!(
            "invalid channel name {channel:?}: lowercase letters, digits, '.', '_', '-'"
        )))
    }
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| err(format!("git: {e}")))?;
    if !out.status.success() {
        return Err(err(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A commit needs an author; use git's configured identity when there is
/// one, else a neutral one (a release store may live on a machine with no
/// git identity).
fn with_identity(cmd: &mut Command) {
    let configured = Command::new("git")
        .args(["config", "user.email"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);
    if !configured && std::env::var_os("GIT_AUTHOR_EMAIL").is_none() {
        for (k, v) in [
            ("GIT_AUTHOR_NAME", "kal-iroh-shell"),
            ("GIT_AUTHOR_EMAIL", "kal-iroh-shell@localhost"),
            ("GIT_COMMITTER_NAME", "kal-iroh-shell"),
            ("GIT_COMMITTER_EMAIL", "kal-iroh-shell@localhost"),
        ] {
            cmd.env(k, v);
        }
    }
}

fn tempfile_path(dir: &Path, what: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    dir.join(format!("{what}-{}-{nanos}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A finished build: `files` plus a manifest with `build_id`.
    fn build(root: &Path, build_id: &str, files: &[(&str, &str)]) -> PathBuf {
        let dist = root.join(build_id);
        std::fs::create_dir_all(&dist).unwrap();
        for (path, content) in files {
            let full = dist.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, content).unwrap();
        }
        let entries: Vec<_> = files
            .iter()
            .map(|(p, c)| serde_json::json!({"path": p, "size": c.len(), "sha256": "x"}))
            .collect();
        std::fs::write(
            dist.join(MANIFEST_FILE),
            serde_json::json!({"build_id": build_id, "files": entries}).to_string(),
        )
        .unwrap();
        dist
    }

    fn fixture() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open_or_init(tmp.path().join("releases.git")).unwrap();
        (tmp, store)
    }

    fn head(store: &Store, channel: &str) -> Option<String> {
        store
            .channels()
            .unwrap()
            .into_iter()
            .find(|h| h.channel == channel)
            .map(|h| h.build_id)
    }

    #[test]
    fn publishing_adds_a_release_on_top_of_one_channel_only() {
        let (tmp, store) = fixture();
        let b1 = build(
            tmp.path(),
            "b1",
            &[("index.html", "1"), ("assets/a.js", "a")],
        );
        let b2 = build(
            tmp.path(),
            "b2",
            &[("index.html", "2"), ("assets/a.js", "a")],
        );

        let c1 = store.publish("dev", &b1, "src1").unwrap();
        store.promote("dev", "stable").unwrap();
        let c2 = store.publish("dev", &b2, "src2").unwrap();

        assert_eq!(head(&store, "dev").as_deref(), Some("b2"));
        assert_eq!(
            head(&store, "stable").as_deref(),
            Some("b1"),
            "a publish moves only its channel"
        );
        assert_eq!(store.read(&c2, "index.html").unwrap(), b"2");
        assert_eq!(
            store.diff(Some(&c1), &c2).unwrap(),
            TreeDiff {
                changed: vec!["index.html".into(), MANIFEST_FILE.into()],
                removed: vec![]
            }
        );
        let log = store.log("dev", 10).unwrap();
        assert_eq!(
            log.iter().map(|e| e.build_id.as_str()).collect::<Vec<_>>(),
            ["b2", "b1"]
        );
    }

    #[test]
    fn a_channel_moves_to_any_release_forwards_or_backwards() {
        let (tmp, store) = fixture();
        store
            .publish("dev", &build(tmp.path(), "b1", &[("index.html", "1")]), "s")
            .unwrap();
        store
            .publish("dev", &build(tmp.path(), "b2", &[("index.html", "2")]), "s")
            .unwrap();

        store.promote("dev", "beta").unwrap();
        assert_eq!(head(&store, "beta").as_deref(), Some("b2"));
        store.promote("beta~1", "beta").unwrap();
        assert_eq!(
            head(&store, "beta").as_deref(),
            Some("b1"),
            "back by revision"
        );
        store.promote("b2", "beta").unwrap();
        assert_eq!(
            head(&store, "beta").as_deref(),
            Some("b2"),
            "forward by build id"
        );
    }

    #[test]
    fn promoting_refuses_unknown_releases_and_bad_channel_names() {
        let (tmp, store) = fixture();
        store
            .publish("dev", &build(tmp.path(), "b1", &[("index.html", "1")]), "s")
            .unwrap();

        assert!(
            store
                .promote("nope", "beta")
                .unwrap_err()
                .0
                .contains("no channel, revision or build id")
        );
        assert!(
            store
                .promote("dev", "Beta")
                .unwrap_err()
                .0
                .contains("invalid channel name")
        );
        assert!(
            store
                .publish("a/b", &build(tmp.path(), "b2", &[]), "s")
                .is_err()
        );
        assert_eq!(head(&store, "beta"), None);
    }

    #[test]
    fn publishing_refuses_an_unfinished_build() {
        let (tmp, store) = fixture();
        let empty = tmp.path().join("empty");
        std::fs::create_dir(&empty).unwrap();

        assert!(
            store
                .publish("dev", &empty, "s")
                .unwrap_err()
                .0
                .contains("not a finished build")
        );
    }
}
