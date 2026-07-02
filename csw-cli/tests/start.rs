mod common;

use common::Harness;
use predicates::prelude::*;

#[test]
fn start_creates_copy_with_branch_and_prints_path() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    let copy_str = copy.to_string_lossy().into_owned();

    h.cmd()
        .args(["start", "PROJ-1", "--repos", "frontend", "--no-editor"])
        .assert()
        .success()
        .stdout(predicate::str::contains(copy_str));

    assert!(copy.join(".git").exists(), "copy is not a git repo");
}

#[test]
fn start_resume_is_idempotent() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args(["start", "PROJ-1", "--repos", "frontend", "--no-editor"])
        .assert()
        .success();

    h.cmd()
        .args(["start", "PROJ-1", "--repos", "frontend", "--no-editor"])
        .assert()
        .success()
        .stderr(predicate::str::contains("resumed"));
}

#[test]
fn start_uses_default_repos_when_no_flags() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    // Mark frontend as the default.
    h.cmd()
        .args(["config", "repo", "default", "frontend"])
        .assert()
        .success();

    h.cmd()
        .args(["start", "PROJ-1", "--no-editor"])
        .assert()
        .success();

    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    assert!(copy.exists());
}

#[test]
fn start_only_overrides_defaults() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    h.add_repo("backend", "");
    h.cmd()
        .args(["config", "repo", "default", "frontend"])
        .assert()
        .success();

    h.cmd()
        .args(["start", "PROJ-1", "--only", "backend", "--no-editor"])
        .assert()
        .success();

    assert!(h.worktree_path("backend", "alice", "PROJ-1").exists());
    assert!(!h.worktree_path("frontend", "alice", "PROJ-1").exists());
}

#[test]
fn start_extra_adds_to_defaults() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    h.add_repo("backend", "");
    h.cmd()
        .args(["config", "repo", "default", "frontend"])
        .assert()
        .success();

    h.cmd()
        .args(["start", "PROJ-1", "--repos", "backend", "--no-editor"])
        .assert()
        .success();

    assert!(h.worktree_path("frontend", "alice", "PROJ-1").exists());
    assert!(h.worktree_path("backend", "alice", "PROJ-1").exists());
}

#[test]
fn start_unknown_repo_fails_before_side_effects() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args(["start", "PROJ-1", "--only", "ghost", "--no-editor"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("ghost"));

    assert!(!h.worktree_path("frontend", "alice", "PROJ-1").exists());
}

#[test]
fn start_with_no_selection_errors() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    // No default_repos, no --repos / --only.
    h.cmd()
        .args(["start", "PROJ-1", "--no-editor"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no repositories selected"));
}

#[test]
fn start_full_form_overrides_user() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args(["start", "bob/PROJ-1", "--repos", "frontend", "--no-editor"])
        .assert()
        .success();

    // Directory uses bob, not alice.
    assert!(h.worktree_path("frontend", "bob", "PROJ-1").exists());
    assert!(!h.worktree_path("frontend", "alice", "PROJ-1").exists());
}

#[test]
fn start_full_form_supports_legacy_slug_branch() {
    // Pre-existing branch on the upstream that follows a different username
    // and includes a slug — exactly the legacy case the user has on disk.
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("fran.gonzalez"); // configured user differs
    h.add_repo("frontend", "");

    // Push frangonzalez/PROJ-123-add-foo to the upstream from the canonical.
    let canonical = h.base_dir.join("frontend");
    h.run_git_in(
        &canonical,
        &["checkout", "-b", "frangonzalez/PROJ-123-add-foo"],
    );
    std::fs::write(canonical.join("legacy"), "x").unwrap();
    h.run_git_in(&canonical, &["add", "legacy"]);
    h.run_git_in(&canonical, &["commit", "-m", "legacy"]);
    h.run_git_in(
        &canonical,
        &["push", "origin", "frangonzalez/PROJ-123-add-foo"],
    );
    h.run_git_in(&canonical, &["checkout", "main"]);
    h.run_git_in(
        &canonical,
        &["branch", "-D", "frangonzalez/PROJ-123-add-foo"],
    );

    h.cmd()
        .args([
            "start",
            "frangonzalez/PROJ-123-add-foo",
            "--repos",
            "frontend",
            "--no-editor",
        ])
        .assert()
        .success();

    let worktree = h.worktree_path("frontend", "frangonzalez", "PROJ-123-add-foo");
    assert!(
        worktree.exists(),
        "worktree was not created at expected path"
    );
    assert!(
        worktree.join("legacy").exists(),
        "remote branch was not checked out into the worktree"
    );

    // Done with the explicit full form should clean it up safely.
    h.cmd()
        .args(["done", "frangonzalez/PROJ-123-add-foo", "--force"])
        .assert()
        .success();
    assert!(!worktree.exists());
}

