//! Thin wrappers around the `git` binary.
//!
//! Each function shells out, captures stdout, and returns a typed result.
//! No state is held; every call is independent. The whole module is
//! intentionally low-level — orchestration lives in higher modules.

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn run(repo: Option<&Path>, args: &[&str]) -> Result<Output> {
    let mut cmd = Command::new("git");
    if let Some(p) = repo {
        cmd.current_dir(p);
    }
    cmd.args(args);
    let out = cmd
        .output()
        .with_context(|| format!("spawning `git {}`", args.join(" ")))?;
    Ok(out)
}

fn run_checked(repo: Option<&Path>, args: &[&str]) -> Result<Output> {
    let out = run(repo, args)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!(
            "git {} failed (exit {:?}): {}",
            args.join(" "),
            out.status.code(),
            stderr.trim()
        );
    }
    Ok(out)
}

fn stdout_trimmed(out: Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn is_git_repo(path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    run(Some(path), &["rev-parse", "--git-dir"])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

pub fn fetch(repo: &Path, remote: &str) -> Result<()> {
    run_checked(Some(repo), &["fetch", remote])?;
    Ok(())
}

pub fn fetch_prune(repo: &Path, remote: &str) -> Result<()> {
    run_checked(Some(repo), &["fetch", "--prune", remote])?;
    Ok(())
}

pub fn pull_ff_only(repo: &Path, remote: &str, branch: &str) -> Result<()> {
    run_checked(
        Some(repo),
        &["pull", "--ff-only", "--prune", remote, branch],
    )?;
    Ok(())
}

pub fn current_branch(repo: &Path) -> Result<String> {
    let out = run_checked(Some(repo), &["rev-parse", "--abbrev-ref", "HEAD"])?;
    Ok(stdout_trimmed(out))
}

pub fn branch_exists_local(repo: &Path, branch: &str) -> Result<bool> {
    let ref_name = format!("refs/heads/{branch}");
    let out = run(Some(repo), &["rev-parse", "--verify", "--quiet", &ref_name])?;
    Ok(out.status.success())
}

pub fn branch_exists_remote(repo: &Path, remote: &str, branch: &str) -> Result<bool> {
    let ref_name = format!("refs/remotes/{remote}/{branch}");
    let out = run(Some(repo), &["rev-parse", "--verify", "--quiet", &ref_name])?;
    Ok(out.status.success())
}

/// Returns the short name of the remote's default branch — e.g. `main` or `master`.
pub fn resolve_origin_head(repo: &Path) -> Result<String> {
    let out = run_checked(
        Some(repo),
        &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
    )?;
    let s = stdout_trimmed(out);
    Ok(s.strip_prefix("origin/").unwrap_or(&s).to_string())
}

/// Empty string means clean.
pub fn status_porcelain(repo: &Path) -> Result<String> {
    let out = run_checked(Some(repo), &["status", "--porcelain"])?;
    Ok(stdout_trimmed(out))
}

/// `(ahead, behind)` relative to the upstream, or `None` if no upstream is set.
pub fn ahead_behind(repo: &Path, branch: &str) -> Result<Option<(usize, usize)>> {
    // First check whether an upstream is configured for this branch.
    let upstream_check = run(
        Some(repo),
        &[
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            &format!("{branch}@{{u}}"),
        ],
    )?;
    if !upstream_check.status.success() {
        return Ok(None);
    }

    let out = run_checked(
        Some(repo),
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("{branch}@{{u}}...{branch}"),
        ],
    )?;
    let s = stdout_trimmed(out);
    let mut parts = s.split_whitespace();
    let behind: usize = parts.next().unwrap_or("0").parse().unwrap_or(0);
    let ahead: usize = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Ok(Some((ahead, behind)))
}

/// Whether `branch` is fully merged into `base` (an ancestor of `base`).
pub fn is_merged(repo: &Path, branch: &str, base: &str) -> Result<bool> {
    let out = run(Some(repo), &["merge-base", "--is-ancestor", branch, base])?;
    Ok(out.status.success())
}

pub fn delete_branch(repo: &Path, branch: &str, force: bool) -> Result<()> {
    let flag = if force { "-D" } else { "-d" };
    run_checked(Some(repo), &["branch", flag, branch])?;
    Ok(())
}

/// Resolve a per-worktree git path via `git rev-parse --git-path <name>` in
/// the worktree. For a linked worktree, this returns
/// `<canonical>/.git/worktrees/<wt-name>/<name>` — i.e. the right per-worktree
/// location for sidecar-style metadata that should be cleaned up automatically
/// when the worktree is removed.
pub fn git_path(worktree: &Path, name: &str) -> Result<PathBuf> {
    let out = run_checked(Some(worktree), &["rev-parse", "--git-path", name])?;
    Ok(PathBuf::from(stdout_trimmed(out)))
}

// ---------------------------------------------------------------------------
// worktree operations
// ---------------------------------------------------------------------------

/// Create a worktree at `path` checking out a brand-new branch `branch` off
/// `base` (which is typically a remote-tracking ref like `origin/main`).
///
/// `--no-track` is important: when `base` is `origin/main`, modern git
/// otherwise defaults to making the new branch track `origin/main`, which is
/// wrong for our model — we want the branch to track its own remote
/// counterpart only once it's pushed.
pub fn worktree_add_new_branch(
    canonical: &Path,
    path: &Path,
    branch: &str,
    base: &str,
) -> Result<()> {
    run_checked(
        Some(canonical),
        &[
            "worktree",
            "add",
            "--no-track",
            "-b",
            branch,
            path.to_str().context("worktree path is not utf-8")?,
            base,
        ],
    )?;
    Ok(())
}

/// Create a worktree at `path` checking out the *existing* local branch
/// `branch`. Used when the branch already exists in the canonical's `.git/`.
pub fn worktree_add_existing_local(canonical: &Path, path: &Path, branch: &str) -> Result<()> {
    run_checked(
        Some(canonical),
        &[
            "worktree",
            "add",
            path.to_str().context("worktree path is not utf-8")?,
            branch,
        ],
    )?;
    Ok(())
}

/// Create a worktree at `path` checking out a new local branch `branch` that
/// tracks `origin/<branch>` — i.e. resume a remote-only branch into a fresh
/// worktree.
pub fn worktree_add_tracking_remote(canonical: &Path, path: &Path, branch: &str) -> Result<()> {
    let upstream = format!("origin/{branch}");
    run_checked(
        Some(canonical),
        &[
            "worktree",
            "add",
            "--track",
            "-b",
            branch,
            path.to_str().context("worktree path is not utf-8")?,
            &upstream,
        ],
    )?;
    Ok(())
}

/// Remove a worktree by its on-disk path. `force = true` accepts dirty trees.
pub fn worktree_remove(canonical: &Path, path: &Path, force: bool) -> Result<()> {
    let path_str = path.to_str().context("worktree path is not utf-8")?;
    let mut args: Vec<&str> = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path_str);
    run_checked(Some(canonical), &args)?;
    Ok(())
}

