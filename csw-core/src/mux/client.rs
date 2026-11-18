//! Provider-neutral multiplexer client.
//!
//! Every supported terminal multiplexer (CMux, Rex) implements
//! [`MuxClient`], a small set of primitives the build algorithm in
//! [`super::build`] composes into the task layout. The vocabulary follows
//! CMux's, since that's where the integration started:
//!
//! | csw term    | CMux                  | Rex                     |
//! |-------------|-----------------------|-------------------------|
//! | workspace   | workspace             | session                 |
//! | pane        | pane                  | block                   |
//! | surface     | surface (tab in pane) | block (or a new window) |
//!
//! The integration is best-effort — every error returned from a client ends
//! up as a warn-and-continue at the call site. No multiplexer failure ever
//! bumps csw's exit code.

/// Errors any provider can produce. The provider name is prefixed onto the
/// message by the caller (see [`super::MuxOutcome::Warned`]), so these
/// messages stay provider-agnostic.
#[derive(thiserror::Error, Debug)]
pub enum MuxError {
    /// The multiplexer couldn't be reached at all: socket missing, binary
    /// not on `$PATH`, server down, ...
    #[error("not reachable: {0}")]
    Unavailable(String),

    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    #[error("protocol: {0}")]
    Protocol(String),

    #[error("method `{method}` returned error: {message}")]
    Method { method: String, message: String },

    #[error("response missing expected field `{0}`")]
    MissingField(&'static str),
}

/// Direction to split a pane. Internal to the layout algorithm — not
/// user-configurable, so this carries no serde derives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    /// Human-readable label shown in the multiplexer's sidebar.
    pub name: String,
}

/// A surface plus its containing pane. Tabs attach to a pane (via
/// [`MuxClient::new_surface`]), so anywhere we hold a surface we also need to
/// know the pane it lives in. Providers without per-pane tabs (Rex) use the
/// same id for both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceRef {
    pub surface_id: String,
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWorkspace {
    pub workspace_id: String,
    /// Surface created automatically inside the new workspace. The build
    /// algorithm uses this as the seed for the layout.
    pub initial_surface: SurfaceRef,
}

/// One entry from a surface listing: the surface+pane pair plus whether
/// it's currently focused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceInfo {
    pub surface_ref: SurfaceRef,
    pub focused: bool,
}

/// Multiplexer client abstraction.
///
/// Production implementations live in [`super::cmux`] and [`super::rex`];
/// tests use [`testing::RecordingClient`], which captures the call sequence.
pub trait MuxClient {
    /// The workspace csw is currently running inside, if the multiplexer
    /// exposes it (via an env var it exports into every terminal). Used to
    /// decide whether the current workspace can be adopted in place.
    fn current_workspace_id(&self) -> Option<String>;

    fn list_workspaces(&mut self) -> Result<Vec<Workspace>, MuxError>;
    /// Create a workspace with the given sidebar title.
    fn new_workspace(&mut self, title: &str) -> Result<NewWorkspace, MuxError>;
    /// Bring the workspace to the front. Providers may treat this as
    /// best-effort and swallow failures that only affect the UI.
    fn select_workspace(&mut self, workspace_id: &str) -> Result<(), MuxError>;
    fn close_workspace(&mut self, workspace_id: &str) -> Result<(), MuxError>;
    /// Rename the sidebar label of an existing workspace.
    fn rename_workspace(&mut self, workspace_id: &str, title: &str) -> Result<(), MuxError>;
    /// Set the sidebar accent color (`#RRGGBB`). A no-op for providers
    /// without workspace colors.
    fn set_workspace_color(&mut self, workspace_id: &str, color: &str) -> Result<(), MuxError>;

    /// Split off a new pane from an existing surface, in the given
    /// direction. `percent` is the share (1..=99) of the anchor's space the
    /// new pane should take; providers that can't size at split time ignore
    /// it.
    fn new_split(
        &mut self,
        anchor_surface: &str,
        direction: SplitDirection,
        percent: u8,
    ) -> Result<SurfaceRef, MuxError>;

    /// Add an extra tab associated with the given pane. CMux stacks it as a
    /// surface inside the pane; Rex opens it as a new window in the session.
    fn new_surface(&mut self, pane_id: &str) -> Result<SurfaceRef, MuxError>;

