//! CMux socket client.
//!
//! Speaks the JSON request/response protocol over a Unix domain socket at
//! `/tmp/cmux.sock` (or `/tmp/cmux-debug.sock` for debug builds).
//!
//! The protocol shape is:
//! ```json
//! { "id": "req-1", "method": "workspace.list", "params": {} }
//! ```
//! with a matching `{ "id": "...", "result": ..., "error": ... }` reply.
//!
//! Method names are centralised in [`methods`]: the public CMux docs use
//! kebab-case ("new-split", "focus-surface") in one place and dot-namespaced
//! ("workspace.list") in another. We default to the dot-namespaced forms
//! since those are the ones shown in actual JSON examples; if a real CMux
//! build disagrees, only [`methods`] needs to change.
//!
//! The integration is best-effort — every error returned from this module
//! ends up as a warn-and-continue at the call site. No CMux failure ever
//! bumps csw's exit code.

use super::config::SplitDirection;
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Env var CMux exports inside every surface, pointing at its Unix socket.
/// This is the canonical discovery path — the conventional `/tmp` locations
/// are only a fallback.
const SOCKET_PATH_ENV: &str = "CMUX_SOCKET_PATH";
const FALLBACK_SOCKET: &str = "/tmp/cmux.sock";
const FALLBACK_DEBUG_SOCKET: &str = "/tmp/cmux-debug.sock";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// CMux socket method names, verified against `system.capabilities` on a
/// live CMux v2 build. Adjust here if a future build disagrees.
pub mod methods {
    pub const WORKSPACE_LIST: &str = "workspace.list";
    pub const WORKSPACE_NEW: &str = "workspace.create";
    pub const WORKSPACE_SELECT: &str = "workspace.select";
    pub const WORKSPACE_CLOSE: &str = "workspace.close";
    pub const WORKSPACE_RENAME: &str = "workspace.rename";
    pub const WORKSPACE_ACTION: &str = "workspace.action";
    pub const PANE_SPLIT: &str = "surface.split";
    pub const PANE_RESIZE: &str = "pane.resize";
    pub const SURFACE_NEW: &str = "surface.create";
    pub const SURFACE_FOCUS: &str = "surface.focus";
    pub const SURFACE_SEND: &str = "surface.send_text";
    pub const SURFACE_LIST: &str = "surface.list";
}

#[derive(thiserror::Error, Debug)]
pub enum CmuxError {
    #[error("cmux socket not reachable at {0}: {1}")]
    SocketUnreachable(PathBuf, std::io::Error),

    #[error("cmux i/o: {0}")]
    Io(#[from] std::io::Error),

    #[error("cmux protocol: {0}")]
    Protocol(String),

    #[error("cmux method `{method}` returned error: {message}")]
    Method { method: String, message: String },

    #[error("cmux response missing expected field `{0}`")]
    MissingField(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    pub id: String,
    /// CMux's `title` field — the human-readable label that shows in the
    /// sidebar. Stored as `name` here because that's what we treat it as.
    pub name: String,
}

/// A surface plus its containing pane. Tabs attach to a pane (via
/// `surface.create`), so anywhere we hold a surface we also need to know
/// the pane it lives in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceRef {
    pub surface_id: String,
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWorkspace {
    pub workspace_id: String,
    /// Surface created automatically inside the new workspace. The build
    /// algorithm uses this as the seed for the first repo's slot.
    pub initial_surface: SurfaceRef,
}

/// What CMux reports back from a `pane.resize` call.
///
/// CMux's resize API takes a pixel-space `amount` (treated as a signed delta
/// against the split's axis size) rather than an absolute target ratio, and
/// clamps the resulting divider to [0.1, 0.9] internally. The two divider
/// positions let callers convert "pixels nudged" back into "ratio achieved"
/// and iterate toward a target if needed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResizeOutcome {
    pub old_divider_position: f32,
    pub new_divider_position: f32,
}

/// CMux client abstraction.
///
/// The production implementation talks to the Unix socket; tests use a
/// recording stub that captures the call sequence. The signatures stay
/// boring on purpose — every method maps to one socket request (with the
/// exception of [`Self::new_workspace`], which does a `workspace.create`
/// followed by a `surface.list` to discover the auto-created surface).
pub trait CmuxClient: Send {
    fn list_workspaces(&mut self) -> Result<Vec<Workspace>, CmuxError>;
    /// Create a workspace with the given sidebar title.
    fn new_workspace(&mut self, title: &str) -> Result<NewWorkspace, CmuxError>;
    fn select_workspace(&mut self, workspace_id: &str) -> Result<(), CmuxError>;
    fn close_workspace(&mut self, workspace_id: &str) -> Result<(), CmuxError>;
    /// Rename the sidebar label of an existing workspace.
    fn rename_workspace(&mut self, workspace_id: &str, title: &str) -> Result<(), CmuxError>;
    /// Set the sidebar accent color for a workspace. CMux applies this via
    /// `workspace.action { action: "set_color", color: "#RRGGBB" }`.
    fn set_workspace_color(&mut self, workspace_id: &str, color: &str) -> Result<(), CmuxError>;

