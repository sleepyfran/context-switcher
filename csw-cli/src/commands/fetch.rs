use crate::cli::FetchArgs;
use crate::{output, progress};
use anyhow::Result;
use csw_core::config::Config;
use csw_core::ops::{self, FetchRequest};

pub fn run(args: FetchArgs) -> Result<i32> {
    let config = Config::load()?;

    let repos: Vec<String> = if args.repos.is_empty() {
        config.repos.keys().cloned().collect()
    } else {
        args.repos
    };

    if repos.is_empty() {
        output::step("no repos configured; nothing to fetch");
        return Ok(0);
    }

    let request = FetchRequest { repos };
    let reporter = progress::pick(output::is_quiet() || output::is_verbose());
    let report = ops::fetch::fetch(&config, request, reporter.as_ref())?;
    drop(reporter);

    for (repo, err) in &report.failures {
        output::error(format!("{repo}: {err:#}"));
    }
    let total = report.successes.len() + report.failures.len();
    output::step(format!(
        "fetched {}/{} repos",
        report.successes.len(),
        total
    ));

    if report.any_failure() { Ok(2) } else { Ok(0) }
}
