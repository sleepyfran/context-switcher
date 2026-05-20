//! `csw start` orchestration.
//!
//! Two phases:
//!  1. Pre-flight every selected repo (no side effects). Any failure aborts
//!     the whole operation with [`Err`].
//!  2. Best-effort execution per repo. Per-repo failures are recorded in the
//!     returned [`StartReport`] but don't abort siblings — the caller decides
//!     the exit code.

use crate::cmux::{self, BuildOptions, CmuxOutcome, Contributor};
use crate::errors::CswError;
use crate::hooks::{self, HookContext};
use crate::progress::{RepoProgress, Reporter};
use crate::{Config, RepoConfig, editor, git, paths, sidecar, sidecar::Sidecar};
use anyhow::{Context, Result, anyhow};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub task_id: String,
    pub user: String,
    pub repos: Vec<String>,
    pub title: Option<String>,
    pub spawn_editor: bool,
    pub skip_hooks: bool,
    /// Suppress the CMux integration for this run, even when csw is invoked
    /// from inside a CMux surface and the configuration enables it.
    pub no_cmux: bool,
    /// Skip the in-place adoption of a simple current CMux workspace,
    /// forcing a brand-new workspace to be created instead. Overrides the
    /// `cmux.replace_simple_workspace` config knob for this invocation.
    pub force_new_workspace: bool,
    /// Per-repo branch overrides. When a repo's name appears here, the
    /// effective branch for that repo's worktree is the override instead of
    /// the default `<user>/<task_id>` form.
    pub branch_overrides: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartAction {
    Created,
    Resumed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorStatus {
    Spawned,
    Skipped,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct StartSuccess {
    pub repo: String,
    pub worktree_path: PathBuf,
    pub branch: String,
    pub action: StartAction,
    pub editor: EditorStatus,
}

#[derive(Debug)]
pub struct StartReport {
    pub task_id: String,
    pub user: String,
    pub successes: Vec<StartSuccess>,
    pub failures: Vec<(String, anyhow::Error)>,
    /// Outcome of the CMux integration, if it ran. Never influences the
    /// exit code — purely informational for the renderer.
    pub cmux: CmuxOutcome,
}

impl StartReport {
    pub fn any_failure(&self) -> bool {
        !self.failures.is_empty()
    }
}

/// Split a user-provided task argument into `(user, task_id)`. If the input
/// contains a `/` it's treated as already in `<user>/<task-id>` form;
/// otherwise the supplied `default_user` is used.
pub fn parse_task_input(input: &str, default_user: &str) -> (String, String) {
    if let Some((u, t)) = input.split_once('/') {
        (u.to_string(), t.to_string())
    } else {
        (default_user.to_string(), input.to_string())
    }
}

#[derive(Debug)]
struct Plan {
    repo_name: String,
    repo: RepoConfig,
    canonical: PathBuf,
    worktree: PathBuf,
    branch: String,
    action: StartAction,
}

pub fn start(cfg: &Config, request: StartRequest, reporter: &dyn Reporter) -> Result<StartReport> {
    let plans = preflight(cfg, &request)?;
    let mut report = StartReport {
        task_id: request.task_id.clone(),
        user: request.user.clone(),
        successes: Vec::new(),
        failures: Vec::new(),
        cmux: CmuxOutcome::NotApplicable,
    };

    for plan in plans {
        let action_label = match plan.action {
            StartAction::Created => "creating",
            StartAction::Resumed => "resuming",
        };
        let progress = reporter.begin(&plan.repo_name, action_label);
        match execute(&request, &plan, progress.as_ref()) {
            Ok(success) => {
                progress.ok(&format!(
                    "{} on {}",
                    match success.action {
                        StartAction::Created => "created",
                        StartAction::Resumed => "resumed",
                    },
                    success.branch
                ));
                report.successes.push(success);
            }
            Err(e) => {
                progress.err(&format!("{e}"));
                report.failures.push((plan.repo_name.clone(), e));
            }
        }
    }

    report.cmux = finalize_cmux(cfg, &request, &report.successes);

    Ok(report)
}

/// Task-level CMux finalization. Runs only if csw is inside a CMux surface,
/// the global config doesn't disable the integration, and `--no-cmux` wasn't
/// passed. Builds (or reuses) `csw/<task-id>` from the participating repos.
/// Failure is always soft — the returned outcome never influences exit code.
fn finalize_cmux(cfg: &Config, request: &StartRequest, successes: &[StartSuccess]) -> CmuxOutcome {
    if request.no_cmux || !cfg.cmux_enabled() || !cmux::detect() {
        return CmuxOutcome::NotApplicable;
    }
    let contributors = cmux_contributors(cfg, successes);
    // Prefer the title from this invocation. If absent (e.g. a bare resume
    // a day later), recover the title that was stored in any of the task's
    // sidecars at create time so the workspace label stays stable.
    let title = request.title.clone().or_else(|| sidecar_title(successes));
    let options = BuildOptions {
        replace_simple_workspace: cfg.cmux_replace_simple_workspace(),
        force_new_workspace: request.force_new_workspace,
    };
    cmux::setup(&request.task_id, title.as_deref(), &contributors, options)
}

/// First non-empty title found in any success's sidecar, if any. Sidecars
/// may be missing or corrupt — those are silent skips per [`sidecar::read`].
fn sidecar_title(successes: &[StartSuccess]) -> Option<String> {
    successes
        .iter()
        .filter_map(|s| sidecar::read(&s.worktree_path).ok().flatten())
        .filter_map(|sc: Sidecar| sc.title)
        .find(|t| !t.trim().is_empty())
}

/// Filter successful repos down to those that opt in to CMux participation
/// — i.e. have a `cmux` block with at least one pane configured. Extracted
/// from [`finalize_cmux`] so it can be unit-tested without env-var games.
pub(crate) fn cmux_contributors(cfg: &Config, successes: &[StartSuccess]) -> Vec<Contributor> {
    successes
        .iter()
        .filter_map(|s| {
            let repo = cfg.repo(&s.repo)?;
            let layout = repo.cmux.as_ref()?;
            if !layout.participates() {
                return None;
            }
            Some(Contributor {
                repo: s.repo.clone(),
                worktree_path: s.worktree_path.clone(),
                layout: layout.clone(),
            })
        })
        .collect()
}

/// Validate every repo before any side effects. Aggregates errors so the
/// user sees all problems at once.
fn preflight(cfg: &Config, request: &StartRequest) -> Result<Vec<Plan>> {
    // Validate that every override targets a selected repo. Mistyped flags
    // are easy to make and silently dropping them would be confusing.
    for repo_name in request.branch_overrides.keys() {
        if !request.repos.contains(repo_name) {
            return Err(anyhow!(
                "--branch override for '{repo_name}' but that repo isn't in the selected set"
            ));
        }
    }

    let mut plans = Vec::with_capacity(request.repos.len());
    let mut errors: Vec<String> = Vec::new();
    let default_branch = paths::branch_name(&request.user, &request.task_id);

    for name in &request.repos {
        let effective_branch = request
            .branch_overrides
            .get(name)
            .cloned()
            .unwrap_or_else(|| default_branch.clone());
        match plan_for_repo(
            cfg,
            name,
            &request.user,
            &request.task_id,
            &effective_branch,
        ) {
            Ok(plan) => plans.push(plan),
            Err(e) => errors.push(format!("{name}: {e}")),
        }
    }

    if !errors.is_empty() {
        return Err(anyhow!("pre-flight failed:\n  - {}", errors.join("\n  - ")));
    }
    Ok(plans)
}

fn plan_for_repo(
    cfg: &Config,
    repo_name: &str,
    user: &str,
    task_id: &str,
    branch: &str,
) -> Result<Plan> {
    let repo = cfg
        .repo(repo_name)
        .ok_or_else(|| CswError::UnknownRepo(repo_name.to_string()))?
        .clone();
    let canonical = cfg.canonical_path(&repo);
    if !git::is_git_repo(&canonical) {
        return Err(CswError::CanonicalMissing(canonical).into());
    }

    let worktree = paths::worktree_path(cfg, repo_name, user, task_id);
    let action = if worktree.exists() {
        if !git::is_git_repo(&worktree) {
            return Err(CswError::NotAGitRepo { path: worktree }.into());
        }
        let actual = git::current_branch(&worktree)?;
        if actual != branch {
            return Err(CswError::WrongBranch {
                path: worktree,
                actual,
                expected: branch.to_string(),
            }
            .into());
        }
        StartAction::Resumed
    } else {
        StartAction::Created
    };

    Ok(Plan {
        repo_name: repo_name.to_string(),
        repo,
        canonical,
        worktree,
        branch: branch.to_string(),
        action,
    })
}

fn execute(
    request: &StartRequest,
    plan: &Plan,
    progress: &dyn RepoProgress,
) -> Result<StartSuccess> {
    if plan.action == StartAction::Created {
        create_worktree(plan, progress)?;
        progress.step("writing sidecar");
        write_sidecar(plan, request)?;
        if !request.skip_hooks && !plan.repo.post_create.is_empty() {
            run_post_create_hooks(request, plan, progress)?;
        }
    }
    let editor = launch_editor(request, &plan.repo, &plan.worktree, progress);
    Ok(StartSuccess {
        repo: plan.repo_name.clone(),
        worktree_path: plan.worktree.clone(),
        branch: plan.branch.clone(),
        action: plan.action.clone(),
        editor,
    })
}

fn run_post_create_hooks(
    request: &StartRequest,
    plan: &Plan,
    progress: &dyn RepoProgress,
) -> Result<()> {
    let ctx = HookContext {
        repo: &plan.repo_name,
        worktree: &plan.worktree,
        canonical: &plan.canonical,
        task_id: &request.task_id,
        branch: &plan.branch,
        user: &request.user,
    };
    hooks::run_hooks(&plan.repo.post_create, &ctx, progress).map_err(anyhow::Error::from)
}

fn create_worktree(plan: &Plan, progress: &dyn RepoProgress) -> Result<()> {
    // 1. Make the canonical's view of reality consistent: prune any stale
    //    worktree registrations (left behind by `rm -rf` etc.), then fetch
    //    so subsequent branch lookups land on current remote state.
    progress.step("pruning stale worktrees");
    git::worktree_prune(&plan.canonical).context("pruning worktree registrations")?;

    progress.step("fetching from origin");
    git::fetch(&plan.canonical, "origin").context("fetching origin")?;

    // 2. Materialise the worktree, choosing the highest-priority source for
    //    the branch.
    progress.step(&format!("adding worktree at {}", plan.worktree.display()));
    if let Some(parent) = plan.worktree.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating tasks dir {}", parent.display()))?;
    }
    add_worktree_for_plan(plan)?;
    Ok(())
}

fn add_worktree_for_plan(plan: &Plan) -> Result<()> {
    if git::branch_exists_local(&plan.canonical, &plan.branch)? {
        return git::worktree_add_existing_local(&plan.canonical, &plan.worktree, &plan.branch)
            .with_context(|| format!("adding worktree on existing local branch {}", plan.branch));
    }
    if git::branch_exists_remote(&plan.canonical, "origin", &plan.branch)? {
        return git::worktree_add_tracking_remote(&plan.canonical, &plan.worktree, &plan.branch)
            .with_context(|| format!("adding worktree tracking origin/{}", plan.branch));
    }
    let base_short = match &plan.repo.base_branch {
        Some(b) => b.clone(),
        None => git::resolve_origin_head(&plan.canonical)
            .context("resolving origin/HEAD as base branch")?,
    };
    let base = format!("origin/{base_short}");
    git::worktree_add_new_branch(&plan.canonical, &plan.worktree, &plan.branch, &base)
        .with_context(|| format!("adding worktree on new branch off {base}"))
}

fn write_sidecar(plan: &Plan, request: &StartRequest) -> Result<()> {
    let s = sidecar::Sidecar::new(
        request.task_id.clone(),
        plan.branch.clone(),
        request.title.clone(),
    );
    sidecar::write(&plan.worktree, &s).context("writing sidecar metadata")
}

fn launch_editor(
    request: &StartRequest,
    repo: &RepoConfig,
    worktree: &Path,
    progress: &dyn RepoProgress,
) -> EditorStatus {
    if !request.spawn_editor || repo.editor.is_empty() {
        return EditorStatus::Skipped;
    }
    progress.step("launching editor");
    match editor::spawn(&repo.editor, worktree) {
        Ok(()) => EditorStatus::Spawned,
        Err(e) => EditorStatus::Failed(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NullReporter;
    use std::collections::BTreeMap;
    use std::process::Command;
    use tempfile::TempDir;

    /// A temporary scaffold: a config, a base_dir for canonicals, a tasks_dir
    /// for worktrees, and one or more canonical clones inside base_dir.
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
            let cfg = Config {
                base_dir: base_dir.clone(),
                tasks_dir,
                username: None,
                default_repos: vec![],
                cmux: None,
                repos: BTreeMap::new(),
            };
            Self {
                _tmp: tmp,
                cfg,
                base_dir,
            }
        }

        /// Set up an "upstream" bare repo plus a canonical clone of it. The
        /// canonical clone has its origin pointed at the upstream so that
        /// fetches and remote-branch checks behave realistically.
        fn add_repo(&mut self, name: &str, editor: &str) {
            let upstream = self._tmp.path().join(format!("{name}-upstream.git"));
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
            git_setup_identity(&canonical);
            commit_file(&canonical, "README", "hi");
            run(Some(&canonical), &["push", "origin", "main"]);

            self.cfg
                .repos
                .insert(name.into(), RepoConfig::new(name, editor));
        }

        fn worktree_path(&self, repo: &str, user: &str, task: &str) -> PathBuf {
            self.cfg
                .repo_tasks_dir(repo)
                .join(paths::worktree_dirname(user, task))
        }
    }

    fn run(dir: Option<&Path>, args: &[&str]) {
        let mut c = Command::new("git");
        if let Some(d) = dir {
            c.current_dir(d);
        }
        let out = c.args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_setup_identity(repo: &Path) {
        run(Some(repo), &["config", "user.email", "test@example.com"]);
        run(Some(repo), &["config", "user.name", "Test User"]);
        run(Some(repo), &["config", "commit.gpgsign", "false"]);
    }

    fn commit_file(repo: &Path, name: &str, contents: &str) {
        std::fs::write(repo.join(name), contents).unwrap();
        run(Some(repo), &["add", name]);
        run(Some(repo), &["commit", "-m", &format!("add {name}")]);
    }

    fn req(task: &str, repos: &[&str]) -> StartRequest {
        StartRequest {
            task_id: task.into(),
            user: "alice".into(),
            repos: repos.iter().map(|s| (*s).into()).collect(),
            title: None,
            spawn_editor: false,
            skip_hooks: false,
            no_cmux: true,
            force_new_workspace: false,
            branch_overrides: BTreeMap::new(),
        }
    }

    #[test]
    fn fresh_task_creates_worktree_branch_and_sidecar() {
        let mut s = Scaffold::new();
        s.add_repo("frontend", "");

        let report = start(&s.cfg, req("PROJ-1", &["frontend"]), &NullReporter).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.successes.len(), 1);

        let success = &report.successes[0];
        assert_eq!(success.action, StartAction::Created);
        assert_eq!(success.branch, "alice/PROJ-1");
        assert_eq!(success.editor, EditorStatus::Skipped);

        let worktree = s.worktree_path("frontend", "alice", "PROJ-1");
        assert!(git::is_git_repo(&worktree));
        assert_eq!(git::current_branch(&worktree).unwrap(), "alice/PROJ-1");

        let sc = sidecar::read(&worktree).unwrap().expect("sidecar present");
        assert_eq!(sc.task_id, "PROJ-1");
        assert_eq!(sc.branch, "alice/PROJ-1");
    }

    #[test]
    fn second_run_resumes_without_recreating() {
        let mut s = Scaffold::new();
        s.add_repo("frontend", "");

        start(&s.cfg, req("PROJ-1", &["frontend"]), &NullReporter).unwrap();

        // Drop a marker so we can confirm it survives a resume.
        let worktree = s.worktree_path("frontend", "alice", "PROJ-1");
        std::fs::write(worktree.join("MARKER"), "preserve me").unwrap();

        let report = start(&s.cfg, req("PROJ-1", &["frontend"]), &NullReporter).unwrap();
        assert!(report.failures.is_empty());
        assert_eq!(report.successes[0].action, StartAction::Resumed);
        assert!(
            worktree.join("MARKER").exists(),
            "resume rebuilt the worktree"
        );
    }

    #[test]
    fn preflight_rejects_existing_directory_with_wrong_branch() {
        let mut s = Scaffold::new();
        s.add_repo("frontend", "");

        // First run: creates worktree on alice/PROJ-1.
        start(&s.cfg, req("PROJ-1", &["frontend"]), &NullReporter).unwrap();
        let worktree = s.worktree_path("frontend", "alice", "PROJ-1");
        // Manually flip its branch — simulating user/git surgery.
        run(Some(&worktree), &["checkout", "-b", "something-else"]);

        let err = start(&s.cfg, req("PROJ-1", &["frontend"]), &NullReporter).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("expected alice/PROJ-1"), "{msg}");
    }

    #[test]
    fn preflight_rejects_existing_non_git_directory() {
        let mut s = Scaffold::new();
        s.add_repo("frontend", "");

        // Create a plain directory at the expected worktree path.
        let worktree = s.worktree_path("frontend", "alice", "PROJ-1");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join("not-a-repo"), "x").unwrap();

        let err = start(&s.cfg, req("PROJ-1", &["frontend"]), &NullReporter).unwrap_err();
        assert!(format!("{err}").contains("not a git repository"), "{err}");
    }

    #[test]
    fn preflight_rejects_unknown_repo() {
        let s = Scaffold::new();
        let err = start(&s.cfg, req("PROJ-1", &["ghost"]), &NullReporter).unwrap_err();
        assert!(format!("{err}").contains("ghost"), "{err}");
    }

    #[test]
    fn preflight_rejects_missing_canonical() {
        let mut s = Scaffold::new();
        // Register a repo, then nuke the canonical from disk.
        s.add_repo("frontend", "");
        std::fs::remove_dir_all(s.base_dir.join("frontend")).unwrap();

        let err = start(&s.cfg, req("PROJ-1", &["frontend"]), &NullReporter).unwrap_err();
        assert!(
            format!("{err}").contains("canonical clone not found"),
            "{err}"
        );
    }

    #[test]
    fn multi_repo_creates_worktrees_for_each() {
        let mut s = Scaffold::new();
        s.add_repo("frontend", "");
        s.add_repo("backend", "");

        let report = start(
            &s.cfg,
            req("PROJ-1", &["frontend", "backend"]),
            &NullReporter,
        )
        .unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.successes.len(), 2);
        assert!(git::is_git_repo(
            &s.worktree_path("frontend", "alice", "PROJ-1")
        ));
        assert!(git::is_git_repo(
            &s.worktree_path("backend", "alice", "PROJ-1")
        ));
    }

    #[test]
    fn multi_repo_aborts_before_side_effects_when_one_fails_preflight() {
        let mut s = Scaffold::new();
        s.add_repo("frontend", "");
        s.add_repo("backend", "");

        // Block the backend canonical so it fails preflight.
        std::fs::remove_dir_all(s.base_dir.join("backend")).unwrap();

        let err = start(
            &s.cfg,
            req("PROJ-1", &["frontend", "backend"]),
            &NullReporter,
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("backend"), "{msg}");

        // Frontend worktree must not have been created.
        assert!(
            !s.worktree_path("frontend", "alice", "PROJ-1").exists(),
            "frontend worktree was created despite preflight failure"
        );
    }

    #[test]
    fn existing_remote_branch_is_silently_resumed() {
        let mut s = Scaffold::new();
        s.add_repo("frontend", "");

        // Push alice/PROJ-1 to the upstream from the canonical so that the
        // task worktree's create-time path detects it as an existing remote.
        let canonical = s.base_dir.join("frontend");
        run(Some(&canonical), &["checkout", "-b", "alice/PROJ-1"]);
        commit_file(&canonical, "preexisting", "x");
        run(Some(&canonical), &["push", "origin", "alice/PROJ-1"]);
        run(Some(&canonical), &["checkout", "main"]);
        run(Some(&canonical), &["branch", "-D", "alice/PROJ-1"]);

        let report = start(&s.cfg, req("PROJ-1", &["frontend"]), &NullReporter).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);

        let worktree = s.worktree_path("frontend", "alice", "PROJ-1");
        // The pre-existing file from the remote branch should be present.
        assert!(worktree.join("preexisting").exists());
    }

    #[test]
    fn stale_worktree_registration_does_not_block_create() {
        let mut s = Scaffold::new();
        s.add_repo("frontend", "");

        // Create a first worktree, then `rm -rf` it without telling git.
        start(&s.cfg, req("PROJ-X", &["frontend"]), &NullReporter).unwrap();
        let stale = s.worktree_path("frontend", "alice", "PROJ-X");
        std::fs::remove_dir_all(&stale).unwrap();

        // A subsequent start for the same task should self-heal via the
        // implicit `git worktree prune` at the top of create.
        let report = start(&s.cfg, req("PROJ-X", &["frontend"]), &NullReporter).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(stale.exists());
    }

    #[test]
    fn parse_task_input_accepts_bare_form() {
        let (u, t) = parse_task_input("PROJ-1", "alice");
        assert_eq!((u.as_str(), t.as_str()), ("alice", "PROJ-1"));
    }

    #[test]
    fn parse_task_input_accepts_full_form() {
        let (u, t) = parse_task_input("bob/PROJ-1", "alice");
        assert_eq!((u.as_str(), t.as_str()), ("bob", "PROJ-1"));
    }

    #[test]
    fn editor_skipped_when_disabled_in_request() {
        let mut s = Scaffold::new();
        s.add_repo("frontend", "true {path}");

        let mut request = req("PROJ-1", &["frontend"]);
        request.spawn_editor = false;

        let report = start(&s.cfg, request, &NullReporter).unwrap();
        assert_eq!(report.successes[0].editor, EditorStatus::Skipped);
    }

    #[test]
    fn editor_skipped_when_template_empty() {
        let mut s = Scaffold::new();
        s.add_repo("frontend", "");

        let mut request = req("PROJ-1", &["frontend"]);
        request.spawn_editor = true;

        let report = start(&s.cfg, request, &NullReporter).unwrap();
        assert_eq!(report.successes[0].editor, EditorStatus::Skipped);
    }

    #[test]
    fn reporter_receives_create_event_sequence() {
        use crate::progress::testing::{Event, RecordingReporter};

        let mut s = Scaffold::new();
        s.add_repo("frontend", "");
        let r = RecordingReporter::new();
        start(&s.cfg, req("PROJ-1", &["frontend"]), &r).unwrap();

        let events = r.events();
        assert!(matches!(
            events.first(),
            Some(Event::Begin { repo, action })
                if repo == "frontend" && action == "creating"
        ));
        let step_messages: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::Step { message, .. } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert!(step_messages.iter().any(|m| m.starts_with("pruning")));
        assert!(step_messages.iter().any(|m| m.starts_with("fetching")));
        assert!(
            step_messages
                .iter()
                .any(|m| m.starts_with("adding worktree"))
        );
        assert!(step_messages.contains(&"writing sidecar"));
        assert!(matches!(events.last(), Some(Event::Ok { .. })));
    }

    #[test]
    fn reporter_receives_resume_event_with_no_create_steps() {
        use crate::progress::testing::{Event, RecordingReporter};

        let mut s = Scaffold::new();
        s.add_repo("frontend", "");
        start(&s.cfg, req("PROJ-1", &["frontend"]), &NullReporter).unwrap();

        let r = RecordingReporter::new();
        start(&s.cfg, req("PROJ-1", &["frontend"]), &r).unwrap();
        let events = r.events();
        assert!(matches!(
            events.first(),
            Some(Event::Begin { action, .. }) if action == "resuming"
        ));
        assert!(events.iter().all(|e| match e {
            Event::Step { message, .. } => {
                !message.starts_with("adding worktree")
                    && !message.starts_with("fetching")
                    && !message.starts_with("pruning")
            }
            _ => true,
        }));
    }

    #[test]
    fn branch_override_creates_specified_branch() {
        let mut s = Scaffold::new();
        s.add_repo("frontend", "");
        let mut request = req("PROJ-1", &["frontend"]);
        request
            .branch_overrides
            .insert("frontend".into(), "feature/legacy-thing".into());

        let report = start(&s.cfg, request, &NullReporter).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(report.successes[0].branch, "feature/legacy-thing");

        let worktree = s.worktree_path("frontend", "alice", "PROJ-1");
        assert_eq!(
            git::current_branch(&worktree).unwrap(),
            "feature/legacy-thing"
        );
    }

    #[test]
    fn branch_override_only_affects_listed_repo() {
        let mut s = Scaffold::new();
        s.add_repo("frontend", "");
        s.add_repo("backend", "");
        let mut request = req("PROJ-1", &["frontend", "backend"]);
        request
            .branch_overrides
            .insert("backend".into(), "feature/be-only".into());

        start(&s.cfg, request, &NullReporter).unwrap();

        let fe = s.worktree_path("frontend", "alice", "PROJ-1");
        let be = s.worktree_path("backend", "alice", "PROJ-1");
        assert_eq!(git::current_branch(&fe).unwrap(), "alice/PROJ-1");
        assert_eq!(git::current_branch(&be).unwrap(), "feature/be-only");
    }

    #[test]
    fn branch_override_for_unselected_repo_errors() {
        let mut s = Scaffold::new();
        s.add_repo("frontend", "");
        let mut request = req("PROJ-1", &["frontend"]);
        // backend isn't in the request's repo list.
        request
            .branch_overrides
            .insert("backend".into(), "feature/x".into());

        let err = start(&s.cfg, request, &NullReporter).unwrap_err();
        assert!(
            format!("{err}").contains("isn't in the selected set"),
            "{err}"
        );
    }

    #[test]
    fn resume_with_matching_override_succeeds() {
        let mut s = Scaffold::new();
        s.add_repo("frontend", "");
        let mut request = req("PROJ-1", &["frontend"]);
        request
            .branch_overrides
            .insert("frontend".into(), "feature/legacy".into());

        start(&s.cfg, request.clone(), &NullReporter).unwrap();
        let report = start(&s.cfg, request, &NullReporter).unwrap();
        assert_eq!(report.successes[0].action, StartAction::Resumed);
    }

    #[test]
    fn resume_without_override_after_create_with_override_fails() {
        // Created previously with override, now resuming without it: the
        // computed default branch won't match the on-disk branch and we'd
        // rather error than silently operate on a different branch than
        // the user implied.
        let mut s = Scaffold::new();
        s.add_repo("frontend", "");
        let mut create = req("PROJ-1", &["frontend"]);
        create
            .branch_overrides
            .insert("frontend".into(), "feature/legacy".into());
        start(&s.cfg, create, &NullReporter).unwrap();

        let resume = req("PROJ-1", &["frontend"]);
        let err = start(&s.cfg, resume, &NullReporter).unwrap_err();
        assert!(format!("{err}").contains("expected alice/PROJ-1"), "{err}");
    }

    #[test]
    fn cmux_contributors_includes_only_repos_with_panes() {
        use crate::cmux::config::{PaneSpec, RepoCmuxConfig};

        let mut s = Scaffold::new();
        s.add_repo("frontend", "");
        s.add_repo("backend", "");
        // FE has a layout, BE does not.
        s.cfg.repos.get_mut("frontend").unwrap().cmux = Some(RepoCmuxConfig {
            panes: vec![PaneSpec {
                cmd: Some("pnpm dev".into()),
                split: None,
                size: None,
                tabs: Vec::new(),
            }],
        });
        // BE has an empty cmux block — also should not participate.
        s.cfg.repos.get_mut("backend").unwrap().cmux = Some(RepoCmuxConfig { panes: Vec::new() });

        // Successes for both repos (the "happy path" mid-pipeline).
        let successes = vec![
            StartSuccess {
                repo: "frontend".into(),
                worktree_path: PathBuf::from("/csw/tasks/frontend/alice-PROJ-1"),
                branch: "alice/PROJ-1".into(),
                action: StartAction::Created,
                editor: EditorStatus::Skipped,
            },
            StartSuccess {
                repo: "backend".into(),
                worktree_path: PathBuf::from("/csw/tasks/backend/alice-PROJ-1"),
                branch: "alice/PROJ-1".into(),
                action: StartAction::Created,
                editor: EditorStatus::Skipped,
            },
        ];

        let contribs = cmux_contributors(&s.cfg, &successes);
        assert_eq!(contribs.len(), 1);
        assert_eq!(contribs[0].repo, "frontend");
    }

    #[test]
    fn cmux_contributors_skips_failed_repos_implicitly() {
        // Failed repos never enter `successes`, so the helper only sees
        // succeeded ones.
        use crate::cmux::config::{PaneSpec, RepoCmuxConfig};

        let mut s = Scaffold::new();
        s.add_repo("frontend", "");
        s.add_repo("backend", "");
        for name in ["frontend", "backend"] {
            s.cfg.repos.get_mut(name).unwrap().cmux = Some(RepoCmuxConfig {
                panes: vec![PaneSpec {
                    cmd: Some("ls".into()),
                    split: None,
                    size: None,
                    tabs: Vec::new(),
                }],
            });
        }

        // Only frontend in successes — backend "failed".
        let successes = vec![StartSuccess {
            repo: "frontend".into(),
            worktree_path: PathBuf::from("/csw/tasks/frontend/alice-PROJ-1"),
            branch: "alice/PROJ-1".into(),
            action: StartAction::Created,
            editor: EditorStatus::Skipped,
        }];

        let contribs = cmux_contributors(&s.cfg, &successes);
        assert_eq!(contribs.len(), 1);
        assert_eq!(contribs[0].repo, "frontend");
    }

    #[test]
    fn reporter_records_per_repo_failure_in_multi_repo_run() {
        use crate::progress::testing::{Event, RecordingReporter};

        let mut s = Scaffold::new();
        s.add_repo("frontend", "");
        s.add_repo("backend", "");
        // Break backend so its execute fails post-preflight.
        let backend_canonical = s.base_dir.join("backend");
        run(Some(&backend_canonical), &["remote", "remove", "origin"]);

        let r = RecordingReporter::new();
        let report = start(&s.cfg, req("PROJ-1", &["frontend", "backend"]), &r).unwrap();

        assert_eq!(report.successes.len(), 1);
        assert_eq!(report.failures.len(), 1);
        let backend_outcome = r.events().into_iter().rev().find(|e| {
            matches!(
                e,
                Event::Ok { repo, .. } | Event::Err { repo, .. } if repo == "backend"
            )
        });
        assert!(matches!(backend_outcome, Some(Event::Err { .. })));
    }
}