    /// Split off a new pane from an existing surface, in the given direction.
    fn new_split(
        &mut self,
        anchor_surface: &str,
        direction: SplitDirection,
    ) -> Result<SurfaceRef, CmuxError>;

    /// Nudge a split's divider by `amount` (pixel-space, signed by direction)
    /// against the pane identified by `pane_id`. CMux walks up the Bonsplit
    /// tree until it finds a split with an edge on the requested side, then
    /// moves the divider by `amount / axis_pixels` and clamps to [0.1, 0.9].
    ///
    /// `amount` must be `> 0`; the *direction* carries the sign. Use the
    /// returned [`ResizeOutcome`] to derive the actual axis-pixel size and
    /// converge toward a target ratio if needed.
    fn resize_pane(
        &mut self,
        pane_id: &str,
        direction: SplitDirection,
        amount: i32,
    ) -> Result<ResizeOutcome, CmuxError>;

    /// Add a new tab (surface) inside the given pane. CMux's `surface.create`
    /// takes a `pane_id`, hence the parameter type here.
    fn new_surface(&mut self, pane_id: &str) -> Result<SurfaceRef, CmuxError>;

    /// List every surface in the given workspace, with their containing
    /// pane and whether they're currently focused. Used both to discover
    /// the auto-created surface after [`Self::new_workspace`] and to
    /// inspect the layout of the current workspace before deciding
    /// whether to adopt it in place.
    fn list_surfaces(&mut self, workspace_id: &str) -> Result<Vec<SurfaceInfo>, CmuxError>;

    fn focus_surface(&mut self, surface_id: &str) -> Result<(), CmuxError>;
    fn send(&mut self, surface_id: &str, text: &str) -> Result<(), CmuxError>;
}

/// One entry from a `surface.list` response. Carries the same
/// surface+pane pair as [`SurfaceRef`], plus the focus bit (which CMux
/// includes in the JSON and we need for picking the "current" surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceInfo {
    pub surface_ref: SurfaceRef,
    pub focused: bool,
}

/// Resolve where the CMux socket lives, in this priority order:
///  1. `$CMUX_SOCKET_PATH` — CMux auto-exports this inside every surface,
///     so it's always correct when we're actually running inside CMux.
///  2. `/tmp/cmux.sock` — conventional release-build location.
///  3. `/tmp/cmux-debug.sock` — conventional debug-build location.
///
/// Returns `None` if none of those exist as a socket on disk.
pub fn resolve_socket_path() -> Option<PathBuf> {
    if let Some(raw) = std::env::var_os(SOCKET_PATH_ENV)
        && !raw.is_empty()
    {
        let p = PathBuf::from(raw);
        if p.exists() {
            return Some(p);
        }
    }
    for candidate in [FALLBACK_SOCKET, FALLBACK_DEBUG_SOCKET] {
        let p = Path::new(candidate);
        if p.exists() {
            return Some(p.to_path_buf());
        }
    }
    None
}

/// Connect to the CMux socket using [`resolve_socket_path`]'s precedence.
pub fn connect() -> Result<UnixSocketClient, CmuxError> {
    let path = resolve_socket_path().ok_or_else(|| {
        let env_hint = std::env::var(SOCKET_PATH_ENV).unwrap_or_else(|_| "<unset>".into());
        CmuxError::SocketUnreachable(
            PathBuf::from(FALLBACK_SOCKET),
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "no cmux socket found (tried ${SOCKET_PATH_ENV}={env_hint}, {FALLBACK_SOCKET}, {FALLBACK_DEBUG_SOCKET})"
                ),
            ),
        )
    })?;
    UnixSocketClient::connect(&path)
}

