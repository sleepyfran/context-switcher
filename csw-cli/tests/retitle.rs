mod common;

use common::Harness;
use predicates::prelude::*;

fn started(h: &Harness, task: &str, repos: &[&str], title: Option<&str>) {
    let mut args: Vec<String> = vec!["start".into(), task.into()];
    if !repos.is_empty() {
        args.push("--repos".into());
        args.push(repos.join(","));
    }
    if let Some(t) = title {
        args.push("--title".into());
        args.push(t.into());
    }
    args.push("--no-editor".into());
    args.push("--no-cmux".into());
    h.cmd().args(&args).assert().success();
}

#[test]
fn retitle_changes_title_in_sidecar() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"], Some("Old"));

    h.cmd()
        .args(["retitle", "PROJ-1", "New title", "--no-cmux"])
        .assert()
        .success()
        .stderr(predicate::str::contains("Old"))
        .stderr(predicate::str::contains("New title"));

    let out = h
        .cmd()
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v[0]["title"], "New title");
}

#[test]
fn retitle_with_empty_string_clears_title() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"], Some("Old"));

    h.cmd()
        .args(["retitle", "PROJ-1", "", "--no-cmux"])
        .assert()
        .success();

    let out = h
        .cmd()
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(
        v[0]["title"].is_null(),
        "title should be null, got {:?}",
        v[0]["title"]
    );
}

#[test]
fn retitle_infers_task_from_cwd() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"], Some("Old"));

    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    h.cmd()
        .current_dir(&copy)
        .args(["retitle", "Inferred title", "--no-cmux"])
        .assert()
        .success();

    let out = h
        .cmd()
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v[0]["title"], "Inferred title");
}

#[test]
fn retitle_errors_when_no_copies_exist() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args(["retitle", "MISSING-1", "anything", "--no-cmux"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no worktrees found"));
}

#[test]
fn retitle_updates_every_repo_in_multi_repo_task() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    h.add_repo("backend", "");
    started(&h, "PROJ-1", &["frontend", "backend"], Some("Old"));

    h.cmd()
        .args(["retitle", "PROJ-1", "New", "--no-cmux"])
        .assert()
        .success();

    let out = h
        .cmd()
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v[0]["title"], "New");
    assert_eq!(v[0]["repos"].as_array().unwrap().len(), 2);
}
