//! `csw list` — discover task worktrees by interrogating each canonical's
//! `git worktree list`, filtering to entries that live under
//! `<tasks_dir>/<repo_name>/`. The leaf directory name (`<user>-<task_id>`)
//! tells us which task each worktree belongs to.

use crate::Config;
use crate::{git, paths, sidecar};
use anyhow::Result;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
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
    let mut by_task: BTreeMap<String, TaskEntry> = BTreeMap::new();

    for (repo_name, repo) in &cfg.repos {
        if let Some(ref filter) = request.only_repo {
            if filter != repo_name {
                continue;
            }
        }

        let canonical = cfg.canonical_path(repo);
        if !git::is_git_repo(&canonical) {
            continue;
        }

        let tasks_root = cfg.repo_tasks_dir(repo_name);
        let entries = match git::worktree_list(&canonical) {
            Ok(es) => es,
            // Don't take down the whole list when one repo's canonical is in
            // a weird state — just skip it.
            Err(_) => continue,
        };

        for entry in entries {
            if !is_under(&entry.path, &tasks_root) {
                continue;
            }
            let leaf = match entry.path.file_name().and_then(|s| s.to_str()) {
                Some(n) => n,
                None => continue,
            };
            let Some(task_id) = paths::parse_worktree_dirname(leaf, &request.user) else {
                continue;
            };

            let sidecar_data = sidecar::read(&entry.path).ok().flatten();
            let title = sidecar_data.as_ref().and_then(|s| s.title.clone());
            let created_at = sidecar_data.as_ref().map(|s| s.created_at);

            let bucket = by_task.entry(task_id.clone()).or_insert_with(|| TaskEntry {
                task_id: task_id.clone(),
                repos: Vec::new(),
                title: None,
                created_at: None,
            });
            bucket.repos.push(RepoEntry {
                repo: repo_name.clone(),
                worktree_path: entry.path.clone(),
            });
            if bucket.title.is_none() {
                bucket.title = title;
            }
            bucket.created_at = match (bucket.created_at, created_at) {
                (Some(a), Some(b)) if a < b => Some(a),
                (Some(a), None) => Some(a),
                (_, Some(b)) => Some(b),
                _ => None,
            };
        }
    }

    let mut out: Vec<TaskEntry> = by_task.into_values().collect();
    // Most recent first; tasks without timestamps end up at the end.
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

/// Whether `path` lives directly under `parent` (single nesting level).
/// Compares canonicalised forms first to absorb macOS-style `/var` →
/// `/private/var` differences between `git worktree list` output and the
/// raw `tasks_dir` from the config.
fn is_under(path: &std::path::Path, parent: &std::path::Path) -> bool {
    let direct = path.parent().map(|p| p == parent).unwrap_or(false);
    if direct {
        return true;
    }
    let path_parent = match path.parent() {
        Some(p) => p,
        None => return false,
    };
    match (
        std::fs::canonicalize(path_parent),
        std::fs::canonicalize(parent),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
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

        fn start_task(&self, task: &str, repos: &[&str], title: Option<&str>) {
            let req = StartRequest {
                task_id: task.into(),
                user: "alice".into(),
                repos: repos.iter().map(|s| (*s).into()).collect(),
                title: title.map(String::from),
                spawn_editor: false,
                skip_hooks: false,
                no_cmux: true,
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
        // Drop noise inside the repo's tasks dir.
        std::fs::create_dir_all(s.cfg.repo_tasks_dir("frontend").join("bob-IGNORE")).unwrap();

        let r = list(&s.cfg, &req()).unwrap();
        assert_eq!(
            r.len(),
            1,
            "task ids: {:?}",
            r.iter().map(|t| &t.task_id).collect::<Vec<_>>()
        );
        assert_eq!(r[0].task_id, "PROJ-1");
    }
}
