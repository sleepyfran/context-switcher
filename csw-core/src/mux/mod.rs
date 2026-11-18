//! Terminal multiplexer integration: per-task workspaces composed from the
//! repos that make up a task.
//!
//! Supported providers are [CMux](https://cmux.com) and
//! [Rex](https://www.superlogical.com/rex). Which one is used is decided by
//! [`detect`] from the `[mux] provider` config (`auto` by default) and the
//! env vars each multiplexer exports into its terminals. Outside of a
//! supported multiplexer the integration is a no-op.
//!
//! The layout algorithm ([`build`]) is shared: it only talks to the
//! provider-neutral [`MuxClient`] trait, implemented by [`cmux::CmuxClient`]
//! (Unix socket) and [`rex::RexClient`] (the `rex` CLI).
//!
//! Failure is always soft. The multiplexer is an enhancement; csw's exit
//! code never reflects a multiplexer-side problem — every failure surfaces
//! as a [`MuxOutcome::Warned`] that the CLI renders as a warning line.

pub mod build;
pub mod client;
pub mod cmux;
pub mod config;
pub mod rex;

pub use build::{BuildOptions, BuildOutcome, Contributor, WorkspaceSpec};
pub use client::{MuxClient, MuxError, SplitDirection};
pub use config::{MuxConfig, ProviderChoice};

use std::fmt;

/// A concrete multiplexer csw knows how to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    Cmux,
    Rex,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cmux => "cmux",
            Self::Rex => "rex",
        }
    }

    /// What the provider itself calls a workspace, for user-facing output.
    pub fn workspace_noun(self) -> &'static str {
        match self {
            Self::Cmux => "workspace",
            Self::Rex => "session",
        }
    }

    /// True when csw is running inside a terminal of this provider.
    pub fn is_active(self) -> bool {
        match self {
            Self::Cmux => cmux::detect(),
            Self::Rex => rex::detect(),
        }
    }

    fn connect(self) -> Result<Box<dyn MuxClient>, MuxError> {
        Ok(match self {
            Self::Cmux => Box::new(cmux::connect()?),
            Self::Rex => Box::new(rex::connect()?),
        })
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Pick the provider to use: the first of `choice`'s candidates csw is
/// currently running inside, or `None` when there's nothing to talk to.
pub fn detect(choice: ProviderChoice) -> Option<Provider> {
    choice.candidates().iter().copied().find(|p| p.is_active())
}

/// The provider to use for this config, or `None` when the integration is
/// disabled or csw isn't running inside an allowed multiplexer.
pub fn provider_for(cfg: &crate::Config) -> Option<Provider> {
    if !cfg.mux_enabled() {
        return None;
    }
    detect(cfg.mux_provider())
}

/// User-facing outcome of a setup, close or rename attempt. Always
/// returned, never raised — the CLI inspects this to decide what to print,
/// and csw's exit code never depends on it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum MuxOutcome {
    /// The integration was not invoked (not inside a supported multiplexer,
    /// globally disabled, or suppressed by `--no-mux`). Print nothing.
    #[default]
    NotApplicable,
    /// Nothing to lay out (no attached editors and no agent). No workspace
    /// touched.
    NoContributors,
    /// A fresh workspace was built and is now focused.
    Created { provider: Provider, name: String },
    /// The current workspace was simple (single pane, single tab) and got
    /// reshaped in place rather than spawning a new sidebar entry.
    Adopted { provider: Provider, name: String },
    /// An existing workspace for the task was reused.
    Reused { provider: Provider, name: String },
    /// The workspace was found and closed.
    Closed { provider: Provider, name: String },
    /// `close` was called but no matching workspace existed.
    NotClosed,
    /// The workspace's sidebar label was updated to `name`.
    Renamed { provider: Provider, name: String },
    /// `rename` was called but no matching workspace existed.
    NotRenamed,
    /// Something went wrong on the multiplexer side, already prefixed with
    /// the provider name. Print as a single warning line.
    Warned(String),
}

fn warned(provider: Provider, e: impl fmt::Display) -> MuxOutcome {
    MuxOutcome::Warned(format!("{provider}: {e}"))
}

/// Build (or reuse) the task's workspace. Errors become
/// [`MuxOutcome::Warned`] for the caller to display.
///
/// `title` is the human-readable label to use in the sidebar; pass `None`
/// to fall back to just the task id.
pub fn setup(
    provider: Provider,
    task_id: &str,
    title: Option<&str>,
    contributors: &[Contributor],
    spec: &WorkspaceSpec,
    options: BuildOptions,
) -> MuxOutcome {
    if !build::has_content(contributors, spec) {
        return MuxOutcome::NoContributors;
    }
    match provider.connect() {
        Ok(mut client) => setup_with(
            provider,
            client.as_mut(),
            task_id,
            title,
            contributors,
            spec,
            options,
        ),
        Err(e) => warned(provider, e),
    }
}

/// Inner setup that operates on an already-connected client. Exposed for
/// tests that drive a recording client end-to-end.
pub fn setup_with(
    provider: Provider,
    client: &mut dyn MuxClient,
    task_id: &str,
    title: Option<&str>,
    contributors: &[Contributor],
    spec: &WorkspaceSpec,
    options: BuildOptions,
) -> MuxOutcome {
    match build::build_workspace(client, task_id, title, contributors, spec, options) {
        Ok(BuildOutcome::Created { name, .. }) => MuxOutcome::Created { provider, name },
        Ok(BuildOutcome::Adopted { name, .. }) => MuxOutcome::Adopted { provider, name },
        Ok(BuildOutcome::Reused { name, .. }) => MuxOutcome::Reused { provider, name },
        Ok(BuildOutcome::NoContributors) => MuxOutcome::NoContributors,
        Err(e) => warned(provider, e),
    }
}

