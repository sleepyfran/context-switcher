//! CMux backend: speaks CMux's JSON request/response protocol over its Unix
//! domain socket.
//!
//! The protocol shape is:
//! ```json
//! { "id": "req-1", "method": "workspace.list", "params": {} }
//! ```
//! with a matching `{ "ok": bool, "result": ..., "error": ... }` reply.

use super::client::{
    MuxClient, MuxError, NewWorkspace, SplitDirection, SurfaceInfo, SurfaceRef, Workspace,
    pick_initial_surface,
};
use serde::Deserialize;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Environment variable CMux auto-exports inside every surface. Its presence
/// is how we know csw is being invoked from inside CMux, and its value is
/// the current workspace's id.
pub const WORKSPACE_ENV_VAR: &str = "CMUX_WORKSPACE_ID";

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
    pub const SURFACE_NEW: &str = "surface.create";
    pub const SURFACE_FOCUS: &str = "surface.focus";
    pub const SURFACE_SEND: &str = "surface.send_text";
    pub const SURFACE_LIST: &str = "surface.list";
}

/// True when csw is running inside a CMux surface.
pub fn detect() -> bool {
    std::env::var_os(WORKSPACE_ENV_VAR).is_some()
}

/// CMux's socket API expects directional splits by these string tokens.
fn direction_token(direction: SplitDirection) -> &'static str {
    match direction {
        SplitDirection::Left => "left",
        SplitDirection::Right => "right",
        SplitDirection::Up => "up",
        SplitDirection::Down => "down",
    }
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
pub fn connect() -> Result<CmuxClient, MuxError> {
    let path = resolve_socket_path().ok_or_else(|| {
        let env_hint = std::env::var(SOCKET_PATH_ENV).unwrap_or_else(|_| "<unset>".into());
        MuxError::Unavailable(format!(
            "no cmux socket found (tried ${SOCKET_PATH_ENV}={env_hint}, {FALLBACK_SOCKET}, {FALLBACK_DEBUG_SOCKET})"
        ))
    })?;
    CmuxClient::connect(&path)
}

pub struct CmuxClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    next_id: u64,
}

