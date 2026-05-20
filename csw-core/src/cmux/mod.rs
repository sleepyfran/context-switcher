//! CMux integration: per-task workspaces composed from the repos that make
//! up a task.
//!
//! Triggered when `csw start` is invoked from inside a CMux surface
//! (detected via the `CMUX_WORKSPACE_ID` env var). Builds a workspace named
//! `csw/<task-id>` and lays out one pane per participating repo, with each
//! repo's per-repo layout config controlling the splits and tabs inside its
//! slot. Re-runs are idempotent: if the workspace already exists, it's
//! focused and the build is skipped.
//!
//! Failure is always soft. CMux is enhancement; csw's exit code never
//! reflects a CMux-side problem — every failure surfaces as a
//! [`CmuxOutcome::Warned`] that the CLI renders as a warning line.

pub mod build;
pub mod client;
pub mod config;

pub use build::{BuildOptions, BuildOutcome, Contributor};
pub use client::{CmuxClient, CmuxError};
pub use config::{GlobalCmuxConfig, PaneSpec, RepoCmuxConfig, SplitDirection, TabSpec};

/// Environment variable CMux auto-exports inside every surface. Its presence
/// is how we know csw is being invoked from inside CMux.
pub const WORKSPACE_ENV_VAR: &str = "CMUX_WORKSPACE_ID";

/// User-facing outcome of a setup or close attempt. Always returned, never
/// raised — the CLI inspects this to decide what to print, and csw's exit
/// code never depends on it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CmuxOutcome {
    /// CMux integration was not invoked (not in CMux, globally disabled, or
    /// suppressed by `--no-cmux`). The caller should print nothing.
    #[default]
    NotApplicable,
    /// No selected repos had a layout configured. No workspace touched.
    NoContributors,
    /// A fresh workspace was built and is now focused.
    Created { name: String },
    /// The current workspace was simple (single pane, single surface) and
    /// got reshaped in place rather than spawning a new sidebar entry.
    Adopted { name: String },
    /// An existing workspace by the same name was reused.
    Reused { name: String },
    /// The workspace was found and closed.
    Closed { name: String },
    /// `close` was called but no matching workspace existed.
    NotClosed,
    /// The workspace's sidebar label was updated to `name`.
    Renamed { name: String },
    /// `rename` was called but no matching workspace existed.
    NotRenamed,
    /// Something went wrong on the CMux side. The CLI should print this as
    /// a single warning line and exit unchanged.
    Warned(String),
}

/// True when csw is currently running inside a CMux surface.
pub fn detect() -> bool {
    std::env::var_os(WORKSPACE_ENV_VAR).is_some()
}

/// Build (or reuse) the task's workspace. Always returns a `CmuxOutcome` —
/// errors become [`CmuxOutcome::Warned`] for the caller to display.
///
/// `title` is the human-readable label to use in the sidebar; pass `None`
/// to fall back to just the task id. `options` controls the in-place
/// adoption behavior — see [`BuildOptions`].
pub fn setup(
    task_id: &str,
    title: Option<&str>,
    contributors: &[Contributor],
    options: BuildOptions,
) -> CmuxOutcome {
    if contributors.is_empty() {
        return CmuxOutcome::NoContributors;
    }
    match client::connect() {
        Ok(mut client) => setup_with(&mut client, task_id, title, contributors, options),
        Err(e) => CmuxOutcome::Warned(format!("cmux: socket connect failed: {e}")),
    }
}

/// Inner setup that operates on an already-connected client. Exposed for
/// integration tests that drive a recording client end-to-end.
pub fn setup_with(
    client: &mut dyn CmuxClient,
    task_id: &str,
    title: Option<&str>,
    contributors: &[Contributor],
    options: BuildOptions,
) -> CmuxOutcome {
    match build::build_workspace(client, task_id, title, contributors, options) {
        Ok(BuildOutcome::Created { name, .. }) => CmuxOutcome::Created { name },
        Ok(BuildOutcome::Adopted { name, .. }) => CmuxOutcome::Adopted { name },
        Ok(BuildOutcome::Reused { name, .. }) => CmuxOutcome::Reused { name },
        Ok(BuildOutcome::NoContributors) => CmuxOutcome::NoContributors,
        Err(e) => CmuxOutcome::Warned(format!("cmux: {e}")),
    }
}

/// Close the workspace for this task, if it exists.
pub fn close(task_id: &str) -> CmuxOutcome {
    match client::connect() {
        Ok(mut client) => close_with(&mut client, task_id),
        Err(e) => CmuxOutcome::Warned(format!("cmux: socket connect failed: {e}")),
    }
}

