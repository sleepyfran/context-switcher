use crate::cli::StatusArgs;
use anyhow::{Result, anyhow};
use csw_core::config::Config;
use csw_core::identity;
use csw_core::ops::{self, StatusRequest, WorktreeStatus};
use csw_core::paths;

pub fn run(args: StatusArgs) -> Result<i32> {
    let config = Config::load()?;
    let resolved_user = identity::resolve_username(&config)?;
    let (user, task_id) = match args.task_id {
        Some(t) => ops::parse_task_input(&t, &resolved_user),
        None => {
            let cwd = std::env::current_dir()?;
            let inferred = paths::infer_task_from_path(&config, &resolved_user, &cwd)
                .ok_or_else(|| anyhow!("not in a task worktree; pass an explicit task-id"))?;
            (resolved_user.clone(), inferred.1)
        }
    };

    let request = StatusRequest {
        task_id: task_id.clone(),
        user: user.clone(),
    };
    let report = ops::status::status(&config, &request)?;

    if report.worktrees.is_empty() {
        println!("no worktrees found for {user}/{task_id}");
        return Ok(0);
    }

    println!("task {user}/{task_id}");
    for c in &report.worktrees {
        render_worktree(c);
    }
    Ok(0)
}

fn render_worktree(c: &WorktreeStatus) {
    println!();
    println!("  [{}] {}", c.repo, c.worktree_path.display());
    println!("    branch: {}  base: {}", c.branch, c.base_branch);
    println!(
        "    dirty: {}",
        if c.dirty.is_empty() {
            "no".to_string()
        } else {
            format!(
                "yes ({} entr{})",
                c.dirty.len(),
                if c.dirty.len() == 1 { "y" } else { "ies" }
            )
        }
    );
    match c.ahead_behind {
        None => println!("    upstream: (not set)"),
        Some((ahead, behind)) => println!("    ahead/behind upstream: +{ahead} / -{behind}"),
    }
    println!(
        "    merged into {}: {}",
        c.base_branch,
        if c.merged_into_base { "yes" } else { "no" }
    );
}