#[test]
fn start_with_no_arg_errors_when_zero_tasks_exist() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args(["start", "--no-editor"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no task copies on disk"));
}

#[test]
fn start_with_no_arg_resumes_when_only_one_task_exists() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args(["start", "PROJ-1", "--repos", "frontend", "--no-editor"])
        .assert()
        .success();

    // Bare `csw start` should detect the single task and resume it.
    h.cmd()
        .args(["start", "--no-editor"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "resuming the only on-disk task: PROJ-1",
        ))
        .stderr(predicate::str::contains("resumed"));
}

#[test]
fn start_with_no_arg_errors_in_non_tty_when_multiple_tasks_exist() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    for task in ["PROJ-1", "PROJ-2"] {
        h.cmd()
            .args(["start", task, "--repos", "frontend", "--no-editor"])
            .assert()
            .success();
    }

    h.cmd()
        .args(["start", "--no-editor"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("stderr is not a terminal"));
}

#[test]
fn start_branch_override_creates_custom_branch() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    h.add_repo("backend", "");

    h.cmd()
        .args([
            "start",
            "PROJ-1",
            "--repos",
            "frontend,backend",
            "--branch",
            "backend=feature/legacy",
            "--no-editor",
        ])
        .assert()
        .success();

    let fe = h.worktree_path("frontend", "alice", "PROJ-1");
    let be = h.worktree_path("backend", "alice", "PROJ-1");
    h.run_git_in(&fe, &["rev-parse", "--abbrev-ref", "HEAD"]);
    // Verify branches via git directly.
    let fe_branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&fe)
        .output()
        .unwrap();
    let be_branch = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&be)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&fe_branch.stdout).trim(),
        "alice/PROJ-1"
    );
    assert_eq!(
        String::from_utf8_lossy(&be_branch.stdout).trim(),
        "feature/legacy"
    );
}

#[test]
fn start_branch_override_for_unknown_repo_errors() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args([
            "start",
            "PROJ-1",
            "--repos",
            "frontend",
            "--branch",
            "backend=feature/x",
            "--no-editor",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("isn't in the selected set"));
}

#[test]
fn start_branch_override_malformed_errors() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args([
            "start",
            "PROJ-1",
            "--repos",
            "frontend",
            "--branch",
            "no-equals-sign",
            "--no-editor",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("repo=branch"));
}

#[test]
fn start_writes_sidecar_with_title() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args([
            "start",
            "PROJ-1",
            "--repos",
            "frontend",
            "--title",
            "Add foo to bar",
            "--no-editor",
        ])
        .assert()
        .success();

    // The sidecar lives inside the per-worktree git dir; resolve via
    // `git rev-parse --git-path csw.json` from inside the worktree.
    let worktree = h.worktree_path("frontend", "alice", "PROJ-1");
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--git-path", "csw.json"])
        .current_dir(&worktree)
        .output()
        .unwrap();
    assert!(out.status.success(), "git rev-parse failed");
    let sidecar = String::from_utf8(out.stdout).unwrap();
    let raw = std::fs::read_to_string(sidecar.trim()).unwrap();
    assert!(raw.contains("Add foo to bar"), "{raw}");
}

#[test]
fn start_without_arg_infers_task_from_current_directory() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    // Create two tasks so a bare `csw start` would otherwise need a picker.
    h.cmd()
        .args(["start", "PROJ-1", "--repos", "frontend", "--no-editor"])
        .assert()
        .success();
    h.cmd()
        .args(["start", "PROJ-2", "--repos", "frontend", "--no-editor"])
        .assert()
        .success();

    // Running `csw start` from inside PROJ-1's worktree should resume PROJ-1
    // without prompting.
    let worktree = h.worktree_path("frontend", "alice", "PROJ-1");
    h.cmd()
        .current_dir(&worktree)
        .args(["start", "--no-editor"])
        .assert()
        .success()
        .stderr(predicate::str::contains("PROJ-1"))
        .stderr(predicate::str::contains("current directory"));
}

#[test]
fn start_accepts_no_cmux_flag() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args([
            "start",
            "PROJ-1",
            "--repos",
            "frontend",
            "--no-editor",
            "--no-cmux",
        ])
        .assert()
        .success();
}