/// Close the workspace for this task, if it exists.
pub fn close(provider: Provider, task_id: &str) -> MuxOutcome {
    match provider.connect() {
        Ok(mut client) => close_with(provider, client.as_mut(), task_id),
        Err(e) => warned(provider, e),
    }
}

pub fn close_with(provider: Provider, client: &mut dyn MuxClient, task_id: &str) -> MuxOutcome {
    match build::close_workspace_for(client, task_id) {
        Ok(true) => MuxOutcome::Closed {
            provider,
            // We no longer have the workspace title at this point (it might
            // have been a custom one), so just use the task id for display.
            name: task_id.to_string(),
        },
        Ok(false) => MuxOutcome::NotClosed,
        Err(e) => warned(provider, e),
    }
}

/// Rename the workspace for this task, computing the new label from
/// [`build::workspace_name_for`]. Returns [`MuxOutcome::NotRenamed`] when no
/// workspace matches the task id.
pub fn rename(provider: Provider, task_id: &str, title: Option<&str>) -> MuxOutcome {
    match provider.connect() {
        Ok(mut client) => rename_with(provider, client.as_mut(), task_id, title),
        Err(e) => warned(provider, e),
    }
}

pub fn rename_with(
    provider: Provider,
    client: &mut dyn MuxClient,
    task_id: &str,
    title: Option<&str>,
) -> MuxOutcome {
    match build::rename_workspace_for(client, task_id, title) {
        Ok(Some(name)) => MuxOutcome::Renamed { provider, name },
        Ok(None) => MuxOutcome::NotRenamed,
        Err(e) => warned(provider, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mux::client::Workspace;
    use crate::mux::client::testing::RecordingClient;
    use std::path::PathBuf;

    const P: Provider = Provider::Rex;

    fn one_contributor() -> Contributor {
        Contributor {
            repo: "fe".into(),
            worktree_path: PathBuf::from("/csw/tasks/alice-PROJ-1/fe"),
            editor_cmd: Some("pnpm dev".into()),
            additional_panels: 0,
        }
    }

    fn spec() -> WorkspaceSpec {
        WorkspaceSpec {
            task_root: PathBuf::from("/csw/tasks/alice-PROJ-1"),
            agent_cmd: Some("claude".into()),
            editors_attached: true,
        }
    }

    #[test]
    fn setup_with_no_content_returns_no_contributors() {
        // We never connect, so the provider's state is irrelevant.
        let empty_spec = WorkspaceSpec {
            task_root: PathBuf::from("/csw/tasks/alice-PROJ-1"),
            agent_cmd: None,
            editors_attached: true,
        };
        let outcome = setup(P, "PROJ-1", None, &[], &empty_spec, BuildOptions::default());
        assert_eq!(outcome, MuxOutcome::NoContributors);
    }

    #[test]
    fn setup_with_recording_client_returns_created() {
        let mut client = RecordingClient::new();
        let outcome = setup_with(
            P,
            &mut client,
            "PROJ-1",
            None,
            &[one_contributor()],
            &spec(),
            BuildOptions::default(),
        );
        assert_eq!(
            outcome,
            MuxOutcome::Created {
                provider: P,
                name: "PROJ-1".into()
            }
        );
    }

    #[test]
    fn setup_with_recording_client_reuses_existing() {
        let mut client = RecordingClient::new().with_existing(vec![Workspace {
            id: "ws-existing".into(),
            name: "PROJ-1".into(),
        }]);
        let outcome = setup_with(
            P,
            &mut client,
            "PROJ-1",
            None,
            &[one_contributor()],
            &spec(),
            BuildOptions::default(),
        );
        assert!(matches!(outcome, MuxOutcome::Reused { ref name, .. } if name == "PROJ-1"));
    }

    #[test]
    fn close_with_no_matching_workspace_returns_not_closed() {
        let mut client = RecordingClient::new();
        let outcome = close_with(P, &mut client, "PROJ-1");
        assert_eq!(outcome, MuxOutcome::NotClosed);
    }

    #[test]
    fn rename_with_no_matching_workspace_returns_not_renamed() {
        let mut client = RecordingClient::new();
        let outcome = rename_with(P, &mut client, "PROJ-1", Some("Fix bug"));
        assert_eq!(outcome, MuxOutcome::NotRenamed);
    }

    #[test]
    fn rename_with_matching_workspace_returns_renamed() {
        let mut client = RecordingClient::new().with_existing(vec![Workspace {
            id: "ws".into(),
            name: "PROJ-1".into(),
        }]);
        let outcome = rename_with(P, &mut client, "PROJ-1", Some("Fix bug"));
        assert!(
            matches!(outcome, MuxOutcome::Renamed { ref name, .. } if name == "PROJ-1 · Fix bug")
        );
    }

    #[test]
    fn close_with_matching_workspace_returns_closed() {
        let mut client = RecordingClient::new().with_existing(vec![Workspace {
            id: "ws".into(),
            name: "PROJ-1".into(),
        }]);
        let outcome = close_with(P, &mut client, "PROJ-1");
        assert!(matches!(outcome, MuxOutcome::Closed { ref name, .. } if name == "PROJ-1"));
    }

    #[test]
    fn provider_nouns_match_each_multiplexer() {
        assert_eq!(Provider::Cmux.workspace_noun(), "workspace");
        assert_eq!(Provider::Rex.workspace_noun(), "session");
    }
}
