mod common;

use common::{Harness, run_git};
use predicates::prelude::*;
use std::path::Path;
use std::process::Command as StdCommand;
use tempfile::TempDir;

/// Clone the named repo's upstream into a fresh tempdir, push a new commit to
/// `main`, and return the tempdir (kept alive until drop). Use this to make
/// the upstream advance past the canonical so a subsequent `csw fetch` has
/// something to pull down.
fn advance_upstream(h: &Harness, repo: &str, filename: &str) -> TempDir {
    let upstream = h.upstreams.path().join(format!("{repo}.git"));
    let workdir = TempDir::new().unwrap();
    let work = workdir.path().join("work");
    run_git(
        None,
        &["clone", upstream.to_str().unwrap(), work.to_str().unwrap()],
    );
    run_git(Some(&work), &["config", "user.email", "test@example.com"]);
    run_git(Some(&work), &["config", "user.name", "Test"]);
    run_git(Some(&work), &["config", "commit.gpgsign", "false"]);
    std::fs::write(work.join(filename), "x").unwrap();
    run_git(Some(&work), &["add", filename]);
    run_git(Some(&work), &["commit", "-m", filename]);
    run_git(Some(&work), &["push", "origin", "main"]);
    workdir
}

fn head_of(repo: &Path, refname: &str) -> String {
    let out = StdCommand::new("git")
        .args(["rev-parse", refname])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git rev-parse {refname} in {}: {}",
        repo.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn ref_exists(repo: &Path, refname: &str) -> bool {
    StdCommand::new("git")
        .args(["rev-parse", "--verify", "--quiet", refname])
        .current_dir(repo)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn fetch_advances_origin_main_in_canonical() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    let canonical = h.base_dir.join("frontend");
    let before = head_of(&canonical, "refs/remotes/origin/main");

    let _holder = advance_upstream(&h, "frontend", "new-file");

    h.cmd().args(["fetch"]).assert().success();

    let after = head_of(&canonical, "refs/remotes/origin/main");
    assert_ne!(before, after, "origin/main did not move after csw fetch");
}

#[test]
fn fetch_runs_across_all_configured_repos_by_default() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    h.add_repo("backend", "");

    let _h1 = advance_upstream(&h, "frontend", "fe-file");
    let _h2 = advance_upstream(&h, "backend", "be-file");

    let fe_before = head_of(&h.base_dir.join("frontend"), "refs/remotes/origin/main");
    let be_before = head_of(&h.base_dir.join("backend"), "refs/remotes/origin/main");

    h.cmd().args(["fetch"]).assert().success();

    let fe_after = head_of(&h.base_dir.join("frontend"), "refs/remotes/origin/main");
    let be_after = head_of(&h.base_dir.join("backend"), "refs/remotes/origin/main");

    assert_ne!(fe_before, fe_after);
    assert_ne!(be_before, be_after);
}

#[test]
fn fetch_repos_flag_narrows_selection() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    h.add_repo("backend", "");

    let _h1 = advance_upstream(&h, "frontend", "fe-file");
    let _h2 = advance_upstream(&h, "backend", "be-file");

    let fe_before = head_of(&h.base_dir.join("frontend"), "refs/remotes/origin/main");
    let be_before = head_of(&h.base_dir.join("backend"), "refs/remotes/origin/main");

    h.cmd()
        .args(["fetch", "--repos", "frontend"])
        .assert()
        .success();

    let fe_after = head_of(&h.base_dir.join("frontend"), "refs/remotes/origin/main");
    let be_after = head_of(&h.base_dir.join("backend"), "refs/remotes/origin/main");

    assert_ne!(fe_before, fe_after, "frontend should have been fetched");
    assert_eq!(be_before, be_after, "backend should NOT have been fetched");
}

#[test]
fn fetch_prunes_deleted_remote_branches() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    let canonical = h.base_dir.join("frontend");

    // Push a feature branch from the canonical so origin/feature/x exists.
    h.run_git_in(&canonical, &["checkout", "-b", "feature/x"]);
    std::fs::write(canonical.join("y"), "y").unwrap();
    h.run_git_in(&canonical, &["add", "y"]);
    h.run_git_in(&canonical, &["commit", "-m", "y"]);
    h.run_git_in(&canonical, &["push", "origin", "feature/x"]);
    h.run_git_in(&canonical, &["checkout", "main"]);

    assert!(
        ref_exists(&canonical, "refs/remotes/origin/feature/x"),
        "origin/feature/x should exist after push"
    );

    // Delete the branch on the upstream from a different clone.
    let workdir = TempDir::new().unwrap();
    let work = workdir.path().join("work");
    let upstream = h.upstreams.path().join("frontend.git");
    run_git(
        None,
        &["clone", upstream.to_str().unwrap(), work.to_str().unwrap()],
    );
    run_git(Some(&work), &["push", "origin", "--delete", "feature/x"]);

    h.cmd().args(["fetch"]).assert().success();

    assert!(
        !ref_exists(&canonical, "refs/remotes/origin/feature/x"),
        "origin/feature/x should be pruned after csw fetch"
    );
}

#[test]
fn fetch_unknown_repo_fails_before_any_fetch() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    let _holder = advance_upstream(&h, "frontend", "would-have-been-fetched");
    let before = head_of(&h.base_dir.join("frontend"), "refs/remotes/origin/main");

    h.cmd()
        .args(["fetch", "--repos", "frontend,ghost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown repo `ghost`"))
        .stderr(predicate::str::contains("configured: frontend"));

    let after = head_of(&h.base_dir.join("frontend"), "refs/remotes/origin/main");
    assert_eq!(
        before, after,
        "frontend should not have been fetched when --repos validation failed"
    );
}

#[test]
fn fetch_no_repos_configured_is_a_soft_exit() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");

    h.cmd()
        .args(["fetch"])
        .assert()
        .success()
        .stderr(predicate::str::contains("no repos configured"));
}

#[test]
fn fetch_missing_canonical_is_per_repo_failure() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    // Register a second repo whose canonical was never cloned.
    h.cmd()
        .args([
            "config", "repo", "add", "ghost", "--path", "ghost", "--editor", "",
        ])
        .assert()
        .success();

    let _holder = advance_upstream(&h, "frontend", "fe-file");

    let assert = h.cmd().args(["fetch"]).assert().failure();
    assert
        .code(2)
        .stderr(predicate::str::contains("ghost"))
        .stderr(predicate::str::contains("canonical clone not found"))
        .stderr(predicate::str::contains("fetched 1/2 repos"));

    // The healthy sibling still got fetched.
    // (No assertion needed beyond the summary line — that exit code 2 with
    //  "1/2" means frontend succeeded.)
}
