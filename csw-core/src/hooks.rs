//! Post-create hooks: declarative actions that run after a fresh task
//! worktree is set up but before the editor is launched.
//!
//! Two action types in v1:
//! * [`HookAction::Copy`] — copy a file or directory from the canonical
//!   into the new worktree. Paths are sandboxed: relative-only, no `..`,
//!   no absolute paths.
//! * [`HookAction::Run`] — execute a shell command, with `cwd` either the
//!   new worktree (default) or the canonical. Output is captured; on
//!   failure the runner returns the last 50 lines of combined stdout/stderr.
//!
//! Hooks run sequentially in declaration order. The first failure stops
//! processing for that repo and bubbles up as a `HookError`.

use crate::progress::RepoProgress;
use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

const FAILURE_TAIL_LINES: usize = 50;

/// One declarative action to run after creating a fresh worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HookAction {
    /// Copy a file or directory from the canonical into the new worktree.
    Copy(CopyAction),
    /// Run a shell command (`sh -c`) with cwd at either worktree or canonical.
    Run(RunAction),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyAction {
    /// Source path (relative to canonical) and target path (relative to
    /// worktree) when both are the same. Mutually exclusive with `from`/`to`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,

    /// Source path relative to canonical. Required if `path` is unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<PathBuf>,

    /// Target path relative to worktree. Required if `path` is unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<PathBuf>,

    /// If true, missing source is logged but not treated as an error.
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAction {
    pub cmd: String,
    #[serde(default)]
    pub cwd: RunCwd,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunCwd {
    #[default]
    Worktree,
    Canonical,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Path-sandbox check: must be relative, must not traverse upward, must
/// not be empty.
fn validate_relative_path(p: &Path) -> Result<(), String> {
    if p.as_os_str().is_empty() {
        return Err("path is empty".into());
    }
    if p.is_absolute() {
        return Err(format!("path must be relative: {}", p.display()));
    }
    for c in p.components() {
        match c {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir => {
                return Err(format!("path may not contain `..`: {}", p.display()));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(format!("path may not be absolute: {}", p.display()));
            }
        }
    }
    Ok(())
}

/// Resolve the (source, target) pair for a `CopyAction`, after applying
/// `path` shorthand. Validates both ends.
fn resolve_copy_paths(action: &CopyAction) -> Result<(PathBuf, PathBuf), HookError> {
    let (from, to) = match (&action.path, &action.from, &action.to) {
        (Some(p), None, None) => (p.clone(), p.clone()),
        (None, Some(f), Some(t)) => (f.clone(), t.clone()),
        (Some(p), None, Some(t)) => (p.clone(), t.clone()),
        (Some(p), Some(f), None) => (f.clone(), p.clone()),
        (None, None, _) | (None, _, None) => {
            return Err(HookError::InvalidCopy(
                "copy action requires `path`, or both `from` and `to`".into(),
            ));
        }
        (Some(_), Some(_), Some(_)) => {
            return Err(HookError::InvalidCopy(
                "copy action: pass `path` or `from`+`to`, not all three".into(),
            ));
        }
    };
    validate_relative_path(&from).map_err(HookError::InvalidCopy)?;
    validate_relative_path(&to).map_err(HookError::InvalidCopy)?;
    Ok((from, to))
}

#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("invalid copy action: {0}")]
    InvalidCopy(String),
    #[error("copy source not found: {0}")]
    CopySourceMissing(PathBuf),
    #[error("copy failed: {0}")]
    CopyFailed(String),
    #[error("run failed: {cmd}\n{tail}")]
    RunFailed { cmd: String, tail: String },
    #[error("run could not be spawned: {0}")]
    RunSpawn(String),
}

/// Per-repo context required by the runner.
#[derive(Debug, Clone)]
pub struct HookContext<'a> {
    pub repo: &'a str,
    pub worktree: &'a Path,
    pub canonical: &'a Path,
    pub task_id: &'a str,
    pub branch: &'a str,
    pub user: &'a str,
}

/// Run the configured hooks against a fresh worktree. Stops at the first
/// failure (after which the repo's overall `start` outcome flips to
/// failed). Each step is reported through the supplied `progress` so the
/// CLI spinner stays informative.
pub fn run_hooks(
    actions: &[HookAction],
    ctx: &HookContext<'_>,
    progress: &dyn RepoProgress,
) -> Result<(), HookError> {
    for action in actions {
        match action {
            HookAction::Copy(c) => execute_copy(c, ctx, progress)?,
            HookAction::Run(r) => execute_run(r, ctx, progress)?,
        }
    }
    Ok(())
}

fn execute_copy(
    action: &CopyAction,
    ctx: &HookContext<'_>,
    progress: &dyn RepoProgress,
) -> Result<(), HookError> {
    let (from_rel, to_rel) = resolve_copy_paths(action)?;
    let src = ctx.canonical.join(&from_rel);
    let dst = ctx.worktree.join(&to_rel);

    progress.step(&format!("copy {}", from_rel.display()));

    let meta = match std::fs::symlink_metadata(&src) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if action.optional {
                progress.step(&format!(
                    "copy {} skipped (missing, optional)",
                    from_rel.display()
                ));
                return Ok(());
            }
            return Err(HookError::CopySourceMissing(src));
        }
        Err(e) => {
            return Err(HookError::CopyFailed(format!(
                "stat {}: {e}",
                src.display()
            )));
        }
    };

    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| HookError::CopyFailed(format!("mkdir {}: {e}", parent.display())))?;
    }

    if meta.is_dir() {
        copy_dir_recursive(&src, &dst)
            .map_err(|e| HookError::CopyFailed(format!("{}: {e}", src.display())))?;
    } else {
        std::fs::copy(&src, &dst).map_err(|e| {
            HookError::CopyFailed(format!("{} -> {}: {e}", src.display(), dst.display()))
        })?;
    }
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to)?;
        }
        // Skip symlinks and other special files for v1 — copy semantics
        // are intentionally narrow.
    }
    Ok(())
}