pub struct UnixSocketClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: AtomicU64,
}

impl UnixSocketClient {
    pub fn connect(path: &Path) -> Result<Self, CmuxError> {
        let stream = UnixStream::connect(path)
            .map_err(|e| CmuxError::SocketUnreachable(path.to_path_buf(), e))?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        let writer = stream.try_clone()?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
            next_id: AtomicU64::new(1),
        })
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, CmuxError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = serde_json::json!({
            "id": format!("csw-{id}"),
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req)
            .map_err(|e| CmuxError::Protocol(format!("serialise request: {e}")))?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;

        let mut response = String::new();
        let read = self.reader.read_line(&mut response)?;
        if read == 0 {
            return Err(CmuxError::Protocol("socket closed".into()));
        }
        let parsed: Response = serde_json::from_str(response.trim())
            .map_err(|e| CmuxError::Protocol(format!("parse response: {e}")))?;
        if parsed.ok == Some(false) {
            let message = parsed
                .error
                .as_ref()
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            return Err(CmuxError::Method {
                method: method.to_string(),
                message,
            });
        }
        Ok(parsed.result.unwrap_or(Value::Null))
    }
}

/// CMux response envelope: `{ok: bool, result?: ..., error?: {...}, id}`.
#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    ok: Option<bool>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<Value>,
}

impl CmuxClient for UnixSocketClient {
    fn list_workspaces(&mut self) -> Result<Vec<Workspace>, CmuxError> {
        let result = self.request(methods::WORKSPACE_LIST, serde_json::json!({}))?;
        let arr = result
            .get("workspaces")
            .and_then(Value::as_array)
            .ok_or(CmuxError::MissingField("workspaces"))?;
        let mut out = Vec::with_capacity(arr.len());
        for entry in arr {
            let id = entry
                .get("id")
                .and_then(Value::as_str)
                .ok_or(CmuxError::MissingField("id"))?
                .to_string();
            // CMux's human-readable workspace name lives in `title`.
            let name = entry
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            out.push(Workspace { id, name });
        }
        Ok(out)
    }

    fn new_workspace(&mut self, title: &str) -> Result<NewWorkspace, CmuxError> {
        // `workspace.create` takes `title` as the sidebar label. (An earlier
        // probe with `name` silently fell back to an autogenerated title.)
        let result = self.request(
            methods::WORKSPACE_NEW,
            serde_json::json!({ "title": title }),
        )?;
        let workspace_id = read_string_field(&result, "workspace_id")?;
        // `workspace.create` doesn't return the initial surface — discover it
        // via `surface.list` and pick the focused one.
        let initial_surface = pick_initial_surface(self.list_surfaces(&workspace_id)?)?;
        Ok(NewWorkspace {
            workspace_id,
            initial_surface,
        })
    }

    fn set_workspace_color(&mut self, workspace_id: &str, color: &str) -> Result<(), CmuxError> {
        self.request(
            methods::WORKSPACE_ACTION,
            serde_json::json!({
                "workspace_id": workspace_id,
                "action": "set_color",
                "color": color,
            }),
        )?;
        Ok(())
    }

    fn select_workspace(&mut self, workspace_id: &str) -> Result<(), CmuxError> {
        self.request(
            methods::WORKSPACE_SELECT,
            serde_json::json!({ "workspace_id": workspace_id }),
        )?;
        Ok(())
    }

    fn close_workspace(&mut self, workspace_id: &str) -> Result<(), CmuxError> {
        self.request(
            methods::WORKSPACE_CLOSE,
            serde_json::json!({ "workspace_id": workspace_id }),
        )?;
        Ok(())
    }

    fn rename_workspace(&mut self, workspace_id: &str, title: &str) -> Result<(), CmuxError> {
        self.request(
            methods::WORKSPACE_RENAME,
            serde_json::json!({ "workspace_id": workspace_id, "title": title }),
        )?;
        Ok(())
    }

    fn new_split(
        &mut self,
        anchor_surface: &str,
        direction: SplitDirection,
    ) -> Result<SurfaceRef, CmuxError> {
        let result = self.request(
            methods::PANE_SPLIT,
            serde_json::json!({
                "surface_id": anchor_surface,
                "direction": direction.as_str(),
            }),
        )?;
        extract_surface_ref(&result)
    }

