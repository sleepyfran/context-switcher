use crate::cli::RetitleArgs;
use crate::{output, progress};
use anyhow::{Result, anyhow};
use csw_core::cmux::CmuxOutcome;
use csw_core::config::Config;
use csw_core::identity;
use csw_core::ops::{self, RetitleRequest};
use csw_core::paths;

pub fn run(args: RetitleArgs) -> Result<i32> {
    let config = Config::load()?;
    let resolved_user = identity::resolve_username(&config)?;

    // Two positional forms:
    //   csw retitle <TITLE>           — title only, task from cwd
    //   csw retitle <TASK> <TITLE>    — both explicit
    // Empty / whitespace title clears the existing title.
    let (user, task_id, title) = match args.title {
        Some(t) => {
            let (u, id) = ops::parse_task_input(&args.first, &resolved_user);
            (u, id, t)
        }
        None => {
            let cwd = std::env::current_dir()?;
            let (_inferred_user, id, _worktree_path) =
                paths::infer_task_from_path(&config, &resolved_user, &cwd).ok_or_else(|| {
                    anyhow!("not in a task worktree; pass `csw retitle <task-id> <title>`")
                })?;
            (resolved_user.clone(), id, args.first)
        }
    };

    let request = RetitleRequest {
        task_id: task_id.clone(),
        user: user.clone(),
        title: Some(title),
        no_cmux: args.no_cmux,
    };

    output::step(format!("retitling {user}/{task_id}"));
    let reporter = progress::pick(output::is_quiet() || output::is_verbose());
    let report = ops::retitle(&config, request, reporter.as_ref())?;
    drop(reporter);

    for (repo, err) in &report.failures {
        output::error(format!("{repo}: {err:#}"));
    }
    render_title_change(&report);
    render_cmux(&report.cmux);

    if report.any_failure() { Ok(2) } else { Ok(0) }
}

fn render_title_change(report: &ops::RetitleReport) {
    let display = |t: &Option<String>| t.clone().unwrap_or_else(|| "(no title)".into());
    output::step(format!(
        "title: {:?} → {:?}",
        display(&report.previous_title),
        display(&report.new_title),
    ));
}

fn render_cmux(outcome: &CmuxOutcome) {
    match outcome {
        CmuxOutcome::NotApplicable | CmuxOutcome::NotRenamed => {}
        CmuxOutcome::Renamed { name } => output::step(format!("cmux: workspace renamed: {name}")),
        CmuxOutcome::Warned(msg) => output::warn(msg.clone()),
        // retitle should never produce these outcomes.
        CmuxOutcome::Created { .. }
        | CmuxOutcome::Adopted { .. }
        | CmuxOutcome::Reused { .. }
        | CmuxOutcome::Closed { .. }
        | CmuxOutcome::NotClosed
        | CmuxOutcome::NoContributors => {}
    }
}