fn execute_run(
    action: &RunAction,
    ctx: &HookContext<'_>,
    progress: &dyn RepoProgress,
) -> Result<(), HookError> {
    let label = action
        .name
        .clone()
        .unwrap_or_else(|| short_label(&action.cmd));
    progress.step(&format!("run {label}"));

    let cwd = match action.cwd {
        RunCwd::Worktree => ctx.worktree,
        RunCwd::Canonical => ctx.canonical,
    };

    let output = Command::new("sh")
        .arg("-c")
        .arg(&action.cmd)
        .current_dir(cwd)
        .env("CSW_WORKTREE", ctx.worktree)
        .env("CSW_CANONICAL", ctx.canonical)
        .env("CSW_TASK_ID", ctx.task_id)
        .env("CSW_BRANCH", ctx.branch)
        .env("CSW_USER", ctx.user)
        .env("CSW_REPO", ctx.repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| HookError::RunSpawn(format!("{}: {e}", action.cmd)))?;

    if output.status.success() {
        return Ok(());
    }

    let mut combined = String::new();
    combined.push_str(&String::from_utf8_lossy(&output.stdout));
    if !combined.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    let tail: String = combined
        .lines()
        .rev()
        .take(FAILURE_TAIL_LINES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    Err(HookError::RunFailed {
        cmd: action.cmd.clone(),
        tail,
    })
}

fn short_label(cmd: &str) -> String {
    let trimmed = cmd.trim();
    if trimmed.len() <= 40 {
        trimmed.to_string()
    } else {
        let mut out: String = trimmed.chars().take(37).collect();
        out.push_str("...");
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::NullReporter;
    use crate::progress::Reporter;
    use tempfile::TempDir;

    struct Fixture {
        _tmp: TempDir,
        canonical: PathBuf,
        worktree: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = TempDir::new().unwrap();
            let canonical = tmp.path().join("canonical");
            let worktree = tmp.path().join("worktree");
            std::fs::create_dir_all(&canonical).unwrap();
            std::fs::create_dir_all(&worktree).unwrap();
            Self {
                _tmp: tmp,
                canonical,
                worktree,
            }
        }

        fn ctx<'a>(&'a self, repo: &'a str) -> HookContext<'a> {
            HookContext {
                repo,
                worktree: &self.worktree,
                canonical: &self.canonical,
                task_id: "PROJ-1",
                branch: "alice/PROJ-1",
                user: "alice",
            }
        }
    }

    fn np() -> Box<dyn RepoProgress> {
        NullReporter.begin("test", "creating")
    }

    #[test]
    fn validate_path_rejects_absolute() {
        assert!(validate_relative_path(Path::new("/etc/passwd")).is_err());
    }

    #[test]
    fn validate_path_rejects_parent_traversal() {
        assert!(validate_relative_path(Path::new("../escape")).is_err());
        assert!(validate_relative_path(Path::new("ok/../escape")).is_err());
    }

    #[test]
    fn validate_path_accepts_simple_relative() {
        assert!(validate_relative_path(Path::new("a/b.txt")).is_ok());
        assert!(validate_relative_path(Path::new("./a/b.txt")).is_ok());
    }

    #[test]
    fn copy_with_path_shorthand_copies_file_at_same_path() {
        let f = Fixture::new();
        std::fs::write(f.canonical.join("secret.conf"), "shh").unwrap();

        let action = CopyAction {
            path: Some("secret.conf".into()),
            from: None,
            to: None,
            optional: false,
        };
        execute_copy(&action, &f.ctx("frontend"), np().as_ref()).unwrap();

        let content = std::fs::read_to_string(f.worktree.join("secret.conf")).unwrap();
        assert_eq!(content, "shh");
    }

    #[test]
    fn copy_with_from_to_uses_different_target() {
        let f = Fixture::new();
        std::fs::write(f.canonical.join("template.env"), "X=1").unwrap();

        let action = CopyAction {
            path: None,
            from: Some("template.env".into()),
            to: Some(".env".into()),
            optional: false,
        };
        execute_copy(&action, &f.ctx("frontend"), np().as_ref()).unwrap();

        assert_eq!(
            std::fs::read_to_string(f.worktree.join(".env")).unwrap(),
            "X=1"
        );
        assert!(!f.worktree.join("template.env").exists());
    }

    #[test]
    fn copy_recurses_into_directories() {
        let f = Fixture::new();
        std::fs::create_dir_all(f.canonical.join(".vscode")).unwrap();
        std::fs::write(f.canonical.join(".vscode").join("settings.json"), "{}").unwrap();
        std::fs::create_dir_all(f.canonical.join(".vscode").join("nested")).unwrap();
        std::fs::write(f.canonical.join(".vscode").join("nested").join("a"), "x").unwrap();

        let action = CopyAction {
            path: Some(".vscode".into()),
            from: None,
            to: None,
            optional: false,
        };
        execute_copy(&action, &f.ctx("frontend"), np().as_ref()).unwrap();

        assert!(f.worktree.join(".vscode").join("settings.json").exists());
        assert!(f.worktree.join(".vscode").join("nested").join("a").exists());
    }

    #[test]
    fn copy_creates_target_parent_directories() {
        let f = Fixture::new();
        std::fs::write(f.canonical.join("source.txt"), "x").unwrap();
        let action = CopyAction {
            path: None,
            from: Some("source.txt".into()),
            to: Some("nested/dirs/target.txt".into()),
            optional: false,
        };
        execute_copy(&action, &f.ctx("frontend"), np().as_ref()).unwrap();
        assert!(f.worktree.join("nested/dirs/target.txt").exists());
    }

    #[test]
    fn copy_missing_source_required_errors() {
        let f = Fixture::new();
        let action = CopyAction {
            path: Some("nope".into()),
            from: None,
            to: None,
            optional: false,
        };
        let err = execute_copy(&action, &f.ctx("frontend"), np().as_ref()).unwrap_err();
        assert!(matches!(err, HookError::CopySourceMissing(_)));
    }

    #[test]
    fn copy_missing_source_optional_skips() {
        let f = Fixture::new();
        let action = CopyAction {
            path: Some("nope".into()),
            from: None,
            to: None,
            optional: true,
        };
        execute_copy(&action, &f.ctx("frontend"), np().as_ref()).unwrap();
    }

    #[test]
    fn copy_rejects_path_traversal() {
        let f = Fixture::new();
        let action = CopyAction {
            path: None,
            from: Some("../etc/passwd".into()),
            to: Some("evil".into()),
            optional: false,
        };
        let err = execute_copy(&action, &f.ctx("frontend"), np().as_ref()).unwrap_err();
        assert!(matches!(err, HookError::InvalidCopy(_)));
    }

    #[test]
    fn copy_rejects_absolute_target() {
        let f = Fixture::new();
        let action = CopyAction {
            path: None,
            from: Some("ok".into()),
            to: Some("/tmp/escape".into()),
            optional: false,
        };
        let err = execute_copy(&action, &f.ctx("frontend"), np().as_ref()).unwrap_err();
        assert!(matches!(err, HookError::InvalidCopy(_)));
    }

    #[test]
    fn run_succeeds_for_zero_exit_command() {
        let f = Fixture::new();
        let action = RunAction {
            cmd: "true".into(),
            cwd: RunCwd::Worktree,
            name: None,
        };
        execute_run(&action, &f.ctx("frontend"), np().as_ref()).unwrap();
    }

    #[test]
    fn run_failure_reports_tail() {
        let f = Fixture::new();
        let action = RunAction {
            cmd: "echo boom; exit 7".into(),
            cwd: RunCwd::Worktree,
            name: None,
        };
        let err = execute_run(&action, &f.ctx("frontend"), np().as_ref()).unwrap_err();
        match err {
            HookError::RunFailed { tail, .. } => assert!(tail.contains("boom"), "{tail}"),
            other => panic!("expected RunFailed, got {other:?}"),
        }
    }

    #[test]
    fn run_uses_worktree_cwd_by_default() {
        let f = Fixture::new();
        let action = RunAction {
            cmd: "pwd > marker".into(),
            cwd: RunCwd::Worktree,
            name: None,
        };
        execute_run(&action, &f.ctx("frontend"), np().as_ref()).unwrap();
        let recorded = std::fs::read_to_string(f.worktree.join("marker")).unwrap();
        assert!(
            recorded
                .trim()
                .ends_with(f.worktree.file_name().unwrap().to_str().unwrap()),
            "expected cwd to end in worktree path, got {recorded}"
        );
    }

    #[test]
    fn run_uses_canonical_cwd_when_requested() {
        let f = Fixture::new();
        let action = RunAction {
            cmd: "pwd > marker".into(),
            cwd: RunCwd::Canonical,
            name: None,
        };
        execute_run(&action, &f.ctx("frontend"), np().as_ref()).unwrap();
        let recorded = std::fs::read_to_string(f.canonical.join("marker")).unwrap();
        assert!(
            recorded
                .trim()
                .ends_with(f.canonical.file_name().unwrap().to_str().unwrap()),
            "expected cwd to end in canonical path, got {recorded}"
        );
    }

    #[test]
    fn run_exposes_csw_env_vars() {
        let f = Fixture::new();
        let action = RunAction {
            cmd: r#"printf '%s|%s|%s|%s|%s|%s' "$CSW_REPO" "$CSW_TASK_ID" "$CSW_BRANCH" "$CSW_USER" "$CSW_WORKTREE" "$CSW_CANONICAL" > out"#
                .into(),
            cwd: RunCwd::Worktree,
            name: None,
        };
        execute_run(&action, &f.ctx("frontend"), np().as_ref()).unwrap();
        let out = std::fs::read_to_string(f.worktree.join("out")).unwrap();
        let parts: Vec<&str> = out.split('|').collect();
        assert_eq!(parts[0], "frontend");
        assert_eq!(parts[1], "PROJ-1");
        assert_eq!(parts[2], "alice/PROJ-1");
        assert_eq!(parts[3], "alice");
        assert!(parts[4].ends_with("worktree"));
        assert!(parts[5].ends_with("canonical"));
    }

    #[test]
    fn run_hooks_stops_at_first_failure() {
        let f = Fixture::new();
        let actions = vec![
            HookAction::Run(RunAction {
                cmd: "touch first".into(),
                cwd: RunCwd::Worktree,
                name: None,
            }),
            HookAction::Run(RunAction {
                cmd: "exit 2".into(),
                cwd: RunCwd::Worktree,
                name: None,
            }),
            HookAction::Run(RunAction {
                cmd: "touch should-not-exist".into(),
                cwd: RunCwd::Worktree,
                name: None,
            }),
        ];

        let err = run_hooks(&actions, &f.ctx("frontend"), np().as_ref()).unwrap_err();
        assert!(matches!(err, HookError::RunFailed { .. }));
        assert!(f.worktree.join("first").exists());
        assert!(!f.worktree.join("should-not-exist").exists());
    }

    #[test]
    fn copy_action_serde_round_trips() {
        let action = HookAction::Copy(CopyAction {
            path: Some("secret.conf".into()),
            from: None,
            to: None,
            optional: false,
        });
        let s = toml::to_string(&action).unwrap();
        let parsed: HookAction = toml::from_str(&s).unwrap();
        assert_eq!(action, parsed);
    }

    #[test]
    fn run_action_serde_round_trips() {
        let action = HookAction::Run(RunAction {
            cmd: "pnpm install".into(),
            cwd: RunCwd::Canonical,
            name: Some("install deps".into()),
        });
        let s = toml::to_string(&action).unwrap();
        let parsed: HookAction = toml::from_str(&s).unwrap();
        assert_eq!(action, parsed);
    }
}