pub fn close_with(client: &mut dyn CmuxClient, task_id: &str) -> CmuxOutcome {
    match build::close_workspace_for(client, task_id) {
        Ok(true) => CmuxOutcome::Closed {
            // We no longer have the workspace title at this point (it might
            // have been a custom one), so just use the task id for display.
            name: task_id.to_string(),
        },
        Ok(false) => CmuxOutcome::NotClosed,
        Err(e) => CmuxOutcome::Warned(format!("cmux: {e}")),
    }
}

/// Rename the workspace for this task, computing the new label from
/// [`build::workspace_name_for`]. Returns [`CmuxOutcome::NotRenamed`] when
/// no workspace matches the task id.
pub fn rename(task_id: &str, title: Option<&str>) -> CmuxOutcome {
    match client::connect() {
        Ok(mut client) => rename_with(&mut client, task_id, title),
        Err(e) => CmuxOutcome::Warned(format!("cmux: socket connect failed: {e}")),
    }
}

pub fn rename_with(client: &mut dyn CmuxClient, task_id: &str, title: Option<&str>) -> CmuxOutcome {
    match build::rename_workspace_for(client, task_id, title) {
        Ok(Some(name)) => CmuxOutcome::Renamed { name },
        Ok(None) => CmuxOutcome::NotRenamed,
        Err(e) => CmuxOutcome::Warned(format!("cmux: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmux::client::Workspace;
    use crate::cmux::client::testing::RecordingClient;
    use crate::cmux::config::{PaneSpec, RepoCmuxConfig};
    use std::path::PathBuf;

    fn one_contributor() -> Contributor {
        Contributor {
            repo: "fe".into(),
            worktree_path: PathBuf::from("/csw/tasks/fe/alice-PROJ-1"),
            layout: RepoCmuxConfig {
                panes: vec![PaneSpec {
                    cmd: Some("pnpm dev".into()),
                    split: None,
                    size: None,
                    tabs: Vec::new(),
                }],
            },
        }
    }

    #[test]
    fn setup_with_empty_contributors_returns_no_contributors() {
        // We never connect, so the socket state is irrelevant.
        let outcome = setup("PROJ-1", None, &[], BuildOptions::default());
        assert_eq!(outcome, CmuxOutcome::NoContributors);
    }

    #[test]
    fn setup_with_recording_client_returns_created() {
        let mut client = RecordingClient::new();
        let outcome = setup_with(
            &mut client,
            "PROJ-1",
            None,
            &[one_contributor()],
            BuildOptions::default(),
        );
        assert!(matches!(outcome, CmuxOutcome::Created { ref name } if name == "PROJ-1"));
    }

    #[test]
    fn setup_with_recording_client_reuses_existing() {
        let mut client = RecordingClient::new().with_existing(vec![Workspace {
            id: "ws-existing".into(),
            name: "PROJ-1".into(),
        }]);
        let outcome = setup_with(
            &mut client,
            "PROJ-1",
            None,
            &[one_contributor()],
            BuildOptions::default(),
        );
        assert!(matches!(outcome, CmuxOutcome::Reused { ref name } if name == "PROJ-1"));
    }

    #[test]
    fn close_with_no_matching_workspace_returns_not_closed() {
        let mut client = RecordingClient::new();
        let outcome = close_with(&mut client, "PROJ-1");
        assert_eq!(outcome, CmuxOutcome::NotClosed);
    }

    #[test]
    fn rename_with_no_matching_workspace_returns_not_renamed() {
        let mut client = RecordingClient::new();
        let outcome = rename_with(&mut client, "PROJ-1", Some("Fix bug"));
        assert_eq!(outcome, CmuxOutcome::NotRenamed);
    }

    #[test]
    fn rename_with_matching_workspace_returns_renamed() {
        let mut client = RecordingClient::new().with_existing(vec![Workspace {
            id: "ws".into(),
            name: "PROJ-1".into(),
        }]);
        let outcome = rename_with(&mut client, "PROJ-1", Some("Fix bug"));
        assert!(matches!(outcome, CmuxOutcome::Renamed { ref name } if name == "PROJ-1 · Fix bug"));
    }

    #[test]
    fn close_with_matching_workspace_returns_closed() {
        let mut client = RecordingClient::new().with_existing(vec![Workspace {
            id: "ws".into(),
            name: "PROJ-1".into(),
        }]);
        let outcome = close_with(&mut client, "PROJ-1");
        assert!(matches!(outcome, CmuxOutcome::Closed { ref name } if name == "PROJ-1"));
    }
}
