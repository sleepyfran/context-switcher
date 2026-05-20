use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn csw(home: &TempDir, xdg: &TempDir) -> Command {
    let mut c = Command::cargo_bin("csw").unwrap();
    c.env("HOME", home.path())
        .env("XDG_CONFIG_HOME", xdg.path())
        .env_remove("USER")
        .env_remove("USERNAME");
    c
}

#[test]
fn show_on_empty_config_emits_defaults() {
    let home = TempDir::new().unwrap();
    let xdg = TempDir::new().unwrap();

    csw(&home, &xdg)
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("base_dir"));
}

#[test]
fn add_then_list_shows_repo() {
    let home = TempDir::new().unwrap();
    let xdg = TempDir::new().unwrap();

    csw(&home, &xdg)
        .args([
            "config",
            "repo",
            "add",
            "frontend",
            "--path",
            "frontend",
            "--editor",
            "zed {path}",
            "--default",
        ])
        .assert()
        .success();

    csw(&home, &xdg)
        .args(["config", "repo", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("frontend"))
        .stdout(predicate::str::contains("zed {path}"))
        .stdout(predicate::str::contains("yes")); // marked default
}

#[test]
fn duplicate_add_fails() {
    let home = TempDir::new().unwrap();
    let xdg = TempDir::new().unwrap();

    csw(&home, &xdg)
        .args([
            "config",
            "repo",
            "add",
            "frontend",
            "--path",
            "frontend",
            "--editor",
            "zed {path}",
        ])
        .assert()
        .success();

    csw(&home, &xdg)
        .args([
            "config",
            "repo",
            "add",
            "frontend",
            "--path",
            "frontend",
            "--editor",
            "zed {path}",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn set_unknown_field_fails() {
    let home = TempDir::new().unwrap();
    let xdg = TempDir::new().unwrap();

    csw(&home, &xdg)
        .args([
            "config",
            "repo",
            "add",
            "frontend",
            "--path",
            "p",
            "--editor",
            "zed {path}",
        ])
        .assert()
        .success();

    csw(&home, &xdg)
        .args(["config", "repo", "set", "frontend", "bogus", "x"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown field"));
}

#[test]
fn remove_strips_from_default_repos() {
    let home = TempDir::new().unwrap();
    let xdg = TempDir::new().unwrap();

    csw(&home, &xdg)
        .args([
            "config",
            "repo",
            "add",
            "frontend",
            "--path",
            "p",
            "--editor",
            "zed {path}",
            "--default",
        ])
        .assert()
        .success();

    csw(&home, &xdg)
        .args(["config", "repo", "remove", "frontend"])
        .assert()
        .success();

    let out = csw(&home, &xdg).args(["config", "show"]).output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        !stdout.contains("frontend"),
        "default_repos should no longer reference removed repo: {stdout}"
    );
}

#[test]
fn default_command_replaces_default_repos() {
    let home = TempDir::new().unwrap();
    let xdg = TempDir::new().unwrap();

    for name in ["a", "b"] {
        csw(&home, &xdg)
            .args([
                "config",
                "repo",
                "add",
                name,
                "--path",
                "p",
                "--editor",
                "zed {path}",
            ])
            .assert()
            .success();
    }

    csw(&home, &xdg)
        .args(["config", "repo", "default", "b", "a"])
        .assert()
        .success();

    let out = csw(&home, &xdg).args(["config", "show"]).output().unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    // The TOML emitter may format default_repos either inline or multi-line.
    // Normalise whitespace before checking order.
    let normalised: String = stdout.split_whitespace().collect();
    assert!(
        normalised.contains(r#"default_repos=["b","a",]"#)
            || normalised.contains(r#"default_repos=["b","a"]"#),
        "{stdout}"
    );
}

#[test]
fn default_with_unknown_repo_fails_and_does_not_persist() {
    let home = TempDir::new().unwrap();
    let xdg = TempDir::new().unwrap();

    csw(&home, &xdg)
        .args(["config", "repo", "default", "ghost"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown repo"));

    // Show still works and default_repos is untouched.
    csw(&home, &xdg)
        .args(["config", "show"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ghost").not());
}

#[test]
fn cmux_block_in_config_round_trips_through_show() {
    let home = TempDir::new().unwrap();
    let xdg = TempDir::new().unwrap();

    // Lay down a config with a per-repo cmux layout by hand. We exercise
    // the file-edit path the way a user would: edit the TOML, then verify
    // `csw config show` echoes it back.
    let cfg_dir = xdg.path().join("context-switcher");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        r#"
base_dir = "/tmp/dev"

[cmux]
enabled = true

[repos.frontend]
path = "frontend"
editor = ""

[repos.frontend.cmux]
panes = [
  { cmd = "pnpm dev" },
  { cmd = "claude", split = "right" },
]
"#,
    )
    .unwrap();

    let out = csw(&home, &xdg)
        .args(["config", "show"])
        .assert()
        .success()
        .get_output()
        .clone();
    let stdout = String::from_utf8(out.stdout).unwrap();
    // Don't depend on the emitter's choice of inline vs array-of-tables;
    // just check the values survived the load + serialise round-trip.
    assert!(stdout.contains(r#""pnpm dev""#), "{stdout}");
    assert!(stdout.contains(r#""claude""#), "{stdout}");
    assert!(stdout.contains(r#""right""#), "{stdout}");
    assert!(stdout.contains("[cmux]"), "{stdout}");
}
