//! `csw list` — discover task worktrees by walking `tasks_dir` directly.
//! Each entry under `tasks_dir` is a task root named `<user>-<task_id>`;
//! each repo subdirectory beneath it that's a registered git worktree is
//! one of the task's repos.

use crate::Config;
use crate::{git, paths, sidecar};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct TaskEntry {
    pub task_id: String,
    pub repos: Vec<RepoEntry>,
    pub title: Option<String>,
    /// Earliest creation time across all worktrees, if any sidecar provided one.
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct RepoEntry {
    pub repo: String,
    pub worktree_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ListRequest {
    pub user: String,
    /// Limit to a single configured repo (by name).
    pub only_repo: Option<String>,
}

pub fn list(cfg: &Config, request: &ListRequest) -> Result<Vec<TaskEntry>> {
    let mut out = Vec::new();

    let read_dir = match std::fs::read_dir(&cfg.tasks_dir) {
        Ok(d) => d,
        // No tasks_dir yet — nothing has ever been started.
        Err(_) => return Ok(out),
    };

    for entry in read_dir {
        let Ok(entry) = entry else { continue };
        let task_dir = entry.path();
        if !task_dir.is_dir() {
            continue;
        }
        let Some(leaf) = task_dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(task_id) = paths::parse_worktree_dirname(leaf, &request.user) else {
            continue;
        };

        let mut repos = Vec::new();
        let mut title = None;
        let mut created_at: Option<DateTime<Utc>> = None;

        for repo_name in cfg.repos.keys() {
            if let Some(ref filter) = request.only_repo {
                if filter != repo_name {
                    continue;
                }
            }
            let worktree_path = task_dir.join(repo_name);
            if !git::is_git_repo(&worktree_path) {
                continue;
            }

            let sidecar_data = sidecar::read(&worktree_path).ok().flatten();
            if title.is_none() {
                title = sidecar_data.as_ref().and_then(|s| s.title.clone());
            }
            let this_created_at = sidecar_data.as_ref().map(|s| s.created_at);
            created_at = match (created_at, this_created_at) {
                (Some(a), Some(b)) if a < b => Some(a),
                (Some(a), None) => Some(a),
                (_, Some(b)) => Some(b),
                _ => None,
            };

            repos.push(RepoEntry {
                repo: repo_name.clone(),
                worktree_path,
            });
        }

        if repos.is_empty() {
            continue;
        }

        out.push(TaskEntry {
            task_id,
            repos,
            title,
            created_at,
        });
    }

    // Most recent first; tasks without timestamps end up at the end.
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
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
                    agent: None,
                    detached_editor: true,
                    mux: None,
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

        fn start_task(&self, task: &str, repos: &[&str], title: Option<&str>) {
            let req = StartRequest {
                task_id: task.into(),
                user: "alice".into(),
                repos: repos.iter().map(|s| (*s).into()).collect(),
                title: title.map(String::from),
                spawn_editor: false,
                skip_hooks: false,
                no_mux: true,
                force_new_workspace: false,
                branch_overrides: std::collections::BTreeMap::new(),
            };
            start::start(&self.cfg, req, &NullReporter).unwrap();
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

    fn req() -> ListRequest {
        ListRequest {
            user: "alice".into(),
            only_repo: None,
        }
    }

    #[test]
    fn list_is_empty_when_nothing_started() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        let r = list(&s.cfg, &req()).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn list_returns_started_tasks() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"], Some("First"));
        s.start_task("PROJ-2", &["frontend"], None);

        let r = list(&s.cfg, &req()).unwrap();
        assert_eq!(r.len(), 2);
        let ids: Vec<&str> = r.iter().map(|t| t.task_id.as_str()).collect();
        assert!(ids.contains(&"PROJ-1") && ids.contains(&"PROJ-2"));
    }

    #[test]
    fn list_groups_multi_repo_tasks() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.add_repo("backend");
        s.start_task("PROJ-1", &["frontend", "backend"], None);

        let r = list(&s.cfg, &req()).unwrap();
        assert_eq!(r.len(), 1);
        let repos: Vec<&str> = r[0].repos.iter().map(|x| x.repo.as_str()).collect();
        assert!(repos.contains(&"frontend"));
        assert!(repos.contains(&"backend"));
    }

    #[test]
    fn list_propagates_title_from_sidecar() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"], Some("My title"));
        let r = list(&s.cfg, &req()).unwrap();
        assert_eq!(r[0].title.as_deref(), Some("My title"));
    }

    #[test]
    fn list_filters_by_repo() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.add_repo("backend");
        s.start_task("PROJ-1", &["frontend"], None);
        s.start_task("PROJ-2", &["backend"], None);

        let mut r = req();
        r.only_repo = Some("frontend".into());
        let result = list(&s.cfg, &r).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].task_id, "PROJ-1");
    }

    #[test]
    fn list_skips_unrelated_directories() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        s.start_task("PROJ-1", &["frontend"], None);
        // Drop noise directly inside tasks_dir: another user's task, and a
        // plain non-worktree directory.
        std::fs::create_dir_all(s.cfg.tasks_dir.join("bob-IGNORE")).unwrap();
        std::fs::create_dir_all(s.cfg.tasks_dir.join("not-a-task-dir")).unwrap();

        let r = list(&s.cfg, &req()).unwrap();
        assert_eq!(
            r.len(),
            1,
            "task ids: {:?}",
            r.iter().map(|t| &t.task_id).collect::<Vec<_>>()
        );
        assert_eq!(r[0].task_id, "PROJ-1");
    }

    #[test]
    fn list_skips_task_dir_with_no_registered_repos() {
        let mut s = Scaffold::new();
        s.add_repo("frontend");
        // A task-shaped directory that isn't actually a git worktree for
        // any configured repo (e.g. leftover cruft).
        std::fs::create_dir_all(s.cfg.tasks_dir.join("alice-EMPTY").join("frontend")).unwrap();

        let r = list(&s.cfg, &req()).unwrap();
        assert!(r.is_empty(), "task ids: {:?}", r);
    }
}