    /// List every surface in the given workspace. Used to inspect the
    /// layout of the current workspace before deciding whether to adopt it.
    fn list_surfaces(&mut self, workspace_id: &str) -> Result<Vec<SurfaceInfo>, MuxError>;

    fn focus_surface(&mut self, surface_id: &str) -> Result<(), MuxError>;
    /// Type `text` into the surface's terminal, verbatim (a trailing `\n`
    /// submits it).
    fn send(&mut self, surface_id: &str, text: &str) -> Result<(), MuxError>;
}

/// Pick the focused surface from a surface list. Falls back to the first
/// entry if none is marked focused.
pub(crate) fn pick_initial_surface(surfaces: Vec<SurfaceInfo>) -> Result<SurfaceRef, MuxError> {
    if surfaces.is_empty() {
        return Err(MuxError::MissingField("surfaces"));
    }
    let pick = surfaces
        .iter()
        .find(|s| s.focused)
        .map(|s| s.surface_ref.clone())
        .unwrap_or_else(|| surfaces[0].surface_ref.clone());
    Ok(pick)
}

/// Shell-quote a string for safe insertion into a `sh -c` argument. Wraps in
/// single quotes and escapes embedded single quotes as `'\''`.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
pub mod testing {
    //! Recording client used by build-algorithm tests. Captures every call
    //! so tests can assert the exact sequence of requests without needing a
    //! live multiplexer.

    use super::*;
    use std::collections::HashMap;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum Call {
        ListWorkspaces,
        NewWorkspace {
            title: String,
        },
        SelectWorkspace {
            id: String,
        },
        CloseWorkspace {
            id: String,
        },
        RenameWorkspace {
            id: String,
            title: String,
        },
        SetColor {
            id: String,
            color: String,
        },
        NewSplit {
            anchor: String,
            direction: SplitDirection,
            percent: u8,
        },
        NewSurface {
            pane: String,
        },
        ListSurfaces {
            workspace_id: String,
        },
        FocusSurface {
            id: String,
        },
        Send {
            id: String,
            text: String,
        },
    }

    pub struct RecordingClient {
        pub calls: Vec<Call>,
        pub existing_workspaces: Vec<Workspace>,
        pub next_id: u64,
        /// What [`MuxClient::current_workspace_id`] reports. `None` by
        /// default, i.e. "not running inside any workspace".
        pub current_workspace: Option<String>,
        /// Surfaces returned by `list_surfaces` per workspace id. Workspaces
        /// without an entry report an empty list.
        workspace_surfaces: HashMap<String, Vec<SurfaceInfo>>,
    }

    impl Default for RecordingClient {
        fn default() -> Self {
            Self::new()
        }
    }

    impl RecordingClient {
        pub fn new() -> Self {
            Self {
                calls: Vec::new(),
                existing_workspaces: Vec::new(),
                next_id: 1,
                current_workspace: None,
                workspace_surfaces: HashMap::new(),
            }
        }

        pub fn with_existing(mut self, workspaces: Vec<Workspace>) -> Self {
            self.existing_workspaces = workspaces;
            self
        }

        pub fn with_current_workspace(mut self, workspace_id: &str) -> Self {
            self.current_workspace = Some(workspace_id.to_string());
            self
        }

        /// Seed the surfaces a `list_surfaces(workspace_id)` call will
        /// report. Used by tests that exercise the in-place adoption path.
        pub fn with_workspace_surfaces(
            mut self,
            workspace_id: &str,
            surfaces: Vec<SurfaceInfo>,
        ) -> Self {
            self.workspace_surfaces
                .insert(workspace_id.to_string(), surfaces);
            self
        }

        pub fn calls(&self) -> Vec<Call> {
            self.calls.clone()
        }

        fn record(&mut self, c: Call) {
            self.calls.push(c);
        }

        fn next_pair(&mut self) -> SurfaceRef {
            let n = self.next_id;
            self.next_id += 1;
            SurfaceRef {
                surface_id: format!("surface-{n}"),
                pane_id: format!("pane-{n}"),
            }
        }
    }