    fn resize_pane(
        &mut self,
        pane_id: &str,
        direction: SplitDirection,
        amount: i32,
    ) -> Result<ResizeOutcome, CmuxError> {
        let result = self.request(
            methods::PANE_RESIZE,
            serde_json::json!({
                "pane_id": pane_id,
                "direction": direction.as_str(),
                "amount": amount,
            }),
        )?;
        extract_resize_outcome(&result)
    }

    fn new_surface(&mut self, pane_id: &str) -> Result<SurfaceRef, CmuxError> {
        let result = self.request(
            methods::SURFACE_NEW,
            serde_json::json!({ "pane_id": pane_id }),
        )?;
        extract_surface_ref(&result)
    }

    fn list_surfaces(&mut self, workspace_id: &str) -> Result<Vec<SurfaceInfo>, CmuxError> {
        let result = self.request(
            methods::SURFACE_LIST,
            serde_json::json!({ "workspace_id": workspace_id }),
        )?;
        parse_surface_list(&result)
    }

    fn focus_surface(&mut self, surface_id: &str) -> Result<(), CmuxError> {
        self.request(
            methods::SURFACE_FOCUS,
            serde_json::json!({ "surface_id": surface_id }),
        )?;
        Ok(())
    }

    fn send(&mut self, surface_id: &str, text: &str) -> Result<(), CmuxError> {
        self.request(
            methods::SURFACE_SEND,
            serde_json::json!({ "surface_id": surface_id, "text": text }),
        )?;
        Ok(())
    }
}

fn read_string_field(result: &Value, field: &'static str) -> Result<String, CmuxError> {
    result
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(CmuxError::MissingField(field))
}

fn extract_surface_ref(result: &Value) -> Result<SurfaceRef, CmuxError> {
    Ok(SurfaceRef {
        surface_id: read_string_field(result, "surface_id")?,
        pane_id: read_string_field(result, "pane_id")?,
    })
}

fn read_f32_field(result: &Value, field: &'static str) -> Result<f32, CmuxError> {
    result
        .get(field)
        .and_then(Value::as_f64)
        .map(|v| v as f32)
        .ok_or(CmuxError::MissingField(field))
}

fn extract_resize_outcome(result: &Value) -> Result<ResizeOutcome, CmuxError> {
    Ok(ResizeOutcome {
        old_divider_position: read_f32_field(result, "old_divider_position")?,
        new_divider_position: read_f32_field(result, "new_divider_position")?,
    })
}

/// Parse a `surface.list` response into [`SurfaceInfo`] entries. CMux's
/// JSON shape is `{ "surfaces": [{ "id": ..., "pane_id": ..., "focused":
/// bool }, ...] }`; `focused` defaults to `false` when absent.
fn parse_surface_list(result: &Value) -> Result<Vec<SurfaceInfo>, CmuxError> {
    let arr = result
        .get("surfaces")
        .and_then(Value::as_array)
        .ok_or(CmuxError::MissingField("surfaces"))?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let surface_id = entry
            .get("id")
            .and_then(Value::as_str)
            .ok_or(CmuxError::MissingField("id"))?
            .to_string();
        let pane_id = read_string_field(entry, "pane_id")?;
        let focused = entry
            .get("focused")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        out.push(SurfaceInfo {
            surface_ref: SurfaceRef {
                surface_id,
                pane_id,
            },
            focused,
        });
    }
    Ok(out)
}

