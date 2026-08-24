//! `csw migrate` — convert on-disk tasks from the old repo-then-task layout
//! (`<tasks_dir>/<repo>/<user>-<task_id>`) to the new task-then-repo layout
//! (`<tasks_dir>/<user>-<task_id>/<repo>`).
//!
//! Detection (`scan_legacy`) and the one-line warning it powers
//! (`legacy_warning`) are shared by every command that discovers tasks, so
//! the "you have old-format tasks, run `csw migrate`" message is consistent
//! everywhere instead of being reimplemented per command.

use crate::progress::Reporter;
use crate::{Config, git, paths, sidecar};
use anyhow::{Context, Result};
use std::path::PathBuf;

/// One old-format task found on disk, with every repo copy that belongs to
/// it (regardless of which user owns it — migration is user-agnostic).
#[derive(Debug, Clone)]
pub struct LegacyTask {
    pub user: String,
    pub task_id: String,
    pub repos: Vec<LegacyRepo>,
}

#[derive(Debug, Clone)]
pub struct LegacyRepo {
    pub repo: String,
    pub old_path: PathBuf,
}

/// Walk every configured repo's old-format tasks dir
/// (`<tasks_dir>/<repo>/*`) and group the worktrees found there by
/// `(user, task_id)`. Best-effort: a leaf that isn't a git worktree, or
/// whose owner can't be determined, is silently skipped rather than
/// aborting the whole scan.
pub fn scan_legacy(cfg: &Config) -> Result<Vec<LegacyTask>> {
    let mut by_task: Vec<LegacyTask> = Vec::new();

    for repo_name in cfg.repos.keys() {
        let old_repo_dir = cfg.tasks_dir.join(repo_name);
        let Ok(read_dir) = std::fs::read_dir(&old_repo_dir) else {
            continue;
        };

        for entry in read_dir {
            let Ok(entry) = entry else { continue };
            let leaf_path = entry.path();
            if !leaf_path.is_dir() || !git::is_git_repo(&leaf_path) {
                continue;
            }
            let Some(leaf_name) = leaf_path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };

            let Some((user, task_id)) = identify_owner(&leaf_path, leaf_name) else {
                continue;
            };

            let bucket = match by_task
                .iter_mut()
                .find(|t| t.user == user && t.task_id == task_id)
            {
                Some(t) => t,
                None => {
                    by_task.push(LegacyTask {
                        user: user.clone(),
                        task_id: task_id.clone(),
                        repos: Vec::new(),
                    });
                    by_task.last_mut().expect("just pushed")
                }
            };
            bucket.repos.push(LegacyRepo {
                repo: repo_name.clone(),
                old_path: leaf_path,
            });
        }
    }

    Ok(by_task)
}

/// Recover `(user, task_id)` for a legacy worktree. Prefers the sidecar's
/// `task_id` (paired with the dirname to recover `user`); falls back to
/// parsing the checked-out branch as `user/task_id`.
fn identify_owner(worktree: &std::path::Path, leaf_name: &str) -> Option<(String, String)> {
    if let Some(sidecar) = sidecar::read(worktree).ok().flatten() {
        if let Some(user) =
            paths::split_worktree_dirname_by_known_task(leaf_name, &sidecar.task_id)
        {
            return Some((user, sidecar.task_id));
        }
    }
    let branch = git::current_branch(worktree).ok()?;
    let (user, task_id) = branch.split_once('/')?;
    Some((user.to_string(), task_id.to_string()))
}

/// One-line warning to surface from any command that discovers tasks, when
/// old-format tasks are present. Scan failures are swallowed (`None`) so a
/// transient FS issue never breaks an unrelated command.
pub fn legacy_warning(cfg: &Config) -> Option<String> {
    let legacy = scan_legacy(cfg).ok()?;
    if legacy.is_empty() {
        return None;
    }
    let count = legacy.len();
    let plural = if count == 1 { "task" } else { "tasks" };
    Some(format!(
        "{count} legacy {plural} found under the old layout — run `csw migrate`"
    ))
}

