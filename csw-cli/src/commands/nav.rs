use crate::cli::NavArgs;
use crate::output;
use anyhow::{Result, bail};
use csw_core::config::{Config, RepoConfig};
use csw_core::ops::{self, ListRequest, RepoEntry, TaskEntry};
use csw_core::{git, identity, paths, shell};
use dialoguer::Select;
use dialoguer::theme::ColorfulTheme;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

pub fn run(args: NavArgs) -> Result<i32> {
    let config = Config::load()?;
    if let Some(warning) = ops::legacy_warning(&config) {
        crate::output::warn(warning);
    }
    let resolved_user = identity::resolve_username(&config)?;

    let task = resolve_task(&config, &resolved_user, args.task_id.as_deref())?;

    if args.root {
        let root = task_root(&task)?;
        if args.print_path {
            println!("{}", root.display());
            return Ok(0);
        }
        return enter_task_root(&resolved_user, &task, &root);
    }

    let chosen = pick_repo(&task, args.repo.as_deref())?;

    if args.print_path {
        println!("{}", chosen.worktree_path.display());
        return Ok(0);
    }

    enter_subshell(&config, &resolved_user, &task, &chosen)
}

/// The folder holding every repo copy of a task: the parent of any of its
/// worktrees. Derived from disk rather than recomputed from config so it
/// keeps matching whatever `list` actually found.
fn task_root(task: &TaskEntry) -> Result<PathBuf> {
    task.repos
        .first()
        .and_then(|r| r.worktree_path.parent())
        .map(Path::to_path_buf)
        .ok_or_else(|| anyhow::anyhow!("task {} has no worktrees on disk", task.task_id))
}

fn resolve_task(config: &Config, user: &str, task_arg: Option<&str>) -> Result<TaskEntry> {
    if let Some(raw) = task_arg {
        let (parsed_user, task_id) = ops::parse_task_input(raw, user);
        let entries = ops::list::list(
            config,
            &ListRequest {
                user: parsed_user,
                only_repo: None,
            },
        )?;
        let found = entries.into_iter().find(|t| t.task_id == task_id);
        return found.ok_or_else(|| anyhow::anyhow!("no worktrees found for task {task_id}"));
    }

    let entries = ops::list::list(
        config,
        &ListRequest {
            user: user.to_string(),
            only_repo: None,
        },
    )?;
    match entries.len() {
        0 => bail!("no task worktrees on disk; nothing to navigate to"),
        1 => Ok(entries.into_iter().next().expect("len==1")),
        _ => prompt_task(entries),
    }
}

fn prompt_task(entries: Vec<TaskEntry>) -> Result<TaskEntry> {
    if !std::io::stderr().is_terminal() {
        bail!(
            "multiple tasks on disk and no task-id given; pick one explicitly when stderr is not a terminal"
        );
    }
    let labels: Vec<String> = entries
        .iter()
        .map(|t| {
            let repos: Vec<&str> = t.repos.iter().map(|r| r.repo.as_str()).collect();
            let title = t.title.as_deref().unwrap_or("(no title)");
            format!("{}  {}  [{}]", t.task_id, title, repos.join(", "))
        })
        .collect();
    let chosen = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("which task?")
        .items(&labels)
        .default(0)
        .interact()?;
    Ok(entries.into_iter().nth(chosen).expect("valid index"))
}

fn pick_repo(task: &TaskEntry, requested: Option<&str>) -> Result<RepoEntry> {
    if let Some(name) = requested {
        return task
            .repos
            .iter()
            .find(|r| r.repo == name)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "task {} doesn't have a worktree for repo `{name}` (has: {})",
                    task.task_id,
                    task.repos
                        .iter()
                        .map(|r| r.repo.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            });
    }
    match task.repos.len() {
        0 => bail!("task {} has no worktrees on disk", task.task_id),
        1 => Ok(task.repos[0].clone()),
        _ => prompt_repo(task),
    }
}

fn prompt_repo(task: &TaskEntry) -> Result<RepoEntry> {
    if !std::io::stderr().is_terminal() {
        bail!(
            "task {} spans multiple repos; pass --repo to pick one ({} available)",
            task.task_id,
            task.repos.len()
        );
    }
    let labels: Vec<&str> = task.repos.iter().map(|r| r.repo.as_str()).collect();
    let chosen = Select::with_theme(&ColorfulTheme::default())
        .with_prompt(format!("which repo for {}", task.task_id))
        .items(&labels)
        .default(0)
        .interact()?;
    Ok(task.repos[chosen].clone())
}

fn enter_subshell(
    config: &Config,
    user: &str,
    task: &TaskEntry,
    chosen: &RepoEntry,
) -> Result<i32> {
    let repo: &RepoConfig = config.repo(&chosen.repo).ok_or_else(|| {
        anyhow::anyhow!(
            "repo `{}` is no longer in config; run `csw config repo list` to check",
            chosen.repo
        )
    })?;
    let canonical = config.canonical_path(repo);
    let branch = git::current_branch(&chosen.worktree_path).unwrap_or_else(|_| {
        // Fallback: derive from the standard convention if we can't read git.
        paths::branch_name(user, &task.task_id)
    });

    output::step(format!(
        "entered task {}/{} [{}] ({}) — exit to leave",
        user, task.task_id, chosen.repo, branch
    ));

    let env = env_for_subshell(EnvCtx {
        task_root: task_root(task)?,
        worktree: chosen.worktree_path.clone(),
        canonical,
        task_id: task.task_id.clone(),
        branch,
        user: user.to_string(),
        repo: chosen.repo.clone(),
    });

    shell::spawn_subshell(&chosen.worktree_path, &env)
}

fn enter_task_root(user: &str, task: &TaskEntry, root: &Path) -> Result<i32> {
    let repos: Vec<&str> = task.repos.iter().map(|r| r.repo.as_str()).collect();

    output::step(format!(
        "entered task {}/{} root [{}] — exit to leave",
        user,
        task.task_id,
        repos.join(", ")
    ));

    let mut env = HashMap::new();
    env.insert("CSW_TASK_ROOT".into(), root.display().to_string());
    env.insert("CSW_TASK_ID".into(), task.task_id.clone());
    env.insert("CSW_USER".into(), user.to_string());
    env.insert("CSW_REPOS".into(), repos.join(","));

    shell::spawn_subshell(root, &env)
}

struct EnvCtx {
    task_root: PathBuf,
    worktree: PathBuf,
    canonical: PathBuf,
    task_id: String,
    branch: String,
    user: String,
    repo: String,
}

fn env_for_subshell(ctx: EnvCtx) -> HashMap<String, String> {
    let mut env = HashMap::new();
    env.insert("CSW_TASK_ROOT".into(), ctx.task_root.display().to_string());
    env.insert("CSW_WORKTREE".into(), ctx.worktree.display().to_string());
    env.insert("CSW_CANONICAL".into(), ctx.canonical.display().to_string());
    env.insert("CSW_TASK_ID".into(), ctx.task_id);
    env.insert("CSW_BRANCH".into(), ctx.branch);
    env.insert("CSW_USER".into(), ctx.user);
    env.insert("CSW_REPO".into(), ctx.repo);
    env
}
