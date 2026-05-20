mod cli;
mod commands;
mod output;
mod progress;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};

fn main() {
    let exit = match real_main() {
        Ok(code) => code,
        Err(e) => {
            output::error(format!("{e:#}"));
            1
        }
    };
    std::process::exit(exit);
}

fn real_main() -> Result<i32> {
    let cli = Cli::parse();
    output::install(cli.verbosity);

    match cli.command {
        Command::Start(args) => commands::start::run(args),
        Command::Nav(args) => commands::nav::run(args),
        Command::Fetch(args) => commands::fetch::run(args),
        Command::Pull(args) => commands::pull::run(args),
        Command::Done(args) => commands::done::run(args),
        Command::Status(args) => commands::status::run(args),
        Command::List(args) => commands::list::run(args),
        Command::Retitle(args) => commands::retitle::run(args),
        Command::Config(cmd) => commands::config_cmd::run(cmd).map(|_| 0),
    }
}
