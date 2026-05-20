use crate::cli::{ConfigCommand, HooksCommand, RepoCommand};
use crate::output;
use anyhow::{Context, Result, bail};
use csw_core::config::{Config, RepoConfig, config_path, expand_home};
use csw_core::hooks::{CopyAction, HookAction, RunAction, RunCwd};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Select};
use std::path::{Component, Path, PathBuf};

pub fn run(cmd: ConfigCommand) -> Result<()> {
    match cmd {
        ConfigCommand::Show => show(),
        ConfigCommand::Set { key, value } => set(&key, &value),
        ConfigCommand::Edit => edit(),
        ConfigCommand::Repo(repo_cmd) => repo(repo_cmd),
    }
}

fn show() -> Result<()> {
    let cfg = Config::load()?;
    print!("{}", cfg.to_toml()?);
    Ok(())
}

fn set(key: &str, value: &str) -> Result<()> {
    let mut cfg = Config::load()?;
    match key {
        "base_dir" => {
            cfg.base_dir = expand_home(value);
        }
        "tasks_dir" => {
            cfg.tasks_dir = expand_home(value);
        }
        "username" => {
            cfg.username = if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            };
        }
        "default_repos" => {
            cfg.default_repos = parse_repo_list(value);
            validate_default_repos(&cfg)?;
        }
        other => bail!(
            "unknown key '{other}' (expected: base_dir | tasks_dir | username | default_repos)"
        ),
    }
    cfg.save()?;
    output::step(format!("set {key}"));
    Ok(())
}

fn parse_repo_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn validate_default_repos(cfg: &Config) -> Result<()> {
    for name in &cfg.default_repos {
        if !cfg.repos.contains_key(name) {
            bail!("default_repos references unknown repo '{name}'");
        }
    }
    Ok(())
}

fn repo(cmd: RepoCommand) -> Result<()> {
    match cmd {
        RepoCommand::Add {
            name,
            path,
            editor,
            base_branch,
            default,
        } => repo_add(name, path, editor, base_branch, default),
        RepoCommand::Set { name, field, value } => repo_set(name, field, value),
        RepoCommand::Remove { name } => repo_remove(name),
        RepoCommand::List => repo_list(),
        RepoCommand::Default { names } => repo_default(names),
        RepoCommand::Hooks(hooks_cmd) => hooks(hooks_cmd),
    }
}

fn hooks(cmd: HooksCommand) -> Result<()> {
    match cmd {
        HooksCommand::Add { repo } => hooks_add(repo),
        HooksCommand::List { repo } => hooks_list(repo),
        HooksCommand::Remove { repo } => hooks_remove(repo),
        HooksCommand::Clear { repo } => hooks_clear(repo),
    }
}

fn repo_add(
    name: String,
    path: String,
    editor: String,
    base_branch: Option<String>,
    default: bool,
) -> Result<()> {
    let mut cfg = Config::load()?;
    if cfg.repos.contains_key(&name) {
        bail!("repo '{name}' already exists");
    }
    let repo = RepoConfig {
        path: expand_home(&path),
        editor,
        base_branch,
        post_create: Vec::new(),
        cmux: None,
    };
    cfg.repos.insert(name.clone(), repo);
    if default && !cfg.default_repos.contains(&name) {
        cfg.default_repos.push(name.clone());
    }
    cfg.save()?;
    output::step(format!("added repo '{name}'"));
    Ok(())
}

fn repo_set(name: String, field: String, value: String) -> Result<()> {
    let mut cfg = Config::load()?;
    let repo = cfg
        .repos
        .get_mut(&name)
        .with_context(|| format!("repo '{name}' not found"))?;
    match field.as_str() {
        "path" => repo.path = expand_home(&value),
        "editor" => repo.editor = value,
        "base_branch" => repo.base_branch = if value.is_empty() { None } else { Some(value) },
        other => bail!("unknown field '{other}' (expected: path | editor | base_branch)"),
    }
    cfg.save()?;
    output::step(format!("updated repo '{name}'.{field}"));
    Ok(())
}

fn repo_remove(name: String) -> Result<()> {
    let mut cfg = Config::load()?;
    if cfg.repos.remove(&name).is_none() {
        bail!("repo '{name}' not found");
    }
    cfg.default_repos.retain(|n| n != &name);
    cfg.save()?;
    output::step(format!("removed repo '{name}'"));
    Ok(())
}

