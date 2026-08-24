mod common;

use common::Harness;
use predicates::prelude::*;

/// Seed an old-format worktree by hand at
/// `<tasks_dir>/<repo>/<user>-<task_id>`, checked out on `<user>/<task_id>`.
fn seed_legacy_worktree(h: &Harness, repo: &str, user: &str, task_id: &str) {
    let canonical = h.base_dir.join(repo);
    let old_dir = h.tasks_dir.join(repo);
    std::fs::create_dir_all(&old_dir).unwrap();
    let leaf = old_dir.join(format!("{user}-{task_id}"));
    let branch = format!("{user}/{task_id}");
    h.run_git_in(
        &canonical,
        &[
            "worktree",
            "add",
            "--no-track",
            "-b",
            &branch,
            leaf.to_str().unwrap(),
            "main",
        ],
    );
}

#[test]
fn migrate_dry_run_does_not_move_anything() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    seed_legacy_worktree(&h, "frontend", "alice", "PROJ-1");

    h.cmd()
        .args(["migrate", "--dry-run"])
        .assert()
        .success()
        .stderr(predicate::str::contains("alice/PROJ-1"));

    let old_leaf = h.tasks_dir.join("frontend").join("alice-PROJ-1");
    assert!(old_leaf.exists(), "dry-run must not move anything");
}

#[test]
fn migrate_moves_legacy_task_to_new_layout() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    seed_legacy_worktree(&h, "frontend", "alice", "PROJ-1");

    h.cmd().args(["migrate", "--yes"]).assert().success();

    let new_path = h.worktree_path("frontend", "alice", "PROJ-1");
    assert!(new_path.exists(), "expected worktree at {new_path:?}");
    assert!(new_path.join(".git").exists());

    // Old per-repo directory is gone since it's now empty.
    assert!(!h.tasks_dir.join("frontend").exists());
}

#[test]
fn migrate_with_nothing_to_migrate_is_a_no_op() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args(["migrate"])
        .assert()
        .success()
        .stderr(predicate::str::contains("nothing to migrate"));
}

#[test]
fn list_warns_about_legacy_tasks() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    seed_legacy_worktree(&h, "frontend", "alice", "PROJ-1");

    h.cmd()
        .args(["list"])
        .assert()
        .success()
        .stderr(predicate::str::contains("csw migrate"));
}

#[test]
fn list_after_migrate_sees_task_at_new_path() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    seed_legacy_worktree(&h, "frontend", "alice", "PROJ-1");

    h.cmd().args(["migrate", "--yes"]).assert().success();

    h.cmd()
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("PROJ-1"))
        .stderr(predicate::str::contains("csw migrate").not());
}
