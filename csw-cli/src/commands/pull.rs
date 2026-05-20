use crate::cli::PullArgs;
use crate::{output, progress};
use anyhow::Result;
use csw_core::config::Config;
use csw_core::ops::{self, PullRequest};

pub fn run(args: PullArgs) -> Result<i32> {
    let config = Config::load()?;

    let repos: Vec<String> = if args.repos.is_empty() {
        config.repos.keys().cloned().collect()
    } else {
        args.repos
    };

    if repos.is_empty() {
        output::step("no repos configured; nothing to pull");
        return Ok(0);
    }

    let request = PullRequest { repos };
    let reporter = progress::pick(output::is_quiet() || output::is_verbose());
    let report = ops::pull::pull(&config, request, reporter.as_ref())?;
    drop(reporter);

    for (repo, err) in &report.failures {
        output::error(format!("{repo}: {err:#}"));
    }
    let total = report.successes.len() + report.failures.len();
    output::step(format!("pulled {}/{} repos", report.successes.len(), total));

    if report.any_failure() { Ok(2) } else { Ok(0) }
}
