//! Verbosity-aware logging helpers used by every command.

use crate::cli::Verbosity;
use std::cell::Cell;

thread_local! {
    static LEVEL: Cell<Level> = const { Cell::new(Level::Normal) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    Quiet,
    Normal,
    Verbose,
}

pub fn install(v: Verbosity) {
    let level = if v.quiet {
        Level::Quiet
    } else if v.verbose {
        Level::Verbose
    } else {
        Level::Normal
    };
    LEVEL.with(|l| l.set(level));
}

fn current() -> Level {
    LEVEL.with(|l| l.get())
}

pub fn is_quiet() -> bool {
    current() == Level::Quiet
}

pub fn is_verbose() -> bool {
    current() == Level::Verbose
}

pub fn step(msg: impl std::fmt::Display) {
    if current() != Level::Quiet {
        eprintln!("• {msg}");
    }
}

#[allow(dead_code)] // used by commands implemented in later phases
pub fn debug(msg: impl std::fmt::Display) {
    if current() == Level::Verbose {
        eprintln!("  {msg}");
    }
}

#[allow(dead_code)] // used by commands implemented in later phases
pub fn warn(msg: impl std::fmt::Display) {
    eprintln!("warning: {msg}");
}

pub fn error(msg: impl std::fmt::Display) {
    eprintln!("error: {msg}");
}
