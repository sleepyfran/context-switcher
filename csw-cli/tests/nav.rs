mod common;

use common::Harness;
use predicates::prelude::*;

fn started(h: &Harness, task: &str, repos: &[&str]) {
    let mut args: Vec<String> = vec!["start".into(), task.into()];
    if !repos.is_empty() {
        args.push("--repos".into());
        args.push(repos.join(","));
    }
    args.push("--no-editor".into());
    h.cmd().args(&args).assert().success();
}

#[test]
fn nav_print_path_emits_copy_path_for_single_repo_task() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"]);

    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    h.cmd()
        .args(["nav", "PROJ-1", "--print-path"])
        .assert()
        .success()
        .stdout(predicate::str::contains(copy.to_string_lossy().to_string()));
}

#[test]
fn nav_print_path_with_repo_flag_picks_correct_copy() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    h.add_repo("backend", "");
    started(&h, "PROJ-1", &["frontend", "backend"]);

    let backend_copy = h.worktree_path("backend", "alice", "PROJ-1");
    h.cmd()
        .args(["nav", "PROJ-1", "--repo", "backend", "--print-path"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            backend_copy.to_string_lossy().to_string(),
        ));
}

#[test]
fn nav_print_path_without_repo_flag_errors_for_multi_repo_in_non_tty() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    h.add_repo("backend", "");
    started(&h, "PROJ-1", &["frontend", "backend"]);

    h.cmd()
        .args(["nav", "PROJ-1", "--print-path"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("pass --repo"));
}

#[test]
fn nav_unknown_task_errors() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args(["nav", "DOESNOTEXIST", "--print-path"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no worktrees found"));
}

#[test]
fn nav_unknown_repo_errors() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"]);

    h.cmd()
        .args(["nav", "PROJ-1", "--repo", "ghost", "--print-path"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("doesn't have a worktree for repo"));
}

#[test]
fn nav_no_arg_with_zero_tasks_errors() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");

    h.cmd()
        .args(["nav", "--print-path"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("nothing to navigate to"));
}

#[test]
fn nav_no_arg_with_one_task_uses_it_silently() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"]);

    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    h.cmd()
        .args(["nav", "--print-path"])
        .assert()
        .success()
        .stdout(predicate::str::contains(copy.to_string_lossy().to_string()));
}

#[test]
fn nav_no_arg_with_many_tasks_errors_in_non_tty() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"]);
    started(&h, "PROJ-2", &["frontend"]);

    h.cmd()
        .args(["nav", "--print-path"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("stderr is not a terminal"));
}

#[test]
fn nav_subshell_inherits_csw_env_vars_and_cwd() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"]);

    // Use a fake "shell" script as $SHELL: it captures cwd and the
    // CSW_TASK_ID env var into files inside the copy, then exits 0.
    let fake_shell = h.home.path().join("fake-shell.sh");
    std::fs::write(
        &fake_shell,
        r#"#!/bin/sh
pwd > "$PWD/.nav-pwd"
printf '%s|%s|%s' "$CSW_TASK_ID" "$CSW_REPO" "$CSW_USER" > "$PWD/.nav-env"
exit 0
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    h.cmd()
        .env("SHELL", fake_shell.to_string_lossy().to_string())
        .args(["nav", "PROJ-1"])
        .assert()
        .success();

    let copy = h.worktree_path("frontend", "alice", "PROJ-1");
    let pwd = std::fs::read_to_string(copy.join(".nav-pwd")).unwrap();
    let env = std::fs::read_to_string(copy.join(".nav-env")).unwrap();
    let pwd_basename = pwd.trim().rsplit('/').next().unwrap();
    assert_eq!(pwd_basename, "alice-PROJ-1");
    assert_eq!(env, "PROJ-1|frontend|alice");
}

#[test]
fn nav_subshell_propagates_exit_code() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"]);

    let fake_shell = h.home.path().join("exit7.sh");
    std::fs::write(&fake_shell, "#!/bin/sh\nexit 7\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    h.cmd()
        .env("SHELL", fake_shell.to_string_lossy().to_string())
        .args(["nav", "PROJ-1"])
        .assert()
        .code(7);
}

#[test]
fn nav_subshell_prints_entry_banner() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"]);

    let fake_shell = h.home.path().join("noop-shell.sh");
    std::fs::write(&fake_shell, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    h.cmd()
        .env("SHELL", fake_shell.to_string_lossy().to_string())
        .args(["nav", "PROJ-1"])
        .assert()
        .success()
        .stderr(predicate::str::contains("entered task alice/PROJ-1"))
        .stderr(predicate::str::contains("exit to leave"));
}

#[test]
fn nav_quiet_suppresses_banner() {
    let h = Harness::new();
    h.config_set_base_dir();
    h.config_set_username("alice");
    h.add_repo("frontend", "");
    started(&h, "PROJ-1", &["frontend"]);

    let fake_shell = h.home.path().join("noop2.sh");
    std::fs::write(&fake_shell, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    h.cmd()
        .env("SHELL", fake_shell.to_string_lossy().to_string())
        .args(["--quiet", "nav", "PROJ-1"])
        .assert()
        .success()
        .stderr(predicate::str::contains("entered task").not());
}
