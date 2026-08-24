use crate::cli::MigrateArgs;
use crate::{output, progress};
use anyhow::Result;
use csw_core::config::Config;
use csw_core::ops;
use dialoguer::Confirm;
use dialoguer::theme::ColorfulTheme;

pub fn run(args: MigrateArgs) -> Result<i32> {
    let config = Config::load()?;
    let legacy = ops::scan_legacy(&config)?;

    if legacy.is_empty() {
        output::step("nothing to migrate");
        return Ok(0);
    }

    for task in &legacy {
        let repos: Vec<&str> = task.repos.iter().map(|r| r.repo.as_str()).collect();
        output::step(format!(
            "{}/{}  [{}]  -> {}",
            task.user,
            task.task_id,
            repos.join(", "),
            config.task_root(&task.user, &task.task_id).display()
        ));
    }

    if args.dry_run {
        return Ok(0);
    }

    if !args.yes {
        let proceed = Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!("migrate {} task(s)?", legacy.len()))
            .default(false)
            .interact()?;
        if !proceed {
            output::step("aborted");
            return Ok(0);
        }
    }

    let reporter = progress::pick(output::is_quiet() || output::is_verbose());
    let mut any_failure = false;
    for task in &legacy {
        if let Err(e) = ops::migrate_one(&config, task, reporter.as_ref()) {
            output::error(format!("{}/{}: {e:#}", task.user, task.task_id));
            any_failure = true;
        }
    }
    drop(reporter);

    if any_failure { Ok(2) } else { Ok(0) }
}
