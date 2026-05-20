//! `indicatif`-backed progress reporter for the CLI.
//!
//! Each repo gets its own spinner inside a shared [`MultiProgress`].
//! The spinner stays in place as the operation progresses through cloning,
//! fetching, and checkout, then finishes with a check or cross.
//!
//! Falls back to a no-op reporter when the user passes `--quiet` or when
//! stderr isn't a TTY (e.g. piping into a file or running under tests).

use csw_core::progress::{NullReporter, RepoProgress, Reporter};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::io::IsTerminal;
use std::time::Duration;

/// Decide which reporter to hand to a core operation.
///
/// `--quiet` always silences progress; otherwise we use indicatif when stderr
/// is a real TTY, falling back to the null reporter when output is captured.
pub fn pick(quiet: bool) -> Box<dyn Reporter> {
    if quiet || !std::io::stderr().is_terminal() {
        Box::new(NullReporter)
    } else {
        Box::new(IndicatifReporter::new())
    }
}

pub struct IndicatifReporter {
    multi: MultiProgress,
}

impl IndicatifReporter {
    pub fn new() -> Self {
        Self {
            multi: MultiProgress::new(),
        }
    }
}

impl Default for IndicatifReporter {
    fn default() -> Self {
        Self::new()
    }
}

impl Reporter for IndicatifReporter {
    fn begin(&self, repo: &str, action: &str) -> Box<dyn RepoProgress> {
        let pb = self.multi.add(ProgressBar::new_spinner());
        pb.set_style(spinner_style());
        pb.set_prefix(repo.to_string());
        pb.set_message(action.to_string());
        pb.enable_steady_tick(Duration::from_millis(80));
        Box::new(IndicatifProgress { pb })
    }
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan} [{prefix:.bold}] {wide_msg}")
        .expect("spinner template")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "⠿"])
}

fn finished_style(symbol: &str, color: &str) -> ProgressStyle {
    ProgressStyle::with_template(&format!(
        "{symbol} [{{prefix:.bold}}] {{wide_msg:.{color}}}"
    ))
    .expect("finished template")
}

struct IndicatifProgress {
    pb: ProgressBar,
}

impl RepoProgress for IndicatifProgress {
    fn step(&self, message: &str) {
        self.pb.set_message(message.to_string());
    }
    fn ok(&self, message: &str) {
        self.pb.set_style(finished_style("✓", "green"));
        self.pb.finish_with_message(message.to_string());
    }
    fn err(&self, message: &str) {
        self.pb.set_style(finished_style("✗", "red"));
        self.pb.finish_with_message(message.to_string());
    }
}
