//! Per-worktree sidecar metadata. Resolved from the worktree side via
//! `git rev-parse --git-path csw.json`, which lands the file inside the
//! worktree's private git-dir (`<canonical>/.git/worktrees/<name>/csw.json`).
//! Git wipes that directory automatically on `git worktree remove`, so
//! there's no cleanup to do at csw's end.
//!
//! A missing or corrupt sidecar is non-fatal; callers should treat the
//! worktree directory name as the authoritative source for the task id.

use crate::git;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

const FILE_NAME: &str = "csw.json";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Sidecar {
    pub task_id: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl Sidecar {
    pub fn new(
        task_id: impl Into<String>,
        branch: impl Into<String>,
        title: Option<String>,
    ) -> Self {
        Self {
            task_id: task_id.into(),
            branch: branch.into(),
            title,
            created_at: Utc::now(),
        }
    }
}

/// Resolve the sidecar's on-disk path for a worktree. Delegates to
/// `git rev-parse --git-path` so the returned location lives inside the
/// per-worktree git dir, regardless of whether the worktree is the main
/// one or a linked one.
pub fn sidecar_path(worktree_root: &Path) -> Result<PathBuf> {
    git::git_path(worktree_root, FILE_NAME)
        .with_context(|| format!("resolving sidecar path for {}", worktree_root.display()))
}

/// Write a sidecar. The parent git dir always exists for a real worktree,
/// but we create it just in case (some tests operate on bare directories).
pub fn write(worktree_root: &Path, sidecar: &Sidecar) -> Result<()> {
    let path = sidecar_path(worktree_root)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let serialised = serde_json::to_string_pretty(sidecar).context("serialising sidecar")?;
    fs::write(&path, serialised).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Read a sidecar. `Ok(None)` for missing or unparseable files — the caller
/// decides how loud to be about it.
pub fn read(worktree_root: &Path) -> Result<Option<Sidecar>> {
    let path = match sidecar_path(worktree_root) {
        Ok(p) => p,
        // The worktree may have become invalid (manually removed, etc.).
        // Treat that as "no sidecar".
        Err(_) => return Ok(None),
    };
    if !path.exists() {
        return Ok(None);
    }
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    match serde_json::from_str::<Sidecar>(&raw) {
        Ok(s) => Ok(Some(s)),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    /// Build a real git repo + linked worktree pair. The worktree's `.git`
    /// is the small text file pointing into the canonical, which is exactly
    /// what `sidecar_path` needs to resolve correctly.
    struct WorktreeFixture {
        _tmp: TempDir,
        worktree: PathBuf,
    }

    impl WorktreeFixture {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let canonical = tmp.path().join("canonical");
            std::fs::create_dir_all(&canonical).unwrap();
            git_run(&canonical, &["init", "--initial-branch=main"]);
            git_run(&canonical, &["config", "user.email", "test@example.com"]);
            git_run(&canonical, &["config", "user.name", "Test"]);
            git_run(&canonical, &["config", "commit.gpgsign", "false"]);
            std::fs::write(canonical.join("README"), "x").unwrap();
            git_run(&canonical, &["add", "README"]);
            git_run(&canonical, &["commit", "-m", "init"]);

            let worktree = tmp.path().join("worktree");
            git_run(
                &canonical,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "feature/x",
                    worktree.to_str().unwrap(),
                ],
            );
            Self {
                _tmp: tmp,
                worktree,
            }
        }
    }

    fn git_run(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn write_then_read_round_trip() {
        let f = WorktreeFixture::new();
        let sidecar = Sidecar::new("PROJ-1", "alice/PROJ-1", Some("My title".into()));
        write(&f.worktree, &sidecar).unwrap();

        let loaded = read(&f.worktree).unwrap().expect("sidecar exists");
        assert_eq!(loaded.task_id, "PROJ-1");
        assert_eq!(loaded.branch, "alice/PROJ-1");
        assert_eq!(loaded.title.as_deref(), Some("My title"));
    }

    #[test]
    fn read_returns_none_when_missing() {
        let f = WorktreeFixture::new();
        assert!(read(&f.worktree).unwrap().is_none());
    }

    #[test]
    fn read_returns_none_for_corrupt_sidecar() {
        let f = WorktreeFixture::new();
        // Seed the file via write then corrupt it.
        let s = Sidecar::new("X", "u/X", None);
        write(&f.worktree, &s).unwrap();
        let path = sidecar_path(&f.worktree).unwrap();
        std::fs::write(path, "{ not valid json").unwrap();
        assert!(read(&f.worktree).unwrap().is_none());
    }

    #[test]
    fn sidecar_path_lives_in_worktrees_subdir() {
        let f = WorktreeFixture::new();
        let p = sidecar_path(&f.worktree).unwrap();
        let s = p.display().to_string();
        assert!(
            s.contains("worktrees") && s.ends_with("csw.json"),
            "unexpected sidecar path: {s}"
        );
    }

    #[test]
    fn title_is_optional_in_serialised_form() {
        let f = WorktreeFixture::new();
        let s = Sidecar::new("PROJ-1", "alice/PROJ-1", None);
        write(&f.worktree, &s).unwrap();
        let path = sidecar_path(&f.worktree).unwrap();
        let raw = std::fs::read_to_string(path).unwrap();
        assert!(
            !raw.contains("title"),
            "title should be omitted when None: {raw}"
        );
    }
}
