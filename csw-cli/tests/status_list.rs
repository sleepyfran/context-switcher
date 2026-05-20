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

    h.cmd().args(&args).assert().success();
}

#[test]
fn status_with_no_copies_prints_nothing_found() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args(["status", "PROJ-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("no worktrees found"));
}

#[test]
fn status_reports_clean_freshly_started_task() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"], None);

    h.cmd()
        .args(["status", "PROJ-1"])
        .assert()
        .success()
        .stdout(predicate::str::contains("alice/PROJ-1"))
        .stdout(predicate::str::contains("dirty: no"));
}

#[test]
fn status_infers_task_from_cwd() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"], None);

    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    h.cmd()
        .current_dir(&copy)
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("PROJ-1"));
}

#[test]
fn list_when_empty_prints_placeholder() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("no task copies on disk"));
}

#[test]
fn list_shows_started_tasks_in_table() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"], Some("First task"));
    started(&h, "PROJ-2", &["frontend"], None);

    h.cmd()
        .arg("list")
        .assert()
        .success()
        .stdout(predicate::str::contains("PROJ-1"))
        .stdout(predicate::str::contains("PROJ-2"))
        .stdout(predicate::str::contains("First task"));
}

#[test]
fn list_emits_json() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"], Some("My title"));

    let out = h
        .cmd()
        .args(["list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v[0]["task_id"], "PROJ-1");
    assert_eq!(v[0]["title"], "My title");
    assert_eq!(v[0]["repos"][0]["repo"], "frontend");
}

#[test]
fn list_filters_by_repo() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    h.add_repo("backend", "");
    started(&h, "PROJ-1", &["frontend"], None);
    started(&h, "PROJ-2", &["backend"], None);

    h.cmd()
        .args(["list", "--repo", "frontend", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("PROJ-1"))
        .stdout(predicate::str::contains("PROJ-2").not());
}