fn repo_list() -> Result<()> {
    let cfg = Config::load()?;
    if cfg.repos.is_empty() {
        println!("(no repos configured)");
        return Ok(());
    }

    let header = ["NAME", "PATH", "EDITOR", "BASE", "DEFAULT"];
    let rows: Vec<[String; 5]> = cfg
        .repos
        .iter()
        .map(|(name, r)| {
            let resolved = cfg.canonical_path(r);
            [
                name.clone(),
                resolved.display().to_string(),
                r.editor.clone(),
                r.base_branch.clone().unwrap_or_else(|| "-".into()),
                if cfg.default_repos.contains(name) {
                    "yes".into()
                } else {
                    "".into()
                },
            ]
        })
        .collect();

    print_table(&header, &rows);
    Ok(())
}

fn repo_default(names: Vec<String>) -> Result<()> {
    let mut cfg = Config::load()?;
    cfg.default_repos = names;
    validate_default_repos(&cfg)?;
    cfg.save()?;
    output::step("updated default_repos");
    Ok(())
}

fn print_table(header: &[&str; 5], rows: &[[String; 5]]) {
    let mut widths = header.map(|h| h.len());
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }
    print_row(header.map(|s| s.to_string()), &widths);
    for row in rows {
        print_row(row.clone(), &widths);
    }
}

fn print_row(row: [String; 5], widths: &[usize; 5]) {
    let cells: Vec<String> = row
        .iter()
        .zip(widths.iter())
        .map(|(c, w)| format!("{c:<width$}", width = *w))
        .collect();
    println!("{}", cells.join("  "));
}

// ---------------------------------------------------------------------------
// hooks
// ---------------------------------------------------------------------------

fn hooks_add(repo_name: String) -> Result<()> {
    let mut cfg = Config::load()?;
    if !cfg.repos.contains_key(&repo_name) {
        bail!("repo '{repo_name}' not found");
    }

    let theme = ColorfulTheme::default();
    let mut added = 0usize;

    loop {
        let action = wizard_one_action(&theme)?;
        let repo = cfg.repos.get_mut(&repo_name).expect("checked above");
        repo.post_create.push(action);
        added += 1;

        let again = Confirm::with_theme(&theme)
            .with_prompt("add another?")
            .default(false)
            .interact()?;
        if !again {
            break;
        }
    }

    cfg.save()?;
    output::step(format!(
        "added {added} hook{} to {repo_name}",
        if added == 1 { "" } else { "s" }
    ));
    Ok(())
}

fn wizard_one_action(theme: &ColorfulTheme) -> Result<HookAction> {
    let kinds = ["copy", "run"];
    let kind = Select::with_theme(theme)
        .with_prompt("action type")
        .items(&kinds)
        .default(0)
        .interact()?;

    match kinds[kind] {
        "copy" => wizard_copy(theme),
        "run" => wizard_run(theme),
        _ => unreachable!(),
    }
}

fn wizard_copy(theme: &ColorfulTheme) -> Result<HookAction> {
    let path: String = Input::with_theme(theme)
        .with_prompt("source path (relative to canonical)")
        .validate_with(validate_relative_input)
        .interact_text()?;

    let differs = Confirm::with_theme(theme)
        .with_prompt("does the target path differ?")
        .default(false)
        .interact()?;

    let to_path: Option<String> = if differs {
        Some(
            Input::with_theme(theme)
                .with_prompt("target path (relative to worktree)")
                .validate_with(validate_relative_input)
                .interact_text()?,
        )
    } else {
        None
    };

    let optional = Confirm::with_theme(theme)
        .with_prompt("optional? (skip if missing)")
        .default(false)
        .interact()?;

    let action = match to_path {
        Some(t) => CopyAction {
            path: None,
            from: Some(PathBuf::from(path)),
            to: Some(PathBuf::from(t)),
            optional,
        },
        None => CopyAction {
            path: Some(PathBuf::from(path)),
            from: None,
            to: None,
            optional,
        },
    };
    Ok(HookAction::Copy(action))
}

fn wizard_run(theme: &ColorfulTheme) -> Result<HookAction> {
    let cmd: String = Input::with_theme(theme)
        .with_prompt("command")
        .validate_with(|input: &String| -> Result<(), &'static str> {
            if input.trim().is_empty() {
                Err("command cannot be empty")
            } else {
                Ok(())
            }
        })
        .interact_text()?;

    let cwds = ["worktree", "canonical"];
    let cwd_idx = Select::with_theme(theme)
        .with_prompt("working directory")
        .items(&cwds)
        .default(0)
        .interact()?;
    let cwd = if cwds[cwd_idx] == "canonical" {
        RunCwd::Canonical
    } else {
        RunCwd::Worktree
    };

    let name: String = Input::with_theme(theme)
        .with_prompt("display name (optional)")
        .allow_empty(true)
        .interact_text()?;
    let name = if name.trim().is_empty() {
        None
    } else {
        Some(name)
    };

    Ok(HookAction::Run(RunAction { cmd, cwd, name }))
}

