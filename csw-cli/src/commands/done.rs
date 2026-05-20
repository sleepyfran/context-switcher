use crate::cli::DoneArgs;
use crate::{output, progress};
use anyhow::{Result, anyhow};
use csw_core::cmux::CmuxOutcome;
use csw_core::config::Config;
use csw_core::identity;
use csw_core::ops::{self, BlockingIssue, DonePlan, DoneRequest};
use csw_core::paths;
use std::io::{self, BufRead, Write};

pub fn run(args: DoneArgs) -> Result<i32> {
    let config = Config::load()?;
    let resolved_user = identity::resolve_username(&config)?;
    let (user, task_id) = match args.task_id {
        Some(t) => ops::parse_task_input(&t, &resolved_user),
        None => (
            resolved_user.clone(),
            infer_task_from_cwd(&config, &resolved_user)?,
        ),
    };

    let request = DoneRequest {
        task_id: task_id.clone(),
        user: user.clone(),
        force: args.force,
        keep_branch: args.keep_branch,
        keep_workspace: args.keep_workspace,
    };

    let plan = ops::done::plan(&config, &request)?;

    if !plan.blocking.is_empty() {
        if !request.force {
            render_blocking(&plan);
            output::error("refusing to delete; pass --force to override");
            return Ok(3);
        }
        output::warn("--force: ignoring blocking issues:");
        render_blocking(&plan);
    }

    if !plan.warnings.is_empty() && !args.yes && !request.force {
        render_warnings(&plan);
        if !confirm("proceed with delete?")? {
            output::step("aborted");
            return Ok(0);
        }
    } else if !plan.warnings.is_empty() {
        render_warnings(&plan);
    }

    let reporter = progress::pick(output::is_quiet() || output::is_verbose());
    let report = ops::done::execute(&config, &plan, &request, reporter.as_ref())
        .map_err(anyhow::Error::from)?;
    drop(reporter);
    for path in &report.deleted_worktrees {
        output::step(format!("deleted {}", path.display()));
    }
    for (repo, branch) in &report.deleted_branches {
        output::step(format!("[{repo}] removed branch {branch}"));
    }
    for (repo, err) in &report.failures {
        output::error(format!("{repo}: {err:#}"));
    }
    render_cmux(&report.cmux);
    if report.any_failure() { Ok(2) } else { Ok(0) }
}

fn render_cmux(outcome: &CmuxOutcome) {
    match outcome {
        CmuxOutcome::NotApplicable | CmuxOutcome::NotClosed => {}
        CmuxOutcome::Closed { name } => output::step(format!("cmux: closed workspace {name}")),
        CmuxOutcome::Warned(msg) => output::warn(msg.clone()),
        // done should never produce a Created/Adopted/Reused/NoContributors/Renamed outcome.
        CmuxOutcome::Created { .. }
        | CmuxOutcome::Adopted { .. }
        | CmuxOutcome::Reused { .. }
        | CmuxOutcome::NoContributors
        | CmuxOutcome::Renamed { .. }
        | CmuxOutcome::NotRenamed => {}
    }
}

fn infer_task_from_cwd(config: &Config, user: &str) -> Result<String> {
    let cwd = std::env::current_dir()?;
    let inferred = paths::infer_task_from_path(config, user, &cwd)
        .ok_or_else(|| anyhow!("not in a task worktree; pass an explicit task-id"))?;
    Ok(inferred.1)
}

fn render_blocking(plan: &DonePlan) {
    for issue in &plan.blocking {
        match issue {
            BlockingIssue::Dirty { repo, files, .. } => {
                output::error(format!(
                    "[{repo}] dirty working tree:\n    {}",
                    files.join("\n    ")
                ));
            }
            BlockingIssue::Unpushed { repo, ahead, .. } => {
                output::error(format!(
                    "[{repo}] {ahead} unpushed commit{} on this branch",
                    if *ahead == 1 { "" } else { "s" }
                ));
            }
            BlockingIssue::NoUpstream { repo, .. } => {
                output::error(format!(
                    "[{repo}] branch has no upstream — push it first or pass --force"
                ));
            }
        }
    }
}

fn render_warnings(plan: &DonePlan) {
    for w in &plan.warnings {
        output::warn(format!(
            "[{}] branch {} is pushed but not merged into {}",
            w.repo, w.branch, w.base
        ));
    }
}

fn confirm(question: &str) -> Result<bool> {
    eprint!("{question} [y/N] ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}
