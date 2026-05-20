mod common;

use common::Harness;
use predicates::prelude::*;

fn started_task(h: &Harness, task: &str) {
    h.cmd()
        .args(["start", task, "--repos", "frontend", "--no-editor"])
        .assert()
        .success();
}

#[test]
fn done_refuses_when_unpushed_then_succeeds_with_force() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started_task(&h, "PROJ-1");

    // Default `start` leaves the branch with no upstream — done should refuse.
    h.cmd()
        .args(["done", "PROJ-1"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("no upstream"));

    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    assert!(copy.exists(), "copy must not have been deleted");

    h.cmd()
        .args(["done", "PROJ-1", "--force"])
        .assert()
        .success();
    assert!(!copy.exists());
}

#[test]
fn done_refuses_when_dirty() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started_task(&h, "PROJ-1");

    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    h.run_git_in(&copy, &["push", "-u", "origin", "alice/PROJ-1"]);
    std::fs::write(copy.join("dirty"), "x").unwrap();

    h.cmd()
        .args(["done", "PROJ-1"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("dirty"));
}

#[test]
fn done_clean_pushed_branch_succeeds_silently() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started_task(&h, "PROJ-1");

    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    h.run_git_in(&copy, &["push", "-u", "origin", "alice/PROJ-1"]);
    // Merge into main on the upstream so there's no unmerged warning.
    h.run_git_in(&copy, &["push", "origin", "alice/PROJ-1:main"]);
    h.run_git_in(&copy, &["fetch", "origin"]);

    h.cmd().args(["done", "PROJ-1"]).assert().success();
    assert!(!copy.exists());
}

#[test]
fn done_pushed_unmerged_prompts_unless_yes() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started_task(&h, "PROJ-1");

    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    std::fs::write(copy.join("work"), "x").unwrap();
    h.run_git_in(&copy, &["add", "work"]);
    h.run_git_in(&copy, &["commit", "-m", "wip"]);
    h.run_git_in(&copy, &["push", "-u", "origin", "alice/PROJ-1"]);

    // --yes auto-confirms the unmerged warning.
    h.cmd()
        .args(["done", "PROJ-1", "--yes"])
        .assert()
        .success()
        .stderr(predicate::str::contains("not merged"));
    assert!(!copy.exists());
}

#[test]
fn done_infers_task_from_cwd() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started_task(&h, "PROJ-1");

    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    h.run_git_in(&copy, &["push", "-u", "origin", "alice/PROJ-1"]);

    // Run `csw done --force` from inside the copy with no task argument.
    h.cmd()
        .current_dir(&copy)
        .args(["done", "--force"])
        .assert()
        .success();
}

#[test]
fn done_unknown_task_errors() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args(["done", "DOESNOTEXIST"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no worktrees found"));
}
