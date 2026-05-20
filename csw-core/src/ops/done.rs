//! `csw done` orchestration.
//!
//! Two-phase API so the CLI can prompt for confirmation between phases:
//! * [`plan`] inspects every worktree of the task and returns a [`DonePlan`]
//!   with blocking issues, soft warnings, and the canonical actions that
//!   would be taken on execute. No side effects.
//! * [`execute`] actually performs the destruction using a previously
//!   computed plan. It still re-checks blocking issues unless `force` is
//!   set, since the working tree could have changed between calls.

use crate::Config;
use crate::cmux::{self, CmuxOutcome};
use crate::errors::CswError;
use crate::progress::Reporter;
use crate::{git, paths};
use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct DoneRequest {
    pub task_id: String,
    pub user: String,
    pub force: bool,
    pub keep_branch: bool,
    /// Leave the matching CMux workspace open. Default behaviour is to
    /// close `csw/<task-id>` after destruction so the sidebar doesn't end
    /// up with an entry pointing at deleted directories.
    pub keep_workspace: bool,
}

#[derive(Debug, Clone)]
pub struct WorktreeState {
    pub repo: String,
    pub canonical: PathBuf,
    pub worktree_path: PathBuf,
    pub branch: String,
    pub base_branch: String,
}

#[derive(Debug, Clone)]
pub enum BlockingIssue {
    Dirty {
        repo: String,
        worktree_path: PathBuf,
        files: Vec<String>,
    },
    Unpushed {
        repo: String,
        worktree_path: PathBuf,
        ahead: usize,
    },
    NoUpstream {
        repo: String,
        worktree_path: PathBuf,
    },
}