/// Pick the focused surface from a parsed surface list. Falls back to the
/// first entry if none is marked focused, which matches CMux's behavior
/// of returning a single-surface workspace without setting `focused`.
fn pick_initial_surface(surfaces: Vec<SurfaceInfo>) -> Result<SurfaceRef, CmuxError> {
    if surfaces.is_empty() {
        return Err(CmuxError::MissingField("surfaces"));
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
    //! so tests can assert the exact sequence of socket requests without
    //! needing a live CMux.

    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

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
        },
        ResizePane {
            pane: String,
            direction: SplitDirection,
            amount: i32,
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
        pub calls: Mutex<Vec<Call>>,
        pub existing_workspaces: Vec<Workspace>,
        pub next_id: std::sync::atomic::AtomicU64,
        /// Axis size, in pixels, used by `resize_pane` to simulate CMux's
        /// `delta = amount / axisPixels` math. Tests can override this to
        /// exercise the iterative axis-pixel refinement in the build code.
        pub simulated_axis_pixels: f32,
        /// Last known divider position for each pane, keyed by pane id. CMux
        /// always opens splits at 0.5 and clamps to [0.1, 0.9].
        pane_divider: Mutex<HashMap<String, f32>>,
        /// Surfaces returned by `list_surfaces` per workspace id. Workspaces
        /// without an explicit override get a synthesised single-surface
        /// reply (mirroring CMux's freshly-created workspace).
        workspace_surfaces: Mutex<HashMap<String, Vec<SurfaceInfo>>>,
    }

    impl Default for RecordingClient {
        fn default() -> Self {
            Self::new()
        }
    }

    impl RecordingClient {
        pub fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                existing_workspaces: Vec::new(),
                next_id: std::sync::atomic::AtomicU64::new(1),
                simulated_axis_pixels: 1600.0,
                pane_divider: Mutex::new(HashMap::new()),
                workspace_surfaces: Mutex::new(HashMap::new()),
            }
        }

        pub fn with_existing(mut self, workspaces: Vec<Workspace>) -> Self {
            self.existing_workspaces = workspaces;
            self
        }

        pub fn with_simulated_axis_pixels(mut self, pixels: f32) -> Self {
            self.simulated_axis_pixels = pixels;
            self
        }

        /// Seed the surfaces a `list_surfaces(workspace_id)` call will
        /// report. Used by tests that exercise the in-place adoption path,
        /// which inspects the current workspace's layout before deciding
        /// whether to reshape it.
        pub fn with_workspace_surfaces(
            self,
            workspace_id: &str,
            surfaces: Vec<SurfaceInfo>,
        ) -> Self {
            self.workspace_surfaces
                .lock()
                .unwrap()
                .insert(workspace_id.to_string(), surfaces);
            self
        }

        pub fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        /// Current simulated divider position for the given pane, or `None`
        /// if no `resize_pane` call has touched it. Used by tests to verify
        /// that iterative sizing converged on its target.
        pub fn divider_for(&self, pane_id: &str) -> Option<f32> {
            self.pane_divider.lock().unwrap().get(pane_id).copied()
        }

        fn record(&self, c: Call) {
            self.calls.lock().unwrap().push(c);
        }

        fn next_pair(&self) -> SurfaceRef {
            let n = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            SurfaceRef {
                surface_id: format!("surface-{n}"),
                pane_id: format!("pane-{n}"),
            }
        }
    }

    impl CmuxClient for RecordingClient {
        fn list_workspaces(&mut self) -> Result<Vec<Workspace>, CmuxError> {
            self.record(Call::ListWorkspaces);
            Ok(self.existing_workspaces.clone())
        }
        fn new_workspace(&mut self, title: &str) -> Result<NewWorkspace, CmuxError> {
            self.record(Call::NewWorkspace {
                title: title.to_string(),
            });
            let workspace_id = format!("ws-{title}");
            let initial_surface = self.next_pair();
            // Mirror CMux: the freshly created workspace contains exactly
            // one focused surface. Keep that consistent so subsequent
            // `list_surfaces(workspace_id)` calls don't lie.
            self.workspace_surfaces.lock().unwrap().insert(
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
        fn select_workspace(&mut self, workspace_id: &str) -> Result<(), CmuxError> {
            self.record(Call::SelectWorkspace {
                id: workspace_id.to_string(),
            });
            Ok(())
        }
        fn close_workspace(&mut self, workspace_id: &str) -> Result<(), CmuxError> {
            self.record(Call::CloseWorkspace {
                id: workspace_id.to_string(),
            });
            Ok(())
        }
        fn rename_workspace(&mut self, workspace_id: &str, title: &str) -> Result<(), CmuxError> {
            self.record(Call::RenameWorkspace {
                id: workspace_id.to_string(),
                title: title.to_string(),
            });
            // Reflect the rename in the recorded list so subsequent
            // `list_workspaces` calls in the same test see the new title.
            if let Some(ws) = self
                .existing_workspaces
                .iter_mut()
                .find(|w| w.id == workspace_id)
            {
                ws.name = title.to_string();
            }
            Ok(())
        }
        fn set_workspace_color(
            &mut self,
            workspace_id: &str,
            color: &str,
        ) -> Result<(), CmuxError> {
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
        ) -> Result<SurfaceRef, CmuxError> {
            self.record(Call::NewSplit {
                anchor: anchor_surface.to_string(),
                direction,
            });
            Ok(self.next_pair())
        }
        fn resize_pane(
            &mut self,
            pane_id: &str,
            direction: SplitDirection,
            amount: i32,
        ) -> Result<ResizeOutcome, CmuxError> {
            self.record(Call::ResizePane {
                pane: pane_id.to_string(),
                direction,
                amount,
            });
            // Sign convention matches CMux's `dividerDeltaSign`: positive
            // toward the second child (right/down).
            let sign: f32 = match direction {
                SplitDirection::Right | SplitDirection::Down => 1.0,
                SplitDirection::Left | SplitDirection::Up => -1.0,
            };
            let mut map = self.pane_divider.lock().unwrap();
            let current = *map.entry(pane_id.to_string()).or_insert(0.5);
            let requested = current + sign * (amount as f32 / self.simulated_axis_pixels);
            let new = requested.clamp(0.1, 0.9);
            map.insert(pane_id.to_string(), new);
            Ok(ResizeOutcome {
                old_divider_position: current,
                new_divider_position: new,
            })
        }
        fn new_surface(&mut self, pane_id: &str) -> Result<SurfaceRef, CmuxError> {
            self.record(Call::NewSurface {
                pane: pane_id.to_string(),
            });
            Ok(self.next_pair())
        }
        fn list_surfaces(&mut self, workspace_id: &str) -> Result<Vec<SurfaceInfo>, CmuxError> {
            self.record(Call::ListSurfaces {
                workspace_id: workspace_id.to_string(),
            });
            // Returns the explicit override if one was registered (via
            // `with_workspace_surfaces` or recorded inside `new_workspace`);
            // otherwise an empty list, which would correctly fail any
            // "simple" check.
            Ok(self
                .workspace_surfaces
                .lock()
                .unwrap()
                .get(workspace_id)
                .cloned()
                .unwrap_or_default())
        }
        fn focus_surface(&mut self, surface_id: &str) -> Result<(), CmuxError> {
            self.record(Call::FocusSurface {
                id: surface_id.to_string(),
            });
            Ok(())
        }
        fn send(&mut self, surface_id: &str, text: &str) -> Result<(), CmuxError> {
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
    use std::os::unix::net::UnixListener;
    use tempfile::TempDir;

    #[test]
    fn shell_quote_wraps_plain_string() {
        assert_eq!(shell_quote("hello"), "'hello'");
    }

    #[test]
    fn resolve_socket_path_prefers_env_var_when_set() {
        // Real socket on disk so the `.exists()` gate passes.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("cmux.sock");
        let _listener = UnixListener::bind(&path).unwrap();

        // SAFETY: tests run synchronously here and we restore the env var
        // immediately after reading.
        let previous = std::env::var_os(SOCKET_PATH_ENV);
        unsafe {
            std::env::set_var(SOCKET_PATH_ENV, &path);
        }
        let resolved = resolve_socket_path();
        unsafe {
            match previous {
                Some(v) => std::env::set_var(SOCKET_PATH_ENV, v),
                None => std::env::remove_var(SOCKET_PATH_ENV),
            }
        }
        assert_eq!(resolved.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn resolve_socket_path_handles_empty_env_var() {
        // CMux sets `CMUX_SOCKET` (note: no `_PATH`) to an empty string in
        // some builds; we must not treat that as a literal "" path. Real
        // discovery should still happen via the canonical fallbacks (or
        // return None when none exist).
        let previous = std::env::var_os(SOCKET_PATH_ENV);
        unsafe {
            std::env::set_var(SOCKET_PATH_ENV, "");
        }
        let resolved = resolve_socket_path();
        unsafe {
            match previous {
                Some(v) => std::env::set_var(SOCKET_PATH_ENV, v),
                None => std::env::remove_var(SOCKET_PATH_ENV),
            }
        }
        // Either None (no /tmp socket on CI) or Some(/tmp/cmux.sock) on a
        // dev machine — never an OsString of "" turned into a Path.
        if let Some(p) = resolved {
            assert!(p != Path::new(""), "got empty path: {}", p.display());
        }
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
}