impl CmuxClient {
    pub fn connect(path: &Path) -> Result<Self, MuxError> {
        let stream = UnixStream::connect(path)
            .map_err(|e| MuxError::Unavailable(format!("socket {}: {e}", path.display())))?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        let writer = stream.try_clone()?;
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
            next_id: 1,
        })
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, MuxError> {
        let id = self.next_id;
        self.next_id += 1;
        let req = serde_json::json!({
            "id": format!("csw-{id}"),
            "method": method,
            "params": params,
        });
        let line = serde_json::to_string(&req)
            .map_err(|e| MuxError::Protocol(format!("serialise request: {e}")))?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;

        let mut response = String::new();
        let read = self.reader.read_line(&mut response)?;
        if read == 0 {
            return Err(MuxError::Protocol("socket closed".into()));
        }
        let parsed: Response = serde_json::from_str(response.trim())
            .map_err(|e| MuxError::Protocol(format!("parse response: {e}")))?;
        if parsed.ok == Some(false) {
            let message = parsed
                .error
                .as_ref()
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            return Err(MuxError::Method {
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

impl MuxClient for CmuxClient {
    /// `CMUX_WORKSPACE_ID` as a non-empty string. Empty values (some CMux
    /// builds export the var blank) are treated as absent.
    fn current_workspace_id(&self) -> Option<String> {
        std::env::var(WORKSPACE_ENV_VAR)
            .ok()
            .filter(|s| !s.is_empty())
    }

    fn list_workspaces(&mut self) -> Result<Vec<Workspace>, MuxError> {
        let result = self.request(methods::WORKSPACE_LIST, serde_json::json!({}))?;
        let arr = result
            .get("workspaces")
            .and_then(Value::as_array)
            .ok_or(MuxError::MissingField("workspaces"))?;
        let mut out = Vec::with_capacity(arr.len());
        for entry in arr {
            let id = entry
                .get("id")
                .and_then(Value::as_str)
                .ok_or(MuxError::MissingField("id"))?
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

    fn new_workspace(&mut self, title: &str) -> Result<NewWorkspace, MuxError> {
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

    fn set_workspace_color(&mut self, workspace_id: &str, color: &str) -> Result<(), MuxError> {
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

    fn select_workspace(&mut self, workspace_id: &str) -> Result<(), MuxError> {
        self.request(
            methods::WORKSPACE_SELECT,
            serde_json::json!({ "workspace_id": workspace_id }),
        )?;
        Ok(())
    }

    fn close_workspace(&mut self, workspace_id: &str) -> Result<(), MuxError> {
        self.request(
            methods::WORKSPACE_CLOSE,
            serde_json::json!({ "workspace_id": workspace_id }),
        )?;
        Ok(())
    }

    fn rename_workspace(&mut self, workspace_id: &str, title: &str) -> Result<(), MuxError> {
        self.request(
            methods::WORKSPACE_RENAME,
            serde_json::json!({ "workspace_id": workspace_id, "title": title }),
        )?;
        Ok(())
    }

    /// CMux always opens splits at 50/50, so `percent` is ignored.
    fn new_split(
        &mut self,
        anchor_surface: &str,
        direction: SplitDirection,
        _percent: u8,
    ) -> Result<SurfaceRef, MuxError> {
        let result = self.request(
            methods::PANE_SPLIT,
            serde_json::json!({
                "surface_id": anchor_surface,
                "direction": direction_token(direction),
            }),
        )?;
        extract_surface_ref(&result)
    }

    fn new_surface(&mut self, pane_id: &str) -> Result<SurfaceRef, MuxError> {
        let result = self.request(
            methods::SURFACE_NEW,
            serde_json::json!({ "pane_id": pane_id }),
        )?;
        extract_surface_ref(&result)
    }

    fn list_surfaces(&mut self, workspace_id: &str) -> Result<Vec<SurfaceInfo>, MuxError> {
        let result = self.request(
            methods::SURFACE_LIST,
            serde_json::json!({ "workspace_id": workspace_id }),
        )?;
        parse_surface_list(&result)
    }

    fn focus_surface(&mut self, surface_id: &str) -> Result<(), MuxError> {
        self.request(
            methods::SURFACE_FOCUS,
            serde_json::json!({ "surface_id": surface_id }),
        )?;
        Ok(())
    }

    fn send(&mut self, surface_id: &str, text: &str) -> Result<(), MuxError> {
        self.request(
            methods::SURFACE_SEND,
            serde_json::json!({ "surface_id": surface_id, "text": text }),
        )?;
        Ok(())
    }
}

fn read_string_field(result: &Value, field: &'static str) -> Result<String, MuxError> {
    result
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(MuxError::MissingField(field))
}

fn extract_surface_ref(result: &Value) -> Result<SurfaceRef, MuxError> {
    Ok(SurfaceRef {
        surface_id: read_string_field(result, "surface_id")?,
        pane_id: read_string_field(result, "pane_id")?,
    })
}

/// Parse a `surface.list` response into [`SurfaceInfo`] entries. CMux's
/// JSON shape is `{ "surfaces": [{ "id": ..., "pane_id": ..., "focused":
/// bool }, ...] }`; `focused` defaults to `false` when absent.
fn parse_surface_list(result: &Value) -> Result<Vec<SurfaceInfo>, MuxError> {
    let arr = result
        .get("surfaces")
        .and_then(Value::as_array)
        .ok_or(MuxError::MissingField("surfaces"))?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr {
        let surface_id = entry
            .get("id")
            .and_then(Value::as_str)
            .ok_or(MuxError::MissingField("id"))?
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use tempfile::TempDir;

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
        // Some CMux builds export the socket var as an empty string; we must
        // not treat that as a literal "" path. Discovery should still happen
        // via the canonical fallbacks (or return None when none exist).
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
        if let Some(p) = resolved {
            assert!(p != Path::new(""), "got empty path: {}", p.display());
        }
    }

    #[test]
    fn parse_surface_list_reads_focus_and_pane() {
        let json = serde_json::json!({
            "surfaces": [
                { "id": "s1", "pane_id": "p1" },
                { "id": "s2", "pane_id": "p1", "focused": true },
            ]
        });
        let parsed = parse_surface_list(&json).unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(!parsed[0].focused);
        assert!(parsed[1].focused);
        assert_eq!(parsed[1].surface_ref.pane_id, "p1");
    }
}