#[derive(Debug, Default)]
pub struct MigrateReport {
    pub migrated: Vec<(String, String)>, // (user, task_id)
    pub failures: Vec<(String, String, anyhow::Error)>, // (user, task_id, error)
}

impl MigrateReport {
    pub fn any_failure(&self) -> bool {
        !self.failures.is_empty()
    }
}

/// Move every repo of a single legacy task to its new task-rooted path.
/// Best-effort per-repo failures don't abort the remaining repos of the
/// same task, but do prevent that task's now-empty old directories from
/// being cleaned up.
pub fn migrate_one(cfg: &Config, task: &LegacyTask, reporter: &dyn Reporter) -> Result<()> {
    let new_root = cfg.task_root(&task.user, &task.task_id);
    std::fs::create_dir_all(&new_root)
        .with_context(|| format!("creating task root {}", new_root.display()))?;

    for legacy_repo in &task.repos {
        let progress = reporter.begin(&legacy_repo.repo, "migrating");
        let new_path = new_root.join(&legacy_repo.repo);
        let canonical = match cfg.repo(&legacy_repo.repo) {
            Some(r) => cfg.canonical_path(r),
            None => {
                progress.err("repo no longer configured");
                continue;
            }
        };
        progress.step(&format!(
            "moving {} -> {}",
            legacy_repo.old_path.display(),
            new_path.display()
        ));
        match git::worktree_move(&canonical, &legacy_repo.old_path, &new_path) {
            Ok(()) => progress.ok("moved"),
            Err(e) => progress.err(&format!("{e}")),
        }
    }

    // Clean up the now-empty old per-repo directories.
    for legacy_repo in &task.repos {
        if let Some(old_repo_dir) = legacy_repo.old_path.parent() {
            let _ = std::fs::remove_dir(old_repo_dir); // no-op if not empty
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RepoConfig;
    use crate::progress::NullReporter;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    struct Scaffold {
        _tmp: TempDir,
        cfg: Config,
        base_dir: PathBuf,
    }

    impl Scaffold {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let base_dir = tmp.path().join("dev");
            let tasks_dir = tmp.path().join("tasks");
            std::fs::create_dir_all(&base_dir).unwrap();
            std::fs::create_dir_all(&tasks_dir).unwrap();
            Self {
                cfg: Config {
                    base_dir: base_dir.clone(),
                    tasks_dir,
                    username: None,
                    default_repos: vec![],
                    agent: None,
                    detached_editor: true,
                    cmux: None,
                    repos: BTreeMap::new(),
                },
                _tmp: tmp,
                base_dir,
            }
        }

        fn add_repo(&mut self, name: &str) {
            let upstream = self._tmp.path().join(format!("{name}-up.git"));
            run(
                None,
                &[
                    "init",
                    "--bare",
                    "--initial-branch=main",
                    upstream.to_str().unwrap(),
                ],
            );
            let canonical = self.base_dir.join(name);
            run(
                None,
                &[
                    "clone",
                    upstream.to_str().unwrap(),
                    canonical.to_str().unwrap(),
                ],
            );
            run(
                Some(&canonical),
                &["config", "user.email", "test@example.com"],
            );
            run(Some(&canonical), &["config", "user.name", "Test"]);
            run(Some(&canonical), &["config", "commit.gpgsign", "false"]);
            std::fs::write(canonical.join("README"), "x").unwrap();
            run(Some(&canonical), &["add", "README"]);
            run(Some(&canonical), &["commit", "-m", "init"]);
            run(Some(&canonical), &["push", "origin", "main"]);
            self.cfg
                .repos
                .insert(name.into(), RepoConfig::new(name, ""));
        }

        /// Create an old-format worktree by hand: `<tasks_dir>/<repo>/<user>-<task_id>`.
        fn seed_legacy_worktree(&self, repo: &str, user: &str, task_id: &str, with_sidecar: bool) {
            let canonical = self.base_dir.join(repo);
            let old_dir = self.cfg.tasks_dir.join(repo);
            std::fs::create_dir_all(&old_dir).unwrap();
            let leaf = old_dir.join(format!("{user}-{task_id}"));
            let branch = format!("{user}/{task_id}");
            run(
                Some(&canonical),
                &[
                    "worktree",
                    "add",
                    "--no-track",
                    "-b",
                    &branch,
                    leaf.to_str().unwrap(),
                    "main",
                ],
            );
            if with_sidecar {
                let sc = sidecar::Sidecar::new(task_id, branch, None);
                sidecar::write(&leaf, &sc).unwrap();
            }
        }
    }

    fn run(dir: Option<&Path>, args: &[&str]) {
        let mut c = Command::new("git");
        if let Some(d) = dir {
            c.current_dir(d);
        }
        c.env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        let out = c.args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn scan_finds_nothing_when_no_legacy_tasks() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        let found = scan_legacy(&s.cfg).unwrap();
        assert!(found.is_empty());
    }

    #[test]
    fn scan_finds_legacy_task_via_sidecar() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.seed_legacy_worktree("frontend", "alice", "PROJ-1", true);

        let found = scan_legacy(&s.cfg).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].user, "alice");
        assert_eq!(found[0].task_id, "PROJ-1");
        assert_eq!(found[0].repos.len(), 1);
        assert_eq!(found[0].repos[0].repo, "frontend");
    }

    #[test]
    fn scan_falls_back_to_branch_when_sidecar_missing() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.seed_legacy_worktree("frontend", "bob", "PROJ-2", false);

        let found = scan_legacy(&s.cfg).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].user, "bob");
        assert_eq!(found[0].task_id, "PROJ-2");
    }

    #[test]
    fn scan_groups_multi_repo_task() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.add_repo("backend");
        s.seed_legacy_worktree("frontend", "alice", "PROJ-1", true);
        s.seed_legacy_worktree("backend", "alice", "PROJ-1", true);

        let found = scan_legacy(&s.cfg).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].repos.len(), 2);
    }

    #[test]
    fn legacy_warning_none_when_clean() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        assert_eq!(legacy_warning(&s.cfg), None);
    }

    #[test]
    fn legacy_warning_reports_count() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.seed_legacy_worktree("frontend", "alice", "PROJ-1", true);
        let warning = legacy_warning(&s.cfg).unwrap();
        assert!(warning.contains("1 legacy task"));
        assert!(warning.contains("csw migrate"));
    }

    #[test]
    fn migrate_one_moves_worktree_to_new_layout() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.seed_legacy_worktree("frontend", "alice", "PROJ-1", true);

        let found = scan_legacy(&s.cfg).unwrap();
        migrate_one(&s.cfg, &found[0], &NullReporter).unwrap();

        let new_path = s.cfg.task_root("alice", "PROJ-1").join("frontend");
        assert!(new_path.exists());
        assert!(git::is_git_repo(&new_path));
        assert_eq!(git::current_branch(&new_path).unwrap(), "alice/PROJ-1");
        assert!(sidecar::read(&new_path).unwrap().is_some());

        // Old per-repo directory is gone since it's now empty.
        assert!(!s.cfg.tasks_dir.join("frontend").exists());
        // Nothing left to find at the old layout.
        assert!(scan_legacy(&s.cfg).unwrap().is_empty());
    }

    #[test]
    fn migrate_one_leaves_other_tasks_in_old_repo_dir_untouched() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.seed_legacy_worktree("frontend", "alice", "PROJ-1", true);
        s.seed_legacy_worktree("frontend", "alice", "PROJ-2", true);

        let found = scan_legacy(&s.cfg).unwrap();
        let proj1 = found.iter().find(|t| t.task_id == "PROJ-1").unwrap();
        migrate_one(&s.cfg, proj1, &NullReporter).unwrap();

        // The old per-repo dir still exists (PROJ-2 still lives there).
        assert!(s.cfg.tasks_dir.join("frontend").exists());
        let remaining = scan_legacy(&s.cfg).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].task_id, "PROJ-2");
    }
}