/// Prune stale worktree registrations whose directories no longer exist.
/// Cheap and idempotent; safe to call before any `worktree add`.
pub fn worktree_prune(canonical: &Path) -> Result<()> {
    run_checked(Some(canonical), &["worktree", "prune"])?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeEntry {
    pub path: PathBuf,
    /// Short branch name (e.g. `alice/PROJ-1`). `None` for detached HEAD or
    /// bare entries.
    pub branch: Option<String>,
}

/// Run `git worktree list --porcelain` in `canonical` and parse the output
/// into [`WorktreeEntry`] values. The porcelain format is line-oriented,
/// with one record per worktree separated by a blank line.
pub fn worktree_list(canonical: &Path) -> Result<Vec<WorktreeEntry>> {
    let out = run_checked(Some(canonical), &["worktree", "list", "--porcelain"])?;
    let raw = String::from_utf8_lossy(&out.stdout);
    Ok(parse_worktree_list(&raw))
}

fn parse_worktree_list(raw: &str) -> Vec<WorktreeEntry> {
    let mut out = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;
    for line in raw.lines() {
        if line.is_empty() {
            if let Some(p) = current_path.take() {
                out.push(WorktreeEntry {
                    path: p,
                    branch: current_branch.take(),
                });
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("worktree ") {
            // Starting a new record without a blank line separator (can happen
            // for the very first record). Flush any in-progress record first.
            if let Some(p) = current_path.take() {
                out.push(WorktreeEntry {
                    path: p,
                    branch: current_branch.take(),
                });
            }
            current_path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("branch ") {
            current_branch = Some(rest.strip_prefix("refs/heads/").unwrap_or(rest).to_string());
        }
        // Other porcelain lines (HEAD, detached, bare, locked, prunable) are
        // ignored — we only care about path + branch.
    }
    // Trailing record without terminator.
    if let Some(p) = current_path.take() {
        out.push(WorktreeEntry {
            path: p,
            branch: current_branch.take(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn init_repo(path: &Path) {
        run_checked(
            None,
            &["init", "--initial-branch=main", path.to_str().unwrap()],
        )
        .unwrap();
        run_checked(Some(path), &["config", "user.email", "test@example.com"]).unwrap();
        run_checked(Some(path), &["config", "user.name", "Test User"]).unwrap();
        run_checked(Some(path), &["config", "commit.gpgsign", "false"]).unwrap();
    }

    fn commit_file(path: &Path, name: &str, contents: &str) {
        fs::write(path.join(name), contents).unwrap();
        run_checked(Some(path), &["add", name]).unwrap();
        run_checked(Some(path), &["commit", "-m", &format!("add {name}")]).unwrap();
    }

    #[test]
    fn is_git_repo_true_for_initialised_directory() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        assert!(is_git_repo(tmp.path()));
    }

    #[test]
    fn is_git_repo_false_for_plain_directory() {
        let tmp = TempDir::new().unwrap();
        assert!(!is_git_repo(tmp.path()));
    }

    #[test]
    fn is_git_repo_false_for_missing_path() {
        assert!(!is_git_repo(Path::new("/definitely/not/here/csw-test")));
    }

    #[test]
    fn current_branch_after_init_is_main() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        commit_file(tmp.path(), "README", "hi");
        assert_eq!(current_branch(tmp.path()).unwrap(), "main");
    }

    #[test]
    fn branch_exists_local_distinguishes_existing_from_missing() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        commit_file(tmp.path(), "README", "hi");
        run_checked(Some(tmp.path()), &["branch", "feature/x"]).unwrap();

        assert!(branch_exists_local(tmp.path(), "feature/x").unwrap());
        assert!(!branch_exists_local(tmp.path(), "nope").unwrap());
    }

    #[test]
    fn status_porcelain_empty_when_clean() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        commit_file(tmp.path(), "README", "hi");
        assert_eq!(status_porcelain(tmp.path()).unwrap(), "");
    }

    #[test]
    fn status_porcelain_reports_dirty_files() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        commit_file(tmp.path(), "README", "hi");
        fs::write(tmp.path().join("dirty"), "x").unwrap();
        let s = status_porcelain(tmp.path()).unwrap();
        assert!(s.contains("dirty"), "expected `dirty` in: {s}");
    }

    #[test]
    fn delete_branch_removes_local_branch() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        commit_file(tmp.path(), "README", "hi");
        run_checked(Some(tmp.path()), &["branch", "feature/x"]).unwrap();
        delete_branch(tmp.path(), "feature/x", false).unwrap();
        assert!(!branch_exists_local(tmp.path(), "feature/x").unwrap());
    }

    #[test]
    fn ahead_behind_none_without_upstream() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        commit_file(tmp.path(), "README", "hi");
        assert!(ahead_behind(tmp.path(), "main").unwrap().is_none());
    }

    #[test]
    fn is_merged_true_for_self_and_ancestor() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        commit_file(tmp.path(), "README", "hi");
        assert!(is_merged(tmp.path(), "main", "main").unwrap());
    }

    #[test]
    fn is_merged_false_for_diverged_branch() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        commit_file(tmp.path(), "README", "hi");
        // Create a divergent branch via worktree (no checkout-new-from helper
        // anymore — the canonical stays on main, the worktree carries the
        // diverging history).
        let wt = tmp.path().parent().unwrap().join("diverge-wt");
        worktree_add_new_branch(tmp.path(), &wt, "feature/x", "main").unwrap();
        commit_file(&wt, "feature.txt", "x");
        assert!(!is_merged(tmp.path(), "feature/x", "main").unwrap());
        worktree_remove(tmp.path(), &wt, true).unwrap();
    }

    #[test]
    fn worktree_add_new_branch_creates_path_and_branch() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        commit_file(tmp.path(), "README", "hi");
        let wt = tmp.path().parent().unwrap().join("csw-test-wt-1");
        worktree_add_new_branch(tmp.path(), &wt, "feature/y", "main").unwrap();
        assert!(wt.exists());
        assert_eq!(current_branch(&wt).unwrap(), "feature/y");
        worktree_remove(tmp.path(), &wt, false).unwrap();
        assert!(!wt.exists());
    }

    #[test]
    fn worktree_list_includes_added_worktree() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        commit_file(tmp.path(), "README", "hi");
        let wt = tmp.path().parent().unwrap().join("csw-test-wt-2");
        worktree_add_new_branch(tmp.path(), &wt, "feature/z", "main").unwrap();
        let entries = worktree_list(tmp.path()).unwrap();
        let branches: Vec<&str> = entries.iter().filter_map(|e| e.branch.as_deref()).collect();
        assert!(branches.contains(&"main"));
        assert!(branches.contains(&"feature/z"));
        worktree_remove(tmp.path(), &wt, false).unwrap();
    }

    #[test]
    fn worktree_prune_removes_orphan_registration() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        commit_file(tmp.path(), "README", "hi");
        let wt = tmp.path().parent().unwrap().join("csw-test-wt-prune");
        worktree_add_new_branch(tmp.path(), &wt, "feature/p", "main").unwrap();
        // Nuke the directory without telling git.
        std::fs::remove_dir_all(&wt).unwrap();
        worktree_prune(tmp.path()).unwrap();
        let entries = worktree_list(tmp.path()).unwrap();
        let paths: Vec<&Path> = entries.iter().map(|e| e.path.as_path()).collect();
        assert!(!paths.contains(&wt.as_path()));
    }

    #[test]
    fn parse_worktree_list_handles_multiple_records() {
        let raw = "\
worktree /a/canonical
HEAD abc123
branch refs/heads/main

worktree /b/wt-1
HEAD def456
branch refs/heads/feature/x

worktree /c/wt-detached
HEAD 789abc
detached
";
        let parsed = parse_worktree_list(raw);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].path, PathBuf::from("/a/canonical"));
        assert_eq!(parsed[0].branch.as_deref(), Some("main"));
        assert_eq!(parsed[1].path, PathBuf::from("/b/wt-1"));
        assert_eq!(parsed[1].branch.as_deref(), Some("feature/x"));
        assert_eq!(parsed[2].path, PathBuf::from("/c/wt-detached"));
        assert_eq!(parsed[2].branch, None);
    }

    #[test]
    fn git_path_returns_per_worktree_dir() {
        let tmp = TempDir::new().unwrap();
        init_repo(tmp.path());
        commit_file(tmp.path(), "README", "hi");
        let wt = tmp.path().parent().unwrap().join("csw-test-wt-gp");
        worktree_add_new_branch(tmp.path(), &wt, "feature/gp", "main").unwrap();
        let p = git_path(&wt, "csw.json").unwrap();
        let s = p.display().to_string();
        assert!(
            s.contains("worktrees") && s.ends_with("csw.json"),
            "unexpected git-path result: {s}"
        );
        worktree_remove(tmp.path(), &wt, false).unwrap();
    }
}
