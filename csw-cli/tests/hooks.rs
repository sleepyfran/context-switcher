mod common;

use common::Harness;
use predicates::prelude::*;
use std::path::Path;

/// Helper: rewrite the config.toml file in place to add a `post_create`
/// block to a repo. Tests bypass the interactive wizard since dialoguer
/// requires stdin we can't easily script through `assert_cmd`.
fn append_to_config(config_path: &Path, snippet: &str) {
    let original = std::fs::read_to_string(config_path).unwrap();
    let mut combined = original;
    combined.push('\n');
    combined.push_str(snippet);
    combined.push('\n');
    std::fs::write(config_path, combined).unwrap();
}

fn config_file(h: &Harness) -> std::path::PathBuf {
    h.xdg.path().join("context-switcher").join("config.toml")
}

#[test]
fn start_runs_copy_and_run_hooks_in_order() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    // Drop a gitignored file in the canonical that the copy hook should pull over.
    let canonical = h.base_dir.join("frontend");
    std::fs::write(canonical.join("secret.conf"), "shh").unwrap();

    append_to_config(
        &config_file(&h),
        r#"
[[repos.frontend.post_create]]
type = "copy"
path = "secret.conf"

[[repos.frontend.post_create]]
type = "run"
cmd = "printf '%s' \"$CSW_REPO\" > .ran-via-hook"
"#,
    );

    h.cmd()
        .args(["start", "PROJ-1", "--repos", "frontend", "--no-editor"])
        .assert()
        .success();

    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    assert_eq!(
        std::fs::read_to_string(copy.join("secret.conf")).unwrap(),
        "shh",
        "copy hook did not run"
    );
    assert_eq!(
        std::fs::read_to_string(copy.join(".ran-via-hook")).unwrap(),
        "frontend",
        "run hook did not fire or env vars not propagated"
    );
}

#[test]
fn start_skip_hooks_flag_bypasses_post_create() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    let canonical = h.base_dir.join("frontend");
    std::fs::write(canonical.join("secret.conf"), "shh").unwrap();

    append_to_config(
        &config_file(&h),
        r#"
[[repos.frontend.post_create]]
type = "copy"
path = "secret.conf"
"#,
    );

    h.cmd()
        .args([
            "start",
            "PROJ-1",
            "--repos",
            "frontend",
            "--no-editor",
            "--skip-hooks",
        ])
        .assert()
        .success();

    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    assert!(
        !copy.join("secret.conf").exists(),
        "copy hook ran despite --skip-hooks"
    );
}

#[test]
fn hook_failure_flips_repo_to_failed_and_leaves_copy_on_disk() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    append_to_config(
        &config_file(&h),
        r#"
[[repos.frontend.post_create]]
type = "run"
cmd = "exit 7"
"#,
    );

    h.cmd()
        .args(["start", "PROJ-1", "--repos", "frontend", "--no-editor"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("run failed"));

    // Copy is intentionally left on disk so the user can inspect it.
    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    assert!(copy.exists(), "copy was rolled back on hook failure");
}

#[test]
fn missing_copy_source_required_fails_start() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    append_to_config(
        &config_file(&h),
        r#"
[[repos.frontend.post_create]]
type = "copy"
path = "does-not-exist.conf"
"#,
    );

    h.cmd()
        .args(["start", "PROJ-1", "--repos", "frontend", "--no-editor"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("copy source not found"));
}

#[test]
fn optional_copy_source_silently_skips() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    append_to_config(
        &config_file(&h),
        r#"
[[repos.frontend.post_create]]
type = "copy"
path = "missing-but-optional"
optional = true
"#,
    );

    h.cmd()
        .args(["start", "PROJ-1", "--repos", "frontend", "--no-editor"])
        .assert()
        .success();
}

#[test]
fn hooks_only_run_on_create_not_on_resume() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    append_to_config(
        &config_file(&h),
        r#"
[[repos.frontend.post_create]]
type = "run"
cmd = "echo $RANDOM > marker"
"#,
    );

    h.cmd()
        .args(["start", "PROJ-1", "--repos", "frontend", "--no-editor"])
        .assert()
        .success();
    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    let initial = std::fs::read_to_string(copy.join("marker")).unwrap();

    // Resume: should not re-run the hook.
    h.cmd()
        .args(["start", "PROJ-1", "--repos", "frontend", "--no-editor"])
        .assert()
        .success();
    let after_resume = std::fs::read_to_string(copy.join("marker")).unwrap();
    assert_eq!(
        initial, after_resume,
        "hook re-ran on resume — marker was overwritten"
    );
}

#[test]
fn hooks_list_shows_configured_actions() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    append_to_config(
        &config_file(&h),
        r#"
[[repos.frontend.post_create]]
type = "copy"
path = "secret.conf"

[[repos.frontend.post_create]]
type = "run"
cmd = "pnpm install"
name = "deps"
"#,
    );

    h.cmd()
        .args(["config", "repo", "hooks", "list", "frontend"])
        .assert()
        .success()
        .stdout(predicate::str::contains("0. copy secret.conf"))
        .stdout(predicate::str::contains("1. run [worktree] deps"));
}

#[test]
fn hooks_list_for_repo_without_hooks_says_so() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args(["config", "repo", "hooks", "list", "frontend"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(no hooks configured"));
}

#[test]
fn config_edit_invokes_editor_and_validates_result() {
    let h = Harness::new();
    h.config_set_base_dir();

    // A scripted editor: a tiny shell script that appends a known repo to the file.
    let editor_script = h.home.path().join("editor.sh");
    std::fs::write(
        &editor_script,
        r#"#!/bin/sh
cat >> "$1" <<'EOF'

[repos.via-edit]
path = "via-edit"
editor = "zed {path}"
EOF
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&editor_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    h.cmd()
        .env("EDITOR", editor_script.to_string_lossy().to_string())
        .args(["config", "edit"])
        .assert()
        .success();

    h.cmd()
        .args(["config", "repo", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("via-edit"));
}

#[test]
fn config_edit_rejects_invalid_toml() {
    let h = Harness::new();
    h.config_set_base_dir();

    let editor_script = h.home.path().join("bad-editor.sh");
    std::fs::write(
        &editor_script,
        r#"#!/bin/sh
echo "not = valid toml [[" > "$1"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&editor_script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    h.cmd()
        .env("EDITOR", editor_script.to_string_lossy().to_string())
        .args(["config", "edit"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("does not parse"));

    // Original file is untouched.
    let raw = std::fs::read_to_string(config_file(&h)).unwrap();
    assert!(raw.contains("base_dir"), "original config was clobbered");
}