impl BlockingIssue {
    pub fn repo(&self) -> &str {
        match self {
            BlockingIssue::Dirty { repo, .. }
            | BlockingIssue::Unpushed { repo, .. }
            | BlockingIssue::NoUpstream { repo, .. } => repo,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UnmergedWarning {
    pub repo: String,
    pub branch: String,
    pub base: String,
    pub worktree_path: PathBuf,
}

#[derive(Debug)]
pub struct DonePlan {
    pub task_id: String,
    pub user: String,
    pub worktrees: Vec<WorktreeState>,
    pub blocking: Vec<BlockingIssue>,
    pub warnings: Vec<UnmergedWarning>,
}

#[derive(Debug, Default)]
pub struct DoneReport {
    pub deleted_worktrees: Vec<PathBuf>,
    pub deleted_branches: Vec<(String, String)>, // (repo, branch)
    pub failures: Vec<(String, anyhow::Error)>,
    /// Outcome of attempting to close the matching CMux workspace. Never
    /// influences exit code.
    pub cmux: CmuxOutcome,
}

impl DoneReport {
    pub fn any_failure(&self) -> bool {
        !self.failures.is_empty()
    }
}

/// Inspect every existing worktree for a task and produce a plan describing
/// what would happen on execute.
pub fn plan(cfg: &Config, request: &DoneRequest) -> Result<DonePlan> {
    let worktrees = find_worktrees(cfg, &request.user, &request.task_id)?;
    if worktrees.is_empty() {
        return Err(anyhow!(
            "no worktrees found for task {}/{}",
            request.user,
            request.task_id
        ));
    }

    let mut blocking = Vec::new();
    let mut warnings = Vec::new();

    for state in &worktrees {
        // Dirty?
        let porcelain =
            git::status_porcelain(&state.worktree_path).context("checking worktree status")?;
        if !porcelain.is_empty() {
            blocking.push(BlockingIssue::Dirty {
                repo: state.repo.clone(),
                worktree_path: state.worktree_path.clone(),
                files: porcelain.lines().take(20).map(str::to_string).collect(),
            });
            // We still want to report unpushed/unmerged status, so keep going.
        }

        // Unpushed?
        match git::ahead_behind(&state.worktree_path, &state.branch)? {
            None => blocking.push(BlockingIssue::NoUpstream {
                repo: state.repo.clone(),
                worktree_path: state.worktree_path.clone(),
            }),
            Some((ahead, _behind)) if ahead > 0 => blocking.push(BlockingIssue::Unpushed {
                repo: state.repo.clone(),
                worktree_path: state.worktree_path.clone(),
                ahead,
            }),
            Some(_) => {
                // Pushed and up-to-date — check whether merged into base.
                let base_remote = format!("origin/{}", state.base_branch);
                if !git::is_merged(&state.worktree_path, &state.branch, &base_remote)? {
                    warnings.push(UnmergedWarning {
                        repo: state.repo.clone(),
                        branch: state.branch.clone(),
                        base: state.base_branch.clone(),
                        worktree_path: state.worktree_path.clone(),
                    });
                }
            }
        }
    }

    Ok(DonePlan {
        task_id: request.task_id.clone(),
        user: request.user.clone(),
        worktrees,
        blocking,
        warnings,
    })
}

/// Carry out the deletion. The caller must already have:
/// * checked `plan.blocking.is_empty()` (or set `request.force`),
/// * confirmed any `plan.warnings` with the user (or set `request.force`).
pub fn execute(
    cfg: &Config,
    plan: &DonePlan,
    request: &DoneRequest,
    reporter: &dyn Reporter,
) -> Result<DoneReport, CswError> {
    if !plan.blocking.is_empty() && !request.force {
        return Err(CswError::GitCommandFailed(
            "execute() called with unresolved blocking issues".into(),
        ));
    }

    let mut report = DoneReport::default();

    for state in &plan.worktrees {
        let progress = reporter.begin(&state.repo, "deleting");
        progress.step(&format!("removing {}", state.worktree_path.display()));
        if let Err(e) = git::worktree_remove(&state.canonical, &state.worktree_path, request.force)
        {
            progress.err(&format!("worktree remove failed: {e}"));
            report.failures.push((state.repo.clone(), e));
            continue;
        }
        report.deleted_worktrees.push(state.worktree_path.clone());

        if request.keep_branch {
            progress.ok("worktree removed");
            continue;
        }
        // Delete the branch from the canonical's `.git/` if it lives there.
        let Some(repo) = cfg.repo(&state.repo) else {
            progress.ok("worktree removed");
            continue;
        };
        let canonical = cfg.canonical_path(repo);
        let branch_present = git::branch_exists_local(&canonical, &state.branch).unwrap_or(false);
        if !branch_present {
            progress.ok("worktree removed");
            continue;
        }

        // Only delete a still-present branch if it's merged or the user
        // passed --force.
        let base_remote = format!("origin/{}", state.base_branch);
        let merged = git::is_merged(&canonical, &state.branch, &base_remote).unwrap_or(false);
        if !merged && !request.force {
            progress.ok("worktree removed (branch left in place)");
            continue;
        }
        progress.step(&format!("removing branch {}", state.branch));
        match git::delete_branch(&canonical, &state.branch, request.force) {
            Ok(()) => {
                progress.ok("worktree and branch removed");
                report
                    .deleted_branches
                    .push((state.repo.clone(), state.branch.clone()));
            }
            Err(e) => {
                progress.err(&format!("branch delete failed: {e}"));
                report.failures.push((state.repo.clone(), e));
            }
        }
    }

    report.cmux = finalize_cmux(cfg, request);

    Ok(report)
}

/// Task-level CMux teardown. Symmetric with `start`: only fires when csw is
/// running inside CMux, the global config doesn't disable the integration,
/// and the user hasn't asked to keep the workspace.
fn finalize_cmux(cfg: &Config, request: &DoneRequest) -> CmuxOutcome {
    if request.keep_workspace || !cfg.cmux_enabled() || !cmux::detect() {
        return CmuxOutcome::NotApplicable;
    }
    cmux::close(&request.task_id)
}

fn find_worktrees(cfg: &Config, user: &str, task_id: &str) -> Result<Vec<WorktreeState>> {
    let mut out = Vec::new();
    for (name, repo) in &cfg.repos {
        let worktree = paths::worktree_path(cfg, name, user, task_id);
        if !worktree.exists() {
            continue;
        }
        if !git::is_git_repo(&worktree) {
            return Err(anyhow!(
                "{}: worktree directory exists but is not a git repository",
                worktree.display()
            ));
        }
        // Trust whatever branch is checked out. The directory naming
        // identifies the task; the branch may have been overridden at
        // start-time with `--branch <repo>=...`.
        let actual_branch = git::current_branch(&worktree)
            .with_context(|| format!("reading current branch of {}", worktree.display()))?;
        let base_branch = match repo.base_branch.clone() {
            Some(b) => b,
            None => git::resolve_origin_head(&worktree)
                .with_context(|| format!("resolving origin/HEAD for {}", worktree.display()))?,
        };
        out.push(WorktreeState {
            repo: name.clone(),
            canonical: cfg.canonical_path(repo),
            worktree_path: worktree,
            branch: actual_branch,
            base_branch,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::start::{self, StartRequest};
    use crate::progress::NullReporter;
    use crate::{RepoConfig, sidecar};
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
            let report = start::start(&self.cfg, req, &NullReporter).unwrap();
            assert!(report.failures.is_empty(), "{:?}", report.failures);
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

    fn req(task: &str) -> DoneRequest {
        DoneRequest {
            task_id: task.into(),
            user: "alice".into(),
            force: false,
            keep_branch: false,
            keep_workspace: true,
        }
    }

    #[test]
    fn plan_errors_when_no_worktrees_exist() {
        let s = Scaffold::new();
        let err = plan(&s.cfg, &req("PROJ-1")).unwrap_err();
        assert!(format!("{err}").contains("no worktrees found"));
    }

    #[test]
    fn plan_dirty_worktree_blocks() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"]);
        std::fs::write(s.worktree("frontend", "PROJ-1").join("dirty"), "x").unwrap();

        let p = plan(&s.cfg, &req("PROJ-1")).unwrap();
        assert!(matches!(p.blocking[0], BlockingIssue::Dirty { .. }));
    }

    #[test]
    fn plan_unpushed_commit_blocks() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"]);
        let wt = s.worktree("frontend", "PROJ-1");
        run(Some(&wt), &["push", "-u", "origin", "alice/PROJ-1"]);
        std::fs::write(wt.join("local-only"), "x").unwrap();
        run(Some(&wt), &["add", "local-only"]);
        run(Some(&wt), &["commit", "-m", "ahead"]);

        let p = plan(&s.cfg, &req("PROJ-1")).unwrap();
        assert!(matches!(
            p.blocking[0],
            BlockingIssue::Unpushed { ahead: 1, .. }
        ));
    }

    #[test]
    fn plan_no_upstream_blocks() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"]);
        let p = plan(&s.cfg, &req("PROJ-1")).unwrap();
        assert!(matches!(p.blocking[0], BlockingIssue::NoUpstream { .. }));
    }

    #[test]
    fn plan_pushed_unmerged_warns_but_does_not_block() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"]);
        let wt = s.worktree("frontend", "PROJ-1");
        std::fs::write(wt.join("work"), "x").unwrap();
        run(Some(&wt), &["add", "work"]);
        run(Some(&wt), &["commit", "-m", "wip"]);
        run(Some(&wt), &["push", "-u", "origin", "alice/PROJ-1"]);

        let p = plan(&s.cfg, &req("PROJ-1")).unwrap();
        assert!(p.blocking.is_empty(), "unexpected blocks: {:?}", p.blocking);
        assert_eq!(p.warnings.len(), 1);
        assert_eq!(p.warnings[0].branch, "alice/PROJ-1");
    }

    #[test]
    fn plan_no_warning_when_merged_into_base() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"]);
        let wt = s.worktree("frontend", "PROJ-1");
        std::fs::write(wt.join("work"), "x").unwrap();
        run(Some(&wt), &["add", "work"]);
        run(Some(&wt), &["commit", "-m", "wip"]);
        run(Some(&wt), &["push", "-u", "origin", "alice/PROJ-1"]);
        run(Some(&wt), &["push", "origin", "alice/PROJ-1:main"]);
        run(Some(&wt), &["fetch", "origin"]);

        let p = plan(&s.cfg, &req("PROJ-1")).unwrap();
        assert!(p.blocking.is_empty(), "{:?}", p.blocking);
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    }

    #[test]
    fn execute_removes_worktree_directory() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"]);
        let wt = s.worktree("frontend", "PROJ-1");
        run(Some(&wt), &["push", "-u", "origin", "alice/PROJ-1"]);

        let p = plan(&s.cfg, &req("PROJ-1")).unwrap();
        assert!(p.blocking.is_empty(), "{:?}", p.blocking);

        let report = execute(&s.cfg, &p, &req("PROJ-1"), &NullReporter).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.deleted_worktrees, vec![wt.clone()]);
        assert!(!wt.exists());
    }

    #[test]
    fn execute_force_proceeds_through_blocking_issues() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"]);
        let wt = s.worktree("frontend", "PROJ-1");
        std::fs::write(wt.join("dirty"), "x").unwrap();

        let p = plan(&s.cfg, &req("PROJ-1")).unwrap();
        assert!(!p.blocking.is_empty());

        let mut force = req("PROJ-1");
        force.force = true;
        let report = execute(&s.cfg, &p, &force, &NullReporter).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(!wt.exists());
    }

    #[test]
    fn execute_refuses_when_blocking_and_not_forced() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"]);
        std::fs::write(s.worktree("frontend", "PROJ-1").join("dirty"), "x").unwrap();

        let p = plan(&s.cfg, &req("PROJ-1")).unwrap();
        let err = execute(&s.cfg, &p, &req("PROJ-1"), &NullReporter).unwrap_err();
        assert!(matches!(err, CswError::GitCommandFailed(_)));
    }

    #[test]
    fn execute_keep_branch_skips_branch_deletion() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"]);
        let wt = s.worktree("frontend", "PROJ-1");
        // Push so it's not blocked by NoUpstream, and merge to avoid warning.
        run(Some(&wt), &["push", "-u", "origin", "alice/PROJ-1"]);
        run(Some(&wt), &["push", "origin", "alice/PROJ-1:main"]);
        run(Some(&wt), &["fetch", "origin"]);

        let p = plan(&s.cfg, &req("PROJ-1")).unwrap();
        let mut keep = req("PROJ-1");
        keep.keep_branch = true;
        execute(&s.cfg, &p, &keep, &NullReporter).unwrap();

        let canonical = s.base_dir.join("frontend");
        assert!(git::branch_exists_local(&canonical, "alice/PROJ-1").unwrap());
    }

    #[test]
    fn sidecar_is_gone_after_execute() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"]);
        let wt = s.worktree("frontend", "PROJ-1");
        run(Some(&wt), &["push", "-u", "origin", "alice/PROJ-1"]);

        // Sanity: sidecar exists before.
        assert!(sidecar::read(&wt).unwrap().is_some());

        let p = plan(&s.cfg, &req("PROJ-1")).unwrap();
        execute(&s.cfg, &p, &req("PROJ-1"), &NullReporter).unwrap();
        assert!(!wt.exists());
    }
}
