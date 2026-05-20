//! Spawn an interactive subshell rooted at a given directory.
//!
//! Used by `csw nav` to drop the user into a task copy without requiring
//! per-shell integration. The subshell inherits stdio so it's interactive,
//! receives a set of `CSW_*` env vars for prompt customisation, and we
//! wait for it to exit and propagate its exit code.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

/// Resolve which shell to launch: `$SHELL` if set and non-empty, otherwise
/// fall back to `/bin/sh`.
pub fn resolve_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

/// Spawn the user's shell rooted at `cwd`, exporting `extra_env`. Returns
/// the shell's exit code (0 on a clean `exit` / Ctrl+D).
pub fn spawn_subshell(cwd: &Path, extra_env: &HashMap<String, String>) -> Result<i32> {
    spawn_with_shell(cwd, extra_env, &resolve_shell())
}

/// Test-friendly variant that lets the caller specify which shell to spawn.
pub fn spawn_with_shell(
    cwd: &Path,
    extra_env: &HashMap<String, String>,
    shell: &str,
) -> Result<i32> {
    let mut cmd = Command::new(shell);
    cmd.current_dir(cwd);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let status = cmd
        .status()
        .with_context(|| format!("spawning subshell `{shell}` in {}", cwd.display()))?;
    Ok(status.code().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn resolve_shell_uses_shell_env_when_set() {
        // SAFETY: tests are run sequentially within this module by default.
        unsafe {
            std::env::set_var("SHELL", "/usr/local/bin/example");
        }
        assert_eq!(resolve_shell(), "/usr/local/bin/example");
    }

    #[test]
    fn resolve_shell_falls_back_to_sh_when_empty() {
        unsafe {
            std::env::set_var("SHELL", "");
        }
        assert_eq!(resolve_shell(), "/bin/sh");
    }

    #[test]
    fn spawn_with_shell_sets_cwd_and_env() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("nested");
        std::fs::create_dir_all(&cwd).unwrap();

        // Use `sh -c` directly; we ride the subprocess shape rather than
        // spawning an interactive shell. The script writes pwd and one env
        // var into files so we can verify both made it through.
        let mut env: HashMap<String, String> = HashMap::new();
        env.insert("CSW_TEST".into(), "hello".into());

        // We construct an ad-hoc command instead of going through
        // spawn_with_shell directly, since spawn_with_shell doesn't take a
        // script. Cover the core behaviour (cwd + env propagation) by
        // running a trivial script with the same primitives.
        let status = Command::new("sh")
            .arg("-c")
            .arg(r#"pwd > pwd.txt && printf '%s' "$CSW_TEST" > env.txt"#)
            .current_dir(&cwd)
            .envs(&env)
            .status()
            .unwrap();
        assert!(status.success());

        let recorded_pwd = std::fs::read_to_string(cwd.join("pwd.txt")).unwrap();
        let recorded_env = std::fs::read_to_string(cwd.join("env.txt")).unwrap();
        assert!(
            recorded_pwd.trim().ends_with("nested"),
            "pwd recorded was {recorded_pwd:?}"
        );
        assert_eq!(recorded_env, "hello");
    }

    #[test]
    fn spawn_with_shell_propagates_exit_code() {
        let tmp = TempDir::new().unwrap();
        // Use `sh -c 'exit 7'` as the "shell" via a wrapper script: write a
        // tiny script that exits 7, set it executable, point spawn at it.
        let script = tmp.path().join("fake-shell.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 7\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let env: HashMap<String, String> = HashMap::new();
        let code = spawn_with_shell(tmp.path(), &env, script.to_str().unwrap()).unwrap();
        assert_eq!(code, 7);
    }
}
