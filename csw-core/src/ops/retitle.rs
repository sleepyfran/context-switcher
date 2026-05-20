//! `csw retitle` — change a task's title after it's been started.
//!
//! Updates every per-repo sidecar that exists for the task, then refreshes
//! the CMux workspace label (when running inside CMux). The sidecar phase is
//! best-effort per repo, mirroring `start`'s `Report` shape; the CMux phase
//! is soft-fail, matching `start`/`done`.

use crate::cmux::{self, CmuxOutcome};
use crate::ops::list::{self, ListRequest};
use crate::progress::Reporter;
use crate::sidecar::{self, Sidecar};
use crate::{Config, git};
use anyhow::{Context, Result, anyhow};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct RetitleRequest {
    pub task_id: String,
    pub user: String,
    /// New title. `None` (or any all-whitespace input the caller normalises
    /// to `None`) clears the title.
    pub title: Option<String>,
    /// Suppress the CMux rename for this run, even when csw is inside CMux.
    pub no_cmux: bool,
}

#[derive(Debug, Clone)]
pub struct RetitleSuccess {
    pub repo: String,
    pub worktree_path: PathBuf,
    pub previous_title: Option<String>,
}

#[derive(Debug)]
pub struct RetitleReport {
    pub task_id: String,
    pub user: String,
    /// First non-empty title we saw across the task's sidecars before the
    /// rewrite. Used by the renderer for the `old → new` line.
    pub previous_title: Option<String>,
    pub new_title: Option<String>,
    pub successes: Vec<RetitleSuccess>,
    pub failures: Vec<(String, anyhow::Error)>,
    pub cmux: CmuxOutcome,
}

impl RetitleReport {
    pub fn any_failure(&self) -> bool {
        !self.failures.is_empty()
    }
}

