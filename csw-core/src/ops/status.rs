//! `csw status` — read-only inspection of every worktree belonging to a task.

use crate::Config;
use crate::{git, paths};
use anyhow::{Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct StatusRequest {
    pub task_id: String,
    pub user: String,
}

#[derive(Debug, Clone)]
pub struct WorktreeStatus {
    pub repo: String,
    pub worktree_path: PathBuf,
    pub branch: String,
    pub base_branch: String,
    /// Up to ~20 lines of `git status --porcelain`. Empty == clean.
    pub dirty: Vec<String>,
    /// `Some((ahead, behind))` against the upstream, or `None` if no upstream.
    pub ahead_behind: Option<(usize, usize)>,
    /// Whether the branch is fully merged into `origin/<base_branch>`.
    pub merged_into_base: bool,
}

#[derive(Debug)]
pub struct StatusReport {
    pub task_id: String,
    pub user: String,
    pub worktrees: Vec<WorktreeStatus>,
}

pub fn status(cfg: &Config, request: &StatusRequest) -> Result<StatusReport> {
    let mut worktrees = Vec::new();

    for (name, repo) in &cfg.repos {
        let worktree = paths::worktree_path(cfg, name, &request.user, &request.task_id);
        if !worktree.exists() || !git::is_git_repo(&worktree) {
            continue;
        }
        // Use whatever branch is actually checked out. The directory naming
        // ties the worktree to a task; the branch may have been overridden
        // at start-time and isn't always `<user>/<task_id>`.
        let branch = git::current_branch(&worktree)
            .with_context(|| format!("reading branch of {}", worktree.display()))?;
        let base_branch = match repo.base_branch.clone() {
            Some(b) => b,
            None => git::resolve_origin_head(&worktree).unwrap_or_else(|_| "main".into()),
        };
        let porcelain = git::status_porcelain(&worktree)?;
        let dirty: Vec<String> = porcelain.lines().take(20).map(str::to_string).collect();
        let ab = git::ahead_behind(&worktree, &branch)?;
        let merged =
            git::is_merged(&worktree, &branch, &format!("origin/{base_branch}")).unwrap_or(false);

        worktrees.push(WorktreeStatus {
            repo: name.clone(),
            worktree_path: worktree,
            branch,
            base_branch,
            dirty,
            ahead_behind: ab,
            merged_into_base: merged,
        });
    }

    Ok(StatusReport {
        task_id: request.task_id.clone(),
        user: request.user.clone(),
        worktrees,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RepoConfig;
    use crate::ops::start::{self, StartRequest};
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

        fn start_task(&self, task: &str, repos: &[&str]) {
            let req = StartRequest {
                task_id: task.into(),
                user: "alice".into(),
                repos: repos.iter().map(|s| (*s).into()).collect(),
                title: None,
                spawn_editor: false,
                skip_hooks: false,
                no_cmux: true,
                force_new_workspace: false,
                branch_overrides: std::collections::BTreeMap::new(),
            };
            start::start(&self.cfg, req, &NullReporter).unwrap();
        }

        fn worktree(&self, repo: &str, task: &str) -> PathBuf {
            self.cfg
                .repo_tasks_dir(repo)
                .join(paths::worktree_dirname("alice", task))
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

    fn req(task: &str) -> StatusRequest {
        StatusRequest {
            task_id: task.into(),
            user: "alice".into(),
        }
    }

    #[test]
    fn status_empty_when_no_worktrees_exist() {
        let s = Scaffold::new();
        let r = status(&s.cfg, &req("PROJ-1")).unwrap();
        assert!(r.worktrees.is_empty());
    }

    #[test]
    fn status_reports_clean_worktree() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"]);
        let r = status(&s.cfg, &req("PROJ-1")).unwrap();
        assert_eq!(r.worktrees.len(), 1);
        assert!(r.worktrees[0].dirty.is_empty());
        assert_eq!(r.worktrees[0].ahead_behind, None);
        assert!(
            r.worktrees[0].merged_into_base,
            "freshly branched is merged"
        );
    }

    #[test]
    fn status_reports_dirty_files() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"]);
        std::fs::write(s.worktree("frontend", "PROJ-1").join("dirty"), "x").unwrap();

        let r = status(&s.cfg, &req("PROJ-1")).unwrap();
        assert!(!r.worktrees[0].dirty.is_empty());
    }

    #[test]
    fn status_reports_ahead_after_local_commit() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"]);
        let wt = s.worktree("frontend", "PROJ-1");
        run(Some(&wt), &["push", "-u", "origin", "alice/PROJ-1"]);

        std::fs::write(wt.join("work"), "x").unwrap();
        run(Some(&wt), &["add", "work"]);
        run(Some(&wt), &["commit", "-m", "wip"]);

        let r = status(&s.cfg, &req("PROJ-1")).unwrap();
        assert_eq!(r.worktrees[0].ahead_behind, Some((1, 0)));
        assert!(!r.worktrees[0].merged_into_base);
    }

    #[test]
    fn status_reports_actual_branch_even_when_non_default() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"]);
        let wt = s.worktree("frontend", "PROJ-1");
        run(Some(&wt), &["checkout", "-b", "feature/legacy"]);

        let r = status(&s.cfg, &req("PROJ-1")).unwrap();
        assert_eq!(r.worktrees.len(), 1);
        assert_eq!(r.worktrees[0].branch, "feature/legacy");
    }

    #[test]
    fn status_handles_multi_repo_tasks() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.add_repo("backend");
        s.start_task("PROJ-1", &["frontend", "backend"]);
        let r = status(&s.cfg, &req("PROJ-1")).unwrap();
        assert_eq!(r.worktrees.len(), 2);
    }
}