    impl MuxClient for RecordingClient {
        fn current_workspace_id(&self) -> Option<String> {
            self.current_workspace.clone()
        }
        fn list_workspaces(&mut self) -> Result<Vec<Workspace>, MuxError> {
            self.record(Call::ListWorkspaces);
            Ok(self.existing_workspaces.clone())
        }
        fn new_workspace(&mut self, title: &str) -> Result<NewWorkspace, MuxError> {
            self.record(Call::NewWorkspace {
                title: title.to_string(),
            });
            let workspace_id = format!("ws-{title}");
            let initial_surface = self.next_pair();
            // A freshly created workspace contains exactly one focused
            // surface; keep `list_surfaces` consistent with that.
            self.workspace_surfaces.insert(
                workspace_id.clone(),
                vec![SurfaceInfo {
                    surface_ref: initial_surface.clone(),
                    focused: true,
                }],
            );
            Ok(NewWorkspace {
                workspace_id,
                initial_surface,
            })
        }
        fn select_workspace(&mut self, workspace_id: &str) -> Result<(), MuxError> {
            self.record(Call::SelectWorkspace {
                id: workspace_id.to_string(),
            });
            Ok(())
        }
        fn close_workspace(&mut self, workspace_id: &str) -> Result<(), MuxError> {
            self.record(Call::CloseWorkspace {
                id: workspace_id.to_string(),
            });
            Ok(())
        }
        fn rename_workspace(&mut self, workspace_id: &str, title: &str) -> Result<(), MuxError> {
            self.record(Call::RenameWorkspace {
                id: workspace_id.to_string(),
                title: title.to_string(),
            });
            // Reflect the rename so later `list_workspaces` calls see it.
            if let Some(ws) = self
                .existing_workspaces
                .iter_mut()
                .find(|w| w.id == workspace_id)
            {
                ws.name = title.to_string();
            }
            Ok(())
        }
        fn set_workspace_color(&mut self, workspace_id: &str, color: &str) -> Result<(), MuxError> {
            self.record(Call::SetColor {
                id: workspace_id.to_string(),
                color: color.to_string(),
            });
            Ok(())
        }
        fn new_split(
            &mut self,
            anchor_surface: &str,
            direction: SplitDirection,
            percent: u8,
        ) -> Result<SurfaceRef, MuxError> {
            self.record(Call::NewSplit {
                anchor: anchor_surface.to_string(),
                direction,
                percent,
            });
            Ok(self.next_pair())
        }
        fn new_surface(&mut self, pane_id: &str) -> Result<SurfaceRef, MuxError> {
            self.record(Call::NewSurface {
                pane: pane_id.to_string(),
            });
            Ok(self.next_pair())
        }
        fn list_surfaces(&mut self, workspace_id: &str) -> Result<Vec<SurfaceInfo>, MuxError> {
            self.record(Call::ListSurfaces {
                workspace_id: workspace_id.to_string(),
            });
            Ok(self
                .workspace_surfaces
                .get(workspace_id)
                .cloned()
                .unwrap_or_default())
        }
        fn focus_surface(&mut self, surface_id: &str) -> Result<(), MuxError> {
            self.record(Call::FocusSurface {
                id: surface_id.to_string(),
            });
            Ok(())
        }
        fn send(&mut self, surface_id: &str, text: &str) -> Result<(), MuxError> {
            self.record(Call::Send {
                id: surface_id.to_string(),
                text: text.to_string(),
            });
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_wraps_plain_string() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn shell_quote_handles_spaces() {
        assert_eq!(shell_quote("hello world"), "'hello world'");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("it's fine"), r"'it'\''s fine'");
    }

    #[test]
    fn shell_quote_preserves_dollar_signs_inside_single_quotes() {
        // Single-quoted strings suppress shell expansion. We don't need to
        // escape `$` — the quotes already protect us.
        assert_eq!(shell_quote("$HOME/x"), "'$HOME/x'");
    }

    fn info(id: &str, focused: bool) -> SurfaceInfo {
        SurfaceInfo {
            surface_ref: SurfaceRef {
                surface_id: id.into(),
                pane_id: id.into(),
            },
            focused,
        }
    }

    #[test]
    fn pick_initial_surface_prefers_focused() {
        let picked = pick_initial_surface(vec![info("a", false), info("b", true)]).unwrap();
        assert_eq!(picked.surface_id, "b");
    }

    #[test]
    fn pick_initial_surface_falls_back_to_first() {
        let picked = pick_initial_surface(vec![info("a", false), info("b", false)]).unwrap();
        assert_eq!(picked.surface_id, "a");
    }

    #[test]
    fn pick_initial_surface_rejects_empty() {
        assert!(pick_initial_surface(vec![]).is_err());
    }
}