/// Normalise a user-supplied title: trim whitespace, treat empty as `None`.
/// Matches CMux's own `workspace_name_for` semantics so the sidecar and
/// workspace label can never disagree about "is there a title".
pub fn normalise_title(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

pub fn retitle(
    cfg: &Config,
    request: RetitleRequest,
    reporter: &dyn Reporter,
) -> Result<RetitleReport> {
    let entries = list::list(
        cfg,
        &ListRequest {
            user: request.user.clone(),
            only_repo: None,
        },
    )?;
    let entry = entries
        .into_iter()
        .find(|e| e.task_id == request.task_id)
        .ok_or_else(|| {
            anyhow!(
                "no worktrees found for {}/{}",
                request.user,
                request.task_id
            )
        })?;

    let new_title = normalise_title(request.title.as_deref());
    let mut report = RetitleReport {
        task_id: request.task_id.clone(),
        user: request.user.clone(),
        previous_title: entry.title.clone(),
        new_title: new_title.clone(),
        successes: Vec::new(),
        failures: Vec::new(),
        cmux: CmuxOutcome::NotApplicable,
    };

    for repo_entry in &entry.repos {
        let progress = reporter.begin(&repo_entry.repo, "retitling");
        progress.step("updating sidecar");
        match update_sidecar(
            &request.task_id,
            &repo_entry.worktree_path,
            new_title.as_deref(),
        ) {
            Ok(previous_title) => {
                progress.ok("title updated");
                report.successes.push(RetitleSuccess {
                    repo: repo_entry.repo.clone(),
                    worktree_path: repo_entry.worktree_path.clone(),
                    previous_title,
                });
            }
            Err(e) => {
                progress.err(&format!("{e}"));
                report.failures.push((repo_entry.repo.clone(), e));
            }
        }
    }

    report.cmux = finalize_cmux(cfg, &request, new_title.as_deref());
    Ok(report)
}

/// Read-modify-write the sidecar at `worktree_path`, preserving every field
/// other than `title`. If the sidecar is missing or corrupt, write a fresh
/// one populated from the live branch and the request's task id.
fn update_sidecar(
    task_id: &str,
    worktree_path: &std::path::Path,
    new_title: Option<&str>,
) -> Result<Option<String>> {
    let existing = sidecar::read(worktree_path).context("reading existing sidecar")?;
    let (previous_title, sidecar_to_write) = match existing {
        Some(prev) => {
            let previous = prev.title.clone();
            let updated = Sidecar {
                title: new_title.map(str::to_string),
                ..prev
            };
            (previous, updated)
        }
        None => {
            let branch = git::current_branch(worktree_path)
                .context("resolving current branch for fresh sidecar")?;
            let fresh = Sidecar::new(task_id, branch, new_title.map(str::to_string));
            (None, fresh)
        }
    };
    sidecar::write(worktree_path, &sidecar_to_write).context("writing sidecar")?;
    Ok(previous_title)
}

fn finalize_cmux(cfg: &Config, request: &RetitleRequest, new_title: Option<&str>) -> CmuxOutcome {
    if request.no_cmux || !cfg.cmux_enabled() || !cmux::detect() {
        return CmuxOutcome::NotApplicable;
    }
    cmux::rename(&request.task_id, new_title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RepoConfig;
    use crate::ops::start::{self, StartRequest};
    use crate::paths;
    use crate::progress::NullReporter;
    use crate::sidecar;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::process::Command;
    use tempfile::TempDir;

    /// Mirrors the integration scaffolding used by `ops::list`: a temp dir
    /// with a bare upstream per repo, a canonical clone, a tasks dir, and
    /// an in-memory `Config`. Lets us drive `start::start` end-to-end to
    /// seed real sidecars before exercising retitle.
    struct Harness {
        tmp: TempDir,
        cfg: Config,
        base_dir: PathBuf,
        user: String,
    }

    impl Harness {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let base_dir = tmp.path().join("dev");
            let tasks_dir = tmp.path().join("tasks");
            std::fs::create_dir_all(&base_dir).unwrap();
            std::fs::create_dir_all(&tasks_dir).unwrap();
            let cfg = Config {
                base_dir: base_dir.clone(),
                tasks_dir,
                username: Some("alice".into()),
                default_repos: vec![],
                cmux: None,
                repos: BTreeMap::new(),
            };
            Self {
                tmp,
                cfg,
                base_dir,
                user: "alice".into(),
            }
        }

        fn add_repo(&mut self, name: &str) {
            let upstream = self.tmp.path().join(format!("{name}-up.git"));
            git(
                None,
                &[
                    "init",
                    "--bare",
                    "--initial-branch=main",
                    upstream.to_str().unwrap(),
                ],
            );
            let canonical = self.base_dir.join(name);
            git(
                None,
                &[
                    "clone",
                    upstream.to_str().unwrap(),
                    canonical.to_str().unwrap(),
                ],
            );
            git(
                Some(&canonical),
                &["config", "user.email", "test@example.com"],
            );
            git(Some(&canonical), &["config", "user.name", "Test"]);
            git(Some(&canonical), &["config", "commit.gpgsign", "false"]);
            std::fs::write(canonical.join("README"), "x").unwrap();
            git(Some(&canonical), &["add", "README"]);
            git(Some(&canonical), &["commit", "-m", "init"]);
            git(Some(&canonical), &["push", "origin", "main"]);
            self.cfg
                .repos
                .insert(name.into(), RepoConfig::new(name, ""));
        }

        fn start_task(&self, task: &str, repos: &[&str], title: Option<&str>) {
            let req = StartRequest {
                task_id: task.into(),
                user: self.user.clone(),
                repos: repos.iter().map(|s| (*s).into()).collect(),
                title: title.map(String::from),
                spawn_editor: false,
                skip_hooks: false,
                no_cmux: true,
                force_new_workspace: false,
                branch_overrides: BTreeMap::new(),
            };
            start::start(&self.cfg, req, &NullReporter).unwrap();
        }

        fn worktree_path(&self, repo: &str, task: &str) -> PathBuf {
            self.cfg
                .repo_tasks_dir(repo)
                .join(paths::worktree_dirname(&self.user, task))
        }
    }

    fn git(dir: Option<&Path>, args: &[&str]) {
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
    fn normalise_title_strips_whitespace_and_empties() {
        assert_eq!(normalise_title(None), None);
        assert_eq!(normalise_title(Some("")), None);
        assert_eq!(normalise_title(Some("   ")), None);
        assert_eq!(normalise_title(Some("  Fix bug  ")), Some("Fix bug".into()));
        assert_eq!(normalise_title(Some("Fix bug")), Some("Fix bug".into()));
    }

    #[test]
    fn retitle_updates_existing_sidecar_preserves_created_at() {
        let mut h = Harness::new();
        h.add_repo("fe");
        h.start_task("PROJ-1", &["fe"], Some("Old title"));
        let wt = h.worktree_path("fe", "PROJ-1");
        let before = sidecar::read(&wt).unwrap().expect("sidecar written");

        let report = retitle(
            &h.cfg,
            RetitleRequest {
                task_id: "PROJ-1".into(),
                user: h.user.clone(),
                title: Some("New title".into()),
                no_cmux: true,
            },
            &NullReporter,
        )
        .unwrap();

        assert!(!report.any_failure());
        assert_eq!(report.previous_title.as_deref(), Some("Old title"));
        assert_eq!(report.new_title.as_deref(), Some("New title"));
        assert_eq!(report.successes.len(), 1);

        let after = sidecar::read(&wt).unwrap().unwrap();
        assert_eq!(after.title.as_deref(), Some("New title"));
        assert_eq!(after.created_at, before.created_at);
        assert_eq!(after.task_id, before.task_id);
        assert_eq!(after.branch, before.branch);
    }

    #[test]
    fn retitle_with_empty_title_clears() {
        let mut h = Harness::new();
        h.add_repo("fe");
        h.start_task("PROJ-1", &["fe"], Some("Old title"));
        let wt = h.worktree_path("fe", "PROJ-1");

        let report = retitle(
            &h.cfg,
            RetitleRequest {
                task_id: "PROJ-1".into(),
                user: h.user.clone(),
                title: Some("".into()),
                no_cmux: true,
            },
            &NullReporter,
        )
        .unwrap();

        assert!(!report.any_failure());
        assert_eq!(report.new_title, None);
        let after = sidecar::read(&wt).unwrap().unwrap();
        assert_eq!(after.title, None);
    }

    #[test]
    fn retitle_updates_every_repo_for_a_multi_repo_task() {
        let mut h = Harness::new();
        h.add_repo("fe");
        h.add_repo("be");
        h.start_task("PROJ-2", &["fe", "be"], Some("Old"));

        let report = retitle(
            &h.cfg,
            RetitleRequest {
                task_id: "PROJ-2".into(),
                user: h.user.clone(),
                title: Some("New".into()),
                no_cmux: true,
            },
            &NullReporter,
        )
        .unwrap();

        assert_eq!(report.successes.len(), 2);
        for repo in ["fe", "be"] {
            let wt = h.worktree_path(repo, "PROJ-2");
            assert_eq!(
                sidecar::read(&wt).unwrap().unwrap().title.as_deref(),
                Some("New"),
                "title not updated on {repo}"
            );
        }
    }

    #[test]
    fn retitle_errors_when_no_worktrees_exist() {
        let mut h = Harness::new();
        h.add_repo("fe");
        let err = retitle(
            &h.cfg,
            RetitleRequest {
                task_id: "MISSING-1".into(),
                user: h.user.clone(),
                title: Some("nope".into()),
                no_cmux: true,
            },
            &NullReporter,
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("no worktrees found"),
            "got: {err}"
        );
    }

    #[test]
    fn retitle_recreates_sidecar_when_missing() {
        let mut h = Harness::new();
        h.add_repo("fe");
        h.start_task("PROJ-3", &["fe"], Some("Old"));
        let wt = h.worktree_path("fe", "PROJ-3");
        std::fs::remove_file(sidecar::sidecar_path(&wt).unwrap()).unwrap();

        let report = retitle(
            &h.cfg,
            RetitleRequest {
                task_id: "PROJ-3".into(),
                user: h.user.clone(),
                title: Some("Recovered".into()),
                no_cmux: true,
            },
            &NullReporter,
        )
        .unwrap();

        assert!(!report.any_failure());
        let fresh = sidecar::read(&wt).unwrap().expect("recreated");
        assert_eq!(fresh.task_id, "PROJ-3");
        assert_eq!(fresh.title.as_deref(), Some("Recovered"));
        // Branch should match whatever `start` checked out for this task.
        assert!(!fresh.branch.is_empty());
    }

    #[test]
    fn retitle_recreates_sidecar_when_corrupt() {
        let mut h = Harness::new();
        h.add_repo("fe");
        h.start_task("PROJ-4", &["fe"], Some("Old"));
        let wt = h.worktree_path("fe", "PROJ-4");
        std::fs::write(sidecar::sidecar_path(&wt).unwrap(), "{ not json").unwrap();

        let report = retitle(
            &h.cfg,
            RetitleRequest {
                task_id: "PROJ-4".into(),
                user: h.user.clone(),
                title: Some("Recovered".into()),
                no_cmux: true,
            },
            &NullReporter,
        )
        .unwrap();

        assert!(!report.any_failure());
        let fresh = sidecar::read(&wt).unwrap().expect("recreated");
        assert_eq!(fresh.title.as_deref(), Some("Recovered"));
    }

    #[test]
    fn retitle_skips_cmux_when_no_cmux_set() {
        let mut h = Harness::new();
        h.add_repo("fe");
        h.start_task("PROJ-5", &["fe"], None);
        let report = retitle(
            &h.cfg,
            RetitleRequest {
                task_id: "PROJ-5".into(),
                user: h.user.clone(),
                title: Some("Hi".into()),
                no_cmux: true,
            },
            &NullReporter,
        )
        .unwrap();
        assert_eq!(report.cmux, CmuxOutcome::NotApplicable);
    }
}
