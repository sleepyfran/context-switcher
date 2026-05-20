mod common;

use common::{Harness, run_git};
use predicates::prelude::*;
use std::path::Path;
use std::process::Command as StdCommand;
use tempfile::TempDir;

/// Clone the named repo's upstream into a fresh tempdir, push a new commit to
/// `main`, and return the tempdir (kept alive until drop). Used to make the
/// upstream advance past the canonical so a subsequent `csw pull` has
/// something to fast-forward.
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

#[test]
fn pull_fast_forwards_canonical_main() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    let canonical = h.base_dir.join("frontend");
    let before = head_of(&canonical, "HEAD");

    let _holder = advance_upstream(&h, "frontend", "new-file");

    h.cmd().args(["pull"]).assert().success();

    let after = head_of(&canonical, "HEAD");
    assert_ne!(before, after, "HEAD did not advance after csw pull");
    assert_eq!(
        after,
        head_of(&canonical, "refs/remotes/origin/main"),
        "HEAD should equal origin/main after a successful ff"
    );
    assert!(canonical.join("new-file").exists());
}

#[test]
fn pull_already_current_is_success() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    let canonical = h.base_dir.join("frontend");
    let before = head_of(&canonical, "HEAD");

    h.cmd().args(["pull"]).assert().success();

    let after = head_of(&canonical, "HEAD");
    assert_eq!(before, after);
}

#[test]
fn pull_runs_across_all_configured_repos_by_default() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    h.add_repo("backend", "");

    let _h1 = advance_upstream(&h, "frontend", "fe-file");
    let _h2 = advance_upstream(&h, "backend", "be-file");

    let fe_before = head_of(&h.base_dir.join("frontend"), "HEAD");
    let be_before = head_of(&h.base_dir.join("backend"), "HEAD");

    h.cmd().args(["pull"]).assert().success();

    let fe_after = head_of(&h.base_dir.join("frontend"), "HEAD");
    let be_after = head_of(&h.base_dir.join("backend"), "HEAD");

    assert_ne!(fe_before, fe_after);
    assert_ne!(be_before, be_after);
}

#[test]
fn pull_repos_flag_narrows_selection() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    h.add_repo("backend", "");

    let _h1 = advance_upstream(&h, "frontend", "fe-file");
    let _h2 = advance_upstream(&h, "backend", "be-file");

    let fe_before = head_of(&h.base_dir.join("frontend"), "HEAD");
    let be_before = head_of(&h.base_dir.join("backend"), "HEAD");

    h.cmd()
        .args(["pull", "--repos", "frontend"])
        .assert()
        .success();

    let fe_after = head_of(&h.base_dir.join("frontend"), "HEAD");
    let be_after = head_of(&h.base_dir.join("backend"), "HEAD");

    assert_ne!(fe_before, fe_after, "frontend should have been pulled");
    assert_eq!(be_before, be_after, "backend should NOT have been pulled");
}

#[test]
fn pull_unknown_repo_fails_before_any_pull() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    let _holder = advance_upstream(&h, "frontend", "would-have-been-pulled");
    let before = head_of(&h.base_dir.join("frontend"), "HEAD");

    h.cmd()
        .args(["pull", "--repos", "frontend,ghost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown repo `ghost`"))
        .stderr(predicate::str::contains("configured: frontend"));

    let after = head_of(&h.base_dir.join("frontend"), "HEAD");
    assert_eq!(
        before, after,
        "frontend should not have been pulled when --repos validation failed"
    );
}

#[test]
fn pull_no_repos_configured_is_a_soft_exit() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");

    h.cmd()
        .args(["pull"])
        .assert()
        .success()
        .stderr(predicate::str::contains("no repos configured"));
}

#[test]
fn pull_wrong_branch_is_per_repo_failure() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    let canonical = h.base_dir.join("frontend");
    // Manually move canonical off main onto a side branch.
    h.run_git_in(&canonical, &["checkout", "-b", "side"]);

    h.cmd()
        .args(["pull"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("on branch side"))
        .stderr(predicate::str::contains("expected main"));
}

#[test]
fn pull_dirty_canonical_is_per_repo_failure() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    let canonical = h.base_dir.join("frontend");
    std::fs::write(canonical.join("dirty"), "x").unwrap();

    let _holder = advance_upstream(&h, "frontend", "upstream-file");
    let before = head_of(&canonical, "HEAD");

    h.cmd()
        .args(["pull"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("working tree dirty"));

    // Canonical HEAD must not have moved when the merge step was skipped.
    let after = head_of(&canonical, "HEAD");
    assert_eq!(before, after);
}

#[test]
fn pull_diverged_canonical_is_per_repo_failure() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    let canonical = h.base_dir.join("frontend");
    // Make a local commit on the canonical's main without pushing.
    std::fs::write(canonical.join("local-only"), "x").unwrap();
    h.run_git_in(&canonical, &["add", "local-only"]);
    h.run_git_in(&canonical, &["commit", "-m", "local commit"]);

    // Advance the upstream so its history diverges from the canonical's.
    let _holder = advance_upstream(&h, "frontend", "remote-only");

    h.cmd().args(["pull"]).assert().failure().code(2);
}

#[test]
fn pull_continues_after_per_repo_failure() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    h.add_repo("backend", "");

    // Break backend by checking it out onto a non-main branch.
    h.run_git_in(&h.base_dir.join("backend"), &["checkout", "-b", "side"]);

    let _h1 = advance_upstream(&h, "frontend", "fe-file");
    let fe_before = head_of(&h.base_dir.join("frontend"), "HEAD");

    h.cmd()
        .args(["pull"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("pulled 1/2 repos"));

    let fe_after = head_of(&h.base_dir.join("frontend"), "HEAD");
    assert_ne!(
        fe_before, fe_after,
        "healthy sibling should still have been pulled"
    );
}

#[test]
fn pull_missing_canonical_is_per_repo_failure() {
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

    h.cmd()
        .args(["pull"])
        .assert()
        .failure()
        .code(2)
        .stderr(predicate::str::contains("ghost"))
        .stderr(predicate::str::contains("canonical clone not found"))
        .stderr(predicate::str::contains("pulled 1/2 repos"));
}
