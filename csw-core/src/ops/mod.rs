//! High-level operations exposed to the CLI.
//!
//! Each submodule defines a request/report pair plus the function that runs
//! it. The CLI is a thin formatter on top of these — no business logic
//! lives in the CLI crate.

pub mod done;
pub mod fetch;
pub mod list;
pub mod pull;
pub mod retitle;
pub mod selection;
pub mod start;
pub mod status;

pub use done::{BlockingIssue, DonePlan, DoneReport, DoneRequest, UnmergedWarning, WorktreeState};
pub use fetch::{FetchReport, FetchRequest, FetchSuccess};
pub use list::{ListRequest, RepoEntry, TaskEntry};
pub use pull::{PullReport, PullRequest, PullSuccess};
pub use retitle::{RetitleReport, RetitleRequest, RetitleSuccess, retitle};
pub use start::{
    EditorStatus, StartAction, StartReport, StartRequest, StartSuccess, parse_task_input, start,
};
pub use status::{StatusReport, StatusRequest, WorktreeStatus};
