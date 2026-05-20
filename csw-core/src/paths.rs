//! Filesystem layout: where task worktrees live on disk, and reverse parsing
//! of a directory back into `(repo_name, task_id)`.
//!
//! A task worktree lives at `<tasks_dir>/<repo_name>/<user>-<task_id>`. The
//! parent two segments tell us which repo and (implicitly) which user owns
//! the worktree; the leaf encodes the task id.

use crate::config::Config;
use std::path::{Path, PathBuf};

const SEPARATOR: &str = "-";

/// Compute the on-disk worktree path for a given task.
pub fn worktree_path(config: &Config, repo_name: &str, user: &str, task_id: &str) -> PathBuf {
    config
        .repo_tasks_dir(repo_name)
        .join(worktree_dirname(user, task_id))
}

/// Build a worktree directory name (the leaf) from its parts. Currently
/// `<user>-<task_id>`; symmetric with the parse routine below.
pub fn worktree_dirname(user: &str, task_id: &str) -> String {
    format!("{user}{SEPARATOR}{task_id}")
}

/// The branch name a task should be checked out as.
pub fn branch_name(user: &str, task_id: &str) -> String {
    format!("{user}/{task_id}")
}

/// Parse a worktree directory name back into a task id, given the expected
/// user. Returns `None` if the name doesn't match.
pub fn parse_worktree_dirname(dirname: &str, user: &str) -> Option<String> {
    let prefix = format!("{user}{SEPARATOR}");
    let rest = dirname.strip_prefix(&prefix)?;
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

/// For a given absolute path, walk upward until we find a directory whose
/// parent is one of the configured repos' tasks directories. Returns
/// `(repo_name, task_id, worktree_root)`.
pub fn infer_task_from_path(
    config: &Config,
    user: &str,
    start: &Path,
) -> Option<(String, String, PathBuf)> {
    let mut cursor: Option<&Path> = Some(start);
    while let Some(dir) = cursor {
        let name = dir.file_name()?.to_string_lossy().into_owned();
        let task_id = parse_worktree_dirname(&name, user);
        if let Some(task_id) = task_id {
            // The directory name parses; confirm its parent matches one of
            // the configured repo tasks dirs.
            if let Some(parent) = dir.parent() {
                for repo_name in config.repos.keys() {
                    let expected = config.repo_tasks_dir(repo_name);
                    if same_directory(Some(parent), Some(&expected)) {
                        return Some((repo_name.clone(), task_id, dir.to_path_buf()));
                    }
                }
            }
        }
        cursor = dir.parent();
    }
    None
}

/// Best-effort directory equality. Tries canonicalised forms first to
/// absorb things like `/var/folders` → `/private/var/folders` on macOS, then
/// falls back to a raw `PathBuf` comparison so this still works for unit
/// tests with imaginary paths.
fn same_directory(a: Option<&Path>, b: Option<&Path>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            if let (Ok(ca), Ok(cb)) = (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
                ca == cb
            } else {
                a == b
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RepoConfig;
    use std::collections::BTreeMap;

    fn cfg_with_repo(name: &str, path: &str, tasks_dir: &str) -> Config {
        let mut repos = BTreeMap::new();
        repos.insert(name.into(), RepoConfig::new(path, "zed {path}"));
        Config {
            base_dir: PathBuf::from("/dev"),
            tasks_dir: PathBuf::from(tasks_dir),
            username: None,
            default_repos: vec![],
            cmux: None,
            repos,
        }
    }

    #[test]
    fn worktree_path_joins_tasks_dir_repo_and_leaf() {
        let cfg = cfg_with_repo("frontend", "frontend", "/csw/tasks");
        let p = worktree_path(&cfg, "frontend", "fran.gonzalez", "PROJ-123");
        assert_eq!(
            p,
            PathBuf::from("/csw/tasks/frontend/fran.gonzalez-PROJ-123")
        );
    }

    #[test]
    fn branch_name_uses_slash() {
        assert_eq!(branch_name("alice", "PROJ-1"), "alice/PROJ-1");
    }

    #[test]
    fn parse_round_trips_simple_taskid() {
        let dn = worktree_dirname("alice", "PROJ-1");
        assert_eq!(parse_worktree_dirname(&dn, "alice"), Some("PROJ-1".into()));
    }

    #[test]
    fn parse_handles_dots_in_username() {
        let dn = worktree_dirname("fran.gonzalez", "PROJ-123");
        assert_eq!(
            parse_worktree_dirname(&dn, "fran.gonzalez"),
            Some("PROJ-123".into())
        );
    }

    #[test]
    fn parse_handles_hyphens_in_taskid() {
        let dn = worktree_dirname("alice", "hotfix-tls-renewal");
        assert_eq!(
            parse_worktree_dirname(&dn, "alice"),
            Some("hotfix-tls-renewal".into())
        );
    }

    #[test]
    fn parse_rejects_wrong_user() {
        assert!(parse_worktree_dirname("alice-PROJ-1", "bob").is_none());
    }

    #[test]
    fn parse_rejects_empty_taskid() {
        assert!(parse_worktree_dirname("alice-", "alice").is_none());
    }

    #[test]
    fn parse_rejects_unrelated_directory() {
        assert!(parse_worktree_dirname("totally-unrelated", "alice").is_none());
    }

    #[test]
    fn infer_task_from_path_finds_worktree_directory() {
        let cfg = cfg_with_repo("frontend", "frontend", "/csw/tasks");
        let dir = PathBuf::from("/csw/tasks/frontend/alice-PROJ-1");
        let r = infer_task_from_path(&cfg, "alice", &dir).unwrap();
        assert_eq!(r.0, "frontend");
        assert_eq!(r.1, "PROJ-1");
        assert_eq!(r.2, dir);
    }

    #[test]
    fn infer_task_walks_up_from_subdirectory() {
        let cfg = cfg_with_repo("frontend", "frontend", "/csw/tasks");
        let inside = PathBuf::from("/csw/tasks/frontend/alice-PROJ-1/src/components");
        let r = infer_task_from_path(&cfg, "alice", &inside).unwrap();
        assert_eq!(r.1, "PROJ-1");
    }

    #[test]
    fn infer_task_returns_none_when_outside_any_tasks_dir() {
        let cfg = cfg_with_repo("frontend", "frontend", "/csw/tasks");
        let outside = PathBuf::from("/some/other/place");
        assert!(infer_task_from_path(&cfg, "alice", &outside).is_none());
    }

    #[test]
    fn infer_task_does_not_match_unrelated_parent() {
        // Directory name parses but its parent isn't the configured repo's
        // tasks dir — should not match.
        let cfg = cfg_with_repo("frontend", "frontend", "/csw/tasks");
        let elsewhere = PathBuf::from("/somewhere/else/alice-PROJ-1");
        assert!(infer_task_from_path(&cfg, "alice", &elsewhere).is_none());
    }
}
