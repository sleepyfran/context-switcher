//! Shared test harness for CLI integration tests.
#![allow(dead_code)] // not every test file uses every helper

use assert_cmd::Command;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use tempfile::TempDir;

pub struct Harness {
    pub home: TempDir,
    pub xdg: TempDir,
    pub upstreams: TempDir,
    pub base_dir: PathBuf,
    pub tasks_dir: PathBuf,
}

impl Harness {
    pub fn new() -> Self {
        let home = TempDir::new().unwrap();
        let xdg = TempDir::new().unwrap();
        let upstreams = TempDir::new().unwrap();
        let base_dir = home.path().join("dev");
        // Mirror the default `tasks_dir` (sibling of config.toml inside the
        // XDG config dir) so we don't need to override the field explicitly.
        let tasks_dir = xdg.path().join("context-switcher").join("tasks");
        std::fs::create_dir_all(&base_dir).unwrap();
        std::fs::create_dir_all(&tasks_dir).unwrap();
        Self {
            home,
            xdg,
            upstreams,
            base_dir,
            tasks_dir,
        }
    }

    pub fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("csw").unwrap();
        c.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.xdg.path())
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env_remove("USER")
            .env_remove("USERNAME")
            // Suppress the CMux integration during tests: even if a developer
            // happens to have CMux running locally, csw should not try to talk
            // to its socket from inside the harness.
            .env_remove("CMUX_WORKSPACE_ID");
        c
    }

    pub fn run_git_in(&self, dir: &Path, args: &[&str]) {
        let mut c = StdCommand::new("git");
        c.current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com");
        let out = c.args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {} in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    pub fn config_set_base_dir(&self) {
        self.cmd()
            .args(["config", "set", "base_dir", self.base_dir.to_str().unwrap()])
            .assert()
            .success();
    }

    pub fn config_set_username(&self, name: &str) {
        self.cmd()
            .args(["config", "set", "username", name])
            .assert()
            .success();
    }

    pub fn add_repo(&self, name: &str, editor: &str) {
        // Create an "upstream" bare repo and a canonical clone under base_dir,
        // then commit one file so the canonical has a useful HEAD.
        let upstream = self.upstreams.path().join(format!("{name}.git"));
        run_git(
            None,
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                upstream.to_str().unwrap(),
            ],
        );

        let canonical = self.base_dir.join(name);
        run_git(
            None,
            &[
                "clone",
                upstream.to_str().unwrap(),
                canonical.to_str().unwrap(),
            ],
        );
        run_git(
            Some(&canonical),
            &["config", "user.email", "test@example.com"],
        );
        run_git(Some(&canonical), &["config", "user.name", "Test User"]);
        run_git(Some(&canonical), &["config", "commit.gpgsign", "false"]);
        std::fs::write(canonical.join("README"), "hi").unwrap();
        run_git(Some(&canonical), &["add", "README"]);
        run_git(Some(&canonical), &["commit", "-m", "initial"]);
        run_git(Some(&canonical), &["push", "origin", "main"]);
        // Mirror what `git clone` does against a non-empty remote: establish
        // origin/HEAD so resolve_origin_head can see the default branch.
        run_git(Some(&canonical), &["remote", "set-head", "origin", "main"]);

        // Register in csw config.
        self.cmd()
            .args([
                "config", "repo", "add", name, "--path", name, "--editor", editor,
            ])
            .assert()
            .success();
    }

    pub fn worktree_path(&self, repo: &str, user: &str, task: &str) -> PathBuf {
        self.tasks_dir.join(repo).join(format!("{user}-{task}"))
    }
}

pub fn run_git(dir: Option<&Path>, args: &[&str]) {
    let mut c = StdCommand::new("git");
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
