use crate::cli::StartArgs;
use crate::{output, progress};
use anyhow::{Result, bail};
use csw_core::cmux::CmuxOutcome;
use csw_core::config::Config;
use csw_core::identity;
use csw_core::ops::{
    self, EditorStatus, ListRequest, StartAction, StartRequest, StartSuccess, TaskEntry,
};
use dialoguer::FuzzySelect;
use dialoguer::theme::ColorfulTheme;
use std::collections::BTreeMap;
use std::io::IsTerminal;

pub fn run(args: StartArgs) -> Result<i32> {
    let config = Config::load()?;
    let resolved_user = identity::resolve_username(&config)?;

    let (raw_task, repos_override) = match args.task_id {
        Some(t) => (t, None),
        None => {
            let entry = pick_existing_task(&config, &resolved_user)?;
            let repos: Vec<String> = entry.repos.iter().map(|r| r.repo.clone()).collect();
            (entry.task_id, Some(repos))
        }
    };
    let (user, task_id) = ops::parse_task_input(&raw_task, &resolved_user);

    let repos = match repos_override {
        // Resuming an existing task: use the repos it already lives in,
        // ignoring default_repos / --repos / --only. The flags don't make
        // sense in this context anyway.
        Some(r) => r,
        None => {
            let only = if args.only.is_empty() {
                None
            } else {
                Some(args.only.as_slice())
            };
            let resolved = ops::selection::resolve(&config, only, &args.repos)?;
            if resolved.is_empty() {
                bail!(
                    "no repositories selected — set default_repos in config, or pass --repos / --only"
                );
            }
            resolved
        }
    };

    let branch_overrides = parse_branch_overrides(&args.branch)?;

    let request = StartRequest {
        task_id: task_id.clone(),
        user: user.clone(),
        repos,
        title: args.title,
        spawn_editor: !args.no_editor,
        skip_hooks: args.skip_hooks,
        no_cmux: args.no_cmux,
        force_new_workspace: args.force_new_workspace,
        branch_overrides,
    };

    output::step(format!("starting task {user}/{task_id}"));
    let reporter = progress::pick(output::is_quiet() || output::is_verbose());
    let report = ops::start(&config, request, reporter.as_ref())?;
    drop(reporter);
    render_report(&report);

    if report.any_failure() { Ok(2) } else { Ok(0) }
}

/// Parse `--branch repo=branch` flags into a map. Rejects malformed
/// entries and duplicate repo keys.
fn parse_branch_overrides(input: &[String]) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for raw in input {
        let (repo, branch) = raw
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("--branch needs `repo=branch` form, got `{raw}`"))?;
        let repo = repo.trim();
        let branch = branch.trim();
        if repo.is_empty() || branch.is_empty() {
            bail!("--branch entry `{raw}` has an empty side");
        }
        if out.insert(repo.to_string(), branch.to_string()).is_some() {
            bail!("--branch given more than once for repo `{repo}`");
        }
    }
    Ok(out)
}

/// Used when `csw start` is invoked without a task argument. Lists every
/// task copy on disk and lets the user pick one, or short-circuits if there
/// are zero or one tasks.
fn pick_existing_task(config: &Config, user: &str) -> Result<TaskEntry> {
    let mut entries = ops::list::list(
        config,
        &ListRequest {
            user: user.to_string(),
            only_repo: None,
        },
    )?;

    match entries.len() {
        0 => bail!("no task copies on disk; pass a task-id to create a new one"),
        1 => {
            let only = entries.remove(0);
            output::step(format!("resuming the only on-disk task: {}", only.task_id));
            Ok(only)
        }
        _ => prompt_select(entries),
    }
}

fn prompt_select(entries: Vec<TaskEntry>) -> Result<TaskEntry> {
    if !std::io::stderr().is_terminal() {
        bail!(
            "multiple tasks on disk and no task-id given; pick one explicitly when stderr is not a terminal"
        );
    }

    let labels: Vec<String> = entries.iter().map(format_label).collect();
    let chosen = FuzzySelect::with_theme(&ColorfulTheme::default())
        .with_prompt("pick a task")
        .items(&labels)
        .default(0)
        .interact()?;
    Ok(entries.into_iter().nth(chosen).expect("valid index"))
}

fn format_label(t: &TaskEntry) -> String {
    let repos: Vec<&str> = t.repos.iter().map(|r| r.repo.as_str()).collect();
    let title = t.title.as_deref().unwrap_or("(no title)");
    format!("{}  {}  [{}]", t.task_id, title, repos.join(", "))
}

fn render_report(report: &ops::StartReport) {
    for success in &report.successes {
        render_success(success);
    }
    for (repo, err) in &report.failures {
        output::error(format!("{repo}: {err:#}"));
    }
    render_cmux(&report.cmux);
    for success in &report.successes {
        println!("{}", success.worktree_path.display());
    }
}

fn render_cmux(outcome: &CmuxOutcome) {
    match outcome {
        CmuxOutcome::NotApplicable | CmuxOutcome::NoContributors => {}
        CmuxOutcome::Created { name } => output::step(format!("cmux: opened workspace {name}")),
        CmuxOutcome::Adopted { name } => {
            output::step(format!("cmux: reshaped current workspace into {name}"))
        }
        CmuxOutcome::Reused { name } => {
            output::step(format!("cmux: focused existing workspace {name}"))
        }
        CmuxOutcome::Warned(msg) => output::warn(msg.clone()),
        // start should never produce a Closed/NotClosed/Renamed outcome.
        CmuxOutcome::Closed { .. }
        | CmuxOutcome::NotClosed
        | CmuxOutcome::Renamed { .. }
        | CmuxOutcome::NotRenamed => {}
    }
}

fn render_success(s: &StartSuccess) {
    let action = match s.action {
        StartAction::Created => "created",
        StartAction::Resumed => "resumed",
    };
    output::step(format!(
        "{} [{}] {} on {}",
        action,
        s.repo,
        s.worktree_path.display(),
        s.branch
    ));
    match &s.editor {
        EditorStatus::Spawned => output::debug(format!("editor spawned for {}", s.repo)),
        EditorStatus::Skipped => {}
        EditorStatus::Failed(e) => output::warn(format!("editor failed for {}: {e}", s.repo)),
    }
}