fn validate_relative_input(input: &String) -> Result<(), String> {
    let p = Path::new(input);
    if input.trim().is_empty() {
        return Err("path cannot be empty".into());
    }
    if p.is_absolute() {
        return Err("path must be relative".into());
    }
    for c in p.components() {
        if matches!(c, Component::ParentDir) {
            return Err("path may not contain `..`".into());
        }
    }
    Ok(())
}

fn hooks_list(repo_name: String) -> Result<()> {
    let cfg = Config::load()?;
    let repo = cfg
        .repos
        .get(&repo_name)
        .with_context(|| format!("repo '{repo_name}' not found"))?;
    if repo.post_create.is_empty() {
        println!("(no hooks configured for {repo_name})");
        return Ok(());
    }
    for (i, action) in repo.post_create.iter().enumerate() {
        println!("{i}. {}", describe_action(action));
    }
    Ok(())
}

fn hooks_remove(repo_name: String) -> Result<()> {
    let mut cfg = Config::load()?;
    let repo = cfg
        .repos
        .get_mut(&repo_name)
        .with_context(|| format!("repo '{repo_name}' not found"))?;
    if repo.post_create.is_empty() {
        bail!("repo '{repo_name}' has no hooks to remove");
    }

    let labels: Vec<String> = repo
        .post_create
        .iter()
        .enumerate()
        .map(|(i, a)| format!("{i}. {}", describe_action(a)))
        .collect();

    let chosen = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("which hook to remove?")
        .items(&labels)
        .default(0)
        .interact()?;
    let removed = repo.post_create.remove(chosen);

    cfg.save()?;
    output::step(format!("removed: {}", describe_action(&removed)));
    Ok(())
}

fn hooks_clear(repo_name: String) -> Result<()> {
    let mut cfg = Config::load()?;
    let repo = cfg
        .repos
        .get_mut(&repo_name)
        .with_context(|| format!("repo '{repo_name}' not found"))?;
    if repo.post_create.is_empty() {
        println!("(no hooks to clear)");
        return Ok(());
    }
    let count = repo.post_create.len();

    let confirmed = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(format!(
            "clear all {count} hook{} from {repo_name}?",
            if count == 1 { "" } else { "s" }
        ))
        .default(false)
        .interact()?;
    if !confirmed {
        output::step("aborted");
        return Ok(());
    }

    repo.post_create.clear();
    cfg.save()?;
    output::step(format!("cleared {count} hook(s) from {repo_name}"));
    Ok(())
}

fn describe_action(action: &HookAction) -> String {
    match action {
        HookAction::Copy(c) => match (&c.path, &c.from, &c.to) {
            (Some(p), _, _) => {
                let opt = if c.optional { " (optional)" } else { "" };
                format!("copy {}{opt}", p.display())
            }
            (None, Some(f), Some(t)) => {
                let opt = if c.optional { " (optional)" } else { "" };
                format!("copy {} -> {}{opt}", f.display(), t.display())
            }
            _ => "copy (malformed)".into(),
        },
        HookAction::Run(r) => {
            let cwd = match r.cwd {
                RunCwd::Worktree => "worktree",
                RunCwd::Canonical => "canonical",
            };
            let name = r.name.clone().unwrap_or_else(|| short(&r.cmd));
            format!("run [{cwd}] {name}")
        }
    }
}

fn short(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= 60 {
        trimmed.to_string()
    } else {
        let mut out: String = trimmed.chars().take(57).collect();
        out.push_str("...");
        out
    }
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

fn edit() -> Result<()> {
    use std::io::Write;

    let path = config_path();
    let raw = if path.exists() {
        std::fs::read_to_string(&path)?
    } else {
        Config::default().to_toml()?
    };

    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".into());

    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".config-edit-")
        .suffix(".toml")
        .tempfile_in(dir)?;
    tmp.write_all(raw.as_bytes())?;
    tmp.as_file_mut().sync_all()?;

    let argv = shlex::split(&editor)
        .ok_or_else(|| anyhow::anyhow!("could not parse editor command: {editor}"))?;
    if argv.is_empty() {
        bail!("editor command is empty");
    }
    let status = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .arg(tmp.path())
        .status()
        .with_context(|| format!("spawning editor `{}`", argv[0]))?;
    if !status.success() {
        bail!("editor exited non-zero; config left unchanged");
    }

    // Validate the post-edit content parses before swapping it in.
    let new_raw = std::fs::read_to_string(tmp.path())?;
    let _: Config = toml::from_str(&new_raw)
        .context("post-edit config does not parse; original left unchanged")?;

    tmp.persist(&path)
        .map_err(|e| e.error)
        .with_context(|| format!("renaming temp config into place at {}", path.display()))?;
    output::step(format!("config saved to {}", path.display()));
    Ok(())
}
