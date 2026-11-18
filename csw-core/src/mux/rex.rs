//! Rex backend: drives [Rex](https://www.superlogical.com/rex) through its
//! `rex` CLI, the supported automation surface (the server's socket protocol
//! is internal).
//!
//! Mapping onto the [`MuxClient`] vocabulary:
//! - a csw *workspace* is a Rex **session** (one sidebar entry per task);
//! - a *pane* and a *surface* are both a Rex **block**: Rex has no tabs
//!   inside a pane, so `pane_id == surface_id`;
//! - an extra tab ([`MuxClient::new_surface`]) becomes a new Rex **window**
//!   (a tab in the session);
//! - workspace colors aren't supported and are a silent no-op;
//! - bringing a session to the front is a *client action*
//!   (`session.select`) that only works with Remote Control enabled in the
//!   app, so it's attempted but its failure is ignored.
//!
//! Every block-scoped command needs its session too (`-s`), so the client
//! remembers which session each block it has seen belongs to.

use super::client::{
    MuxClient, MuxError, NewWorkspace, SplitDirection, SurfaceInfo, SurfaceRef, Workspace,
};
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

/// Env var Rex exports into every terminal, holding the current session id.
/// Its presence is how we know csw is running inside Rex.
pub const SESSION_ENV_VAR: &str = "REX_SESSION";

/// Override for the `rex` binary (defaults to `rex` on `$PATH`).
const BIN_ENV_VAR: &str = "CSW_REX_BIN";

/// True when csw is running inside a Rex terminal.
pub fn detect() -> bool {
    std::env::var_os(SESSION_ENV_VAR).is_some_and(|v| !v.is_empty())
}

/// Executes one `rex` invocation and returns its stdout. Abstracted so the
/// argument building and JSON parsing can be tested without a Rex server.
pub trait RexRunner {
    fn run(&mut self, args: &[String], stdin: Option<&str>) -> Result<String, MuxError>;
}

/// Runs the real `rex` binary.
pub struct ProcessRunner {
    bin: String,
}

impl ProcessRunner {
    pub fn from_env() -> Self {
        let bin = std::env::var(BIN_ENV_VAR)
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "rex".to_string());
        Self { bin }
    }
}

impl RexRunner for ProcessRunner {
    fn run(&mut self, args: &[String], stdin: Option<&str>) -> Result<String, MuxError> {
        let mut cmd = Command::new(&self.bin);
        // Never spin up a fresh server as a side effect of csw: if Rex isn't
        // running, that's an "unavailable" warning, not a new daemon.
        cmd.arg("--autostart=false")
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| MuxError::Unavailable(format!("could not run `{}`: {e}", self.bin)))?;
        if let Some(text) = stdin {
            // Take the handle so it's dropped (closing the pipe) before we
            // wait, otherwise `rex send` would block reading stdin forever.
            let mut pipe = child.stdin.take().expect("stdin was piped");
            pipe.write_all(text.as_bytes())?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let message = stderr
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("exited with an error")
                .trim_start_matches("Error: ")
                .to_string();
            return Err(MuxError::Method {
                method: args.first().cloned().unwrap_or_default(),
                message,
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// Connect to Rex via the CLI. Probes the server once with `rex ls` so an
/// unreachable server surfaces as a single connect-time warning.
pub fn connect() -> Result<RexClient<ProcessRunner>, MuxError> {
    let mut client = RexClient::new(ProcessRunner::from_env());
    client
        .runner
        .run(&args(["ls", "--json"]), None)
        .map_err(|e| match e {
            MuxError::Method { message, .. } => MuxError::Unavailable(message),
            other => other,
        })?;
    Ok(client)
}

pub struct RexClient<R: RexRunner> {
    runner: R,
    /// block id → session id, for every block this client created or saw.
    block_sessions: HashMap<String, String>,
}

impl<R: RexRunner> RexClient<R> {
    pub fn new(runner: R) -> Self {
        Self {
            runner,
            block_sessions: HashMap::new(),
        }
    }

    fn run_json(&mut self, args: Vec<String>) -> Result<Value, MuxError> {
        let out = self.runner.run(&args, None)?;
        serde_json::from_str(out.trim()).map_err(|e| {
            MuxError::Protocol(format!(
                "parse `rex {}` output: {e}",
                args.first().map(String::as_str).unwrap_or("")
            ))
        })
    }

    fn session_of(&self, block_id: &str) -> Result<String, MuxError> {
        self.block_sessions
            .get(block_id)
            .cloned()
            .ok_or_else(|| MuxError::Protocol(format!("unknown rex block `{block_id}`")))
    }

    fn remember(&mut self, block_id: &str, session_id: &str) {
        self.block_sessions
            .insert(block_id.to_string(), session_id.to_string());
    }

    /// Record and return a block created in `session_id`, read from
    /// `.block_ids[0]` of a creation result.
    fn created_block(&mut self, result: &Value, session_id: &str) -> Result<SurfaceRef, MuxError> {
        let block = first_block_id(result).ok_or(MuxError::MissingField("block_ids"))?;
        self.remember(&block, session_id);
        Ok(block_ref(block))
    }
}

fn args<const N: usize>(items: [&str; N]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn block_ref(block_id: String) -> SurfaceRef {
    SurfaceRef {
        pane_id: block_id.clone(),
        surface_id: block_id,
    }
}

fn first_block_id(v: &Value) -> Option<String> {
    v.get("block_ids")?
        .as_array()?
        .first()?
        .as_str()
        .map(str::to_string)
}

fn str_field(v: &Value, field: &'static str) -> Result<String, MuxError> {
    v.get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or(MuxError::MissingField(field))
}

fn split_token(direction: SplitDirection) -> &'static str {
    match direction {
        SplitDirection::Left => "left",
        SplitDirection::Right => "right",
        SplitDirection::Up => "above",
        SplitDirection::Down => "below",
    }
}

impl<R: RexRunner> MuxClient for RexClient<R> {
    fn current_workspace_id(&self) -> Option<String> {
        std::env::var(SESSION_ENV_VAR)
            .ok()
            .filter(|s| !s.is_empty())
    }

    fn list_workspaces(&mut self) -> Result<Vec<Workspace>, MuxError> {
        let result = self.run_json(args(["ls", "--json"]))?;
        let arr = result
            .get("sessions")
            .and_then(Value::as_array)
            .ok_or(MuxError::MissingField("sessions"))?;
        arr.iter()
            .map(|s| {
                Ok(Workspace {
                    id: str_field(s, "session_id")?,
                    name: s
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                })
            })
            .collect()
    }

    fn new_workspace(&mut self, title: &str) -> Result<NewWorkspace, MuxError> {
        let result = self.run_json(vec!["new".into(), title.into(), "--json".into()])?;
        let session_id = str_field(&result, "session_id")?;
        let window = result
            .get("initial_windows")
            .and_then(Value::as_array)
            .and_then(|w| w.first())
            .ok_or(MuxError::MissingField("initial_windows"))?;
        let initial_surface = self.created_block(window, &session_id)?;
        Ok(NewWorkspace {
            workspace_id: session_id,
            initial_surface,
        })
    }

    fn select_workspace(&mut self, workspace_id: &str) -> Result<(), MuxError> {
        // A client action: needs the app running with Remote Control on, and
        // exactly one app connected. Purely cosmetic, so never fail on it.
        let _ = self.runner.run(
            &[
                "do".into(),
                "session.select".into(),
                format!("session_id={workspace_id}"),
            ],
            None,
        );
        Ok(())
    }

    fn close_workspace(&mut self, workspace_id: &str) -> Result<(), MuxError> {
        self.runner
            .run(&["kill".into(), workspace_id.into()], None)?;
        Ok(())
    }

    fn rename_workspace(&mut self, workspace_id: &str, title: &str) -> Result<(), MuxError> {
        self.runner.run(
            &[
                "session".into(),
                "rename".into(),
                workspace_id.into(),
                title.into(),
            ],
            None,
        )?;
        Ok(())
    }

    /// Rex sessions have no accent color.
    fn set_workspace_color(&mut self, _workspace_id: &str, _color: &str) -> Result<(), MuxError> {
        Ok(())
    }

    fn new_split(
        &mut self,
        anchor_surface: &str,
        direction: SplitDirection,
        percent: u8,
    ) -> Result<SurfaceRef, MuxError> {
        let session = self.session_of(anchor_surface)?;
        let result = self.run_json(vec![
            "split".into(),
            "-s".into(),
            session.clone(),
            "-b".into(),
            anchor_surface.into(),
            format!("--split={}", split_token(direction)),
            format!("--ratio={}", percent.clamp(1, 99)),
            "--focus=false".into(),
            "--json".into(),
        ])?;
        self.created_block(&result, &session)
    }

    fn new_surface(&mut self, pane_id: &str) -> Result<SurfaceRef, MuxError> {
        let session = self.session_of(pane_id)?;
        let result = self.run_json(vec![
            "window".into(),
            "new".into(),
            "-s".into(),
            session.clone(),
            "--focus=false".into(),
            "--json".into(),
        ])?;
        self.created_block(&result, &session)
    }

    fn list_surfaces(&mut self, workspace_id: &str) -> Result<Vec<SurfaceInfo>, MuxError> {
        let result = self.run_json(vec![
            "session".into(),
            "inspect".into(),
            workspace_id.into(),
            "--json".into(),
        ])?;
        let surfaces = parse_session_blocks(&result)?;
        for s in &surfaces {
            self.remember(&s.surface_ref.surface_id, workspace_id);
        }
        Ok(surfaces)
    }

    fn focus_surface(&mut self, surface_id: &str) -> Result<(), MuxError> {
        let session = self.session_of(surface_id)?;
        self.runner.run(
            &["focus".into(), "-s".into(), session, surface_id.into()],
            None,
        )?;
        Ok(())
    }

    fn send(&mut self, surface_id: &str, text: &str) -> Result<(), MuxError> {
        let session = self.session_of(surface_id)?;
        // Text goes through stdin so nothing in it can be mistaken for a flag.
        self.runner.run(
            &[
                "send".into(),
                "-s".into(),
                session,
                "-b".into(),
                surface_id.into(),
            ],
            Some(text),
        )?;
        Ok(())
    }
}

/// Flatten `rex session inspect --json` into one [`SurfaceInfo`] per block,
/// across every window and layer. A block is focused when it's the focused
/// block of the session's active window.
fn parse_session_blocks(result: &Value) -> Result<Vec<SurfaceInfo>, MuxError> {
    let active_window = result.get("active_window_id").and_then(Value::as_str);
    let windows = result
        .get("windows")
        .and_then(Value::as_array)
        .ok_or(MuxError::MissingField("windows"))?;
    let mut out = Vec::new();
    for window in windows {
        let is_active = window.get("window_id").and_then(Value::as_str) == active_window;
        let focused_block = window.get("focused_block_id").and_then(Value::as_str);
        let layers = window
            .get("layers")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for block in layers
            .iter()
            .filter_map(|l| l.get("blocks").and_then(Value::as_array))
            .flatten()
        {
            let id = str_field(block, "block_id")?;
            let focused = is_active && focused_block == Some(id.as_str());
            out.push(SurfaceInfo {
                surface_ref: block_ref(id),
                focused,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Records every invocation and replies with canned stdout, in order.
    #[derive(Default)]
    struct FakeRunner {
        calls: Vec<(Vec<String>, Option<String>)>,
        replies: VecDeque<Result<String, MuxError>>,
    }

    impl FakeRunner {
        fn reply(mut self, json: Value) -> Self {
            self.replies.push_back(Ok(json.to_string()));
            self
        }
        fn fail(mut self) -> Self {
            self.replies.push_back(Err(MuxError::Method {
                method: "do".into(),
                message: "remote control is off".into(),
            }));
            self
        }
    }

    impl RexRunner for FakeRunner {
        fn run(&mut self, args: &[String], stdin: Option<&str>) -> Result<String, MuxError> {
            self.calls.push((args.to_vec(), stdin.map(str::to_string)));
            self.replies
                .pop_front()
                .unwrap_or_else(|| Ok(String::new()))
        }
    }

    fn argv(call: &(Vec<String>, Option<String>)) -> Vec<&str> {
        call.0.iter().map(String::as_str).collect()
    }

    fn new_session_reply() -> Value {
        serde_json::json!({
            "session_id": "session:1",
            "initial_windows": [{ "window_id": "window:1", "block_ids": ["block:1"] }]
        })
    }

    #[test]
    fn list_workspaces_reads_session_labels() {
        let runner = FakeRunner::default().reply(serde_json::json!({
            "sessions": [
                { "session_id": "session:a", "label": "PROJ-1 · Fix" },
                { "session_id": "session:b" },
            ]
        }));
        let mut client = RexClient::new(runner);
        let ws = client.list_workspaces().unwrap();
        assert_eq!(
            ws,
            vec![
                Workspace {
                    id: "session:a".into(),
                    name: "PROJ-1 · Fix".into()
                },
                Workspace {
                    id: "session:b".into(),
                    name: String::new()
                },
            ]
        );
    }

    #[test]
    fn new_workspace_returns_initial_block_and_tracks_its_session() {
        let runner = FakeRunner::default()
            .reply(new_session_reply())
            .reply(serde_json::json!({ "block_ids": ["block:2"] }));
        let mut client = RexClient::new(runner);

        let created = client.new_workspace("PROJ-1").unwrap();
        assert_eq!(created.workspace_id, "session:1");
        assert_eq!(created.initial_surface.surface_id, "block:1");
        assert_eq!(created.initial_surface.pane_id, "block:1");

        // The split anchored on the initial block is scoped to its session.
        let split = client
            .new_split("block:1", SplitDirection::Down, 50)
            .unwrap();
        assert_eq!(split.surface_id, "block:2");

        let calls = &client.runner.calls;
        assert_eq!(argv(&calls[0]), ["new", "PROJ-1", "--json"]);
        assert_eq!(
            argv(&calls[1]),
            [
                "split",
                "-s",
                "session:1",
                "-b",
                "block:1",
                "--split=below",
                "--ratio=50",
                "--focus=false",
                "--json"
            ]
        );
    }

    #[test]
    fn blocks_created_by_splits_are_usable_as_anchors() {
        let runner = FakeRunner::default()
            .reply(new_session_reply())
            .reply(serde_json::json!({ "block_ids": ["block:2"] }))
            .reply(serde_json::json!({ "block_ids": ["block:3"] }));
        let mut client = RexClient::new(runner);
        client.new_workspace("PROJ-1").unwrap();
        client
            .new_split("block:1", SplitDirection::Right, 67)
            .unwrap();
        client
            .new_split("block:2", SplitDirection::Right, 50)
            .unwrap();
        assert_eq!(argv(&client.runner.calls[2])[2], "session:1");
    }

    #[test]
    fn new_surface_opens_a_window_in_the_pane_session() {
        let runner = FakeRunner::default()
            .reply(new_session_reply())
            .reply(serde_json::json!({ "window_id": "window:2", "block_ids": ["block:9"] }));
        let mut client = RexClient::new(runner);
        client.new_workspace("PROJ-1").unwrap();
        let tab = client.new_surface("block:1").unwrap();
        assert_eq!(tab.surface_id, "block:9");
        assert_eq!(
            argv(&client.runner.calls[1]),
            [
                "window",
                "new",
                "-s",
                "session:1",
                "--focus=false",
                "--json"
            ]
        );
    }

    #[test]
    fn send_pipes_text_through_stdin() {
        let runner = FakeRunner::default().reply(new_session_reply());
        let mut client = RexClient::new(runner);
        client.new_workspace("PROJ-1").unwrap();
        client.send("block:1", " cd '/tmp' && nvim\n").unwrap();
        let call = &client.runner.calls[1];
        assert_eq!(argv(call), ["send", "-s", "session:1", "-b", "block:1"]);
        assert_eq!(call.1.as_deref(), Some(" cd '/tmp' && nvim\n"));
    }

    #[test]
    fn focus_scopes_to_the_block_session() {
        let runner = FakeRunner::default().reply(new_session_reply());
        let mut client = RexClient::new(runner);
        client.new_workspace("PROJ-1").unwrap();
        client.focus_surface("block:1").unwrap();
        assert_eq!(
            argv(&client.runner.calls[1]),
            ["focus", "-s", "session:1", "block:1"]
        );
    }

    #[test]
    fn unknown_block_is_an_error_not_a_guess() {
        let mut client = RexClient::new(FakeRunner::default());
        assert!(client.send("block:nope", "x").is_err());
        assert!(client.runner.calls.is_empty());
    }

    #[test]
    fn select_workspace_swallows_client_action_failures() {
        let mut client = RexClient::new(FakeRunner::default().fail());
        client.select_workspace("session:1").unwrap();
        assert_eq!(
            argv(&client.runner.calls[0]),
            ["do", "session.select", "session_id=session:1"]
        );
    }

    #[test]
    fn close_and_rename_use_session_commands() {
        let mut client = RexClient::new(FakeRunner::default());
        client
            .rename_workspace("session:1", "PROJ-1 · New")
            .unwrap();
        client.close_workspace("session:1").unwrap();
        assert_eq!(
            argv(&client.runner.calls[0]),
            ["session", "rename", "session:1", "PROJ-1 · New"]
        );
        assert_eq!(argv(&client.runner.calls[1]), ["kill", "session:1"]);
    }

    #[test]
    fn color_is_a_no_op() {
        let mut client = RexClient::new(FakeRunner::default());
        client.set_workspace_color("session:1", "#C62828").unwrap();
        assert!(client.runner.calls.is_empty());
    }

    #[test]
    fn list_surfaces_flattens_windows_and_marks_focus() {
        let runner = FakeRunner::default().reply(serde_json::json!({
            "session_id": "session:1",
            "active_window_id": "window:2",
            "windows": [
                {
                    "window_id": "window:1",
                    "focused_block_id": "block:a",
                    "layers": [{ "blocks": [{ "block_id": "block:a" }] }]
                },
                {
                    "window_id": "window:2",
                    "focused_block_id": "block:c",
                    "layers": [{ "blocks": [{ "block_id": "block:b" }, { "block_id": "block:c" }] }]
                }
            ]
        }));
        let mut client = RexClient::new(runner);
        let surfaces = client.list_surfaces("session:1").unwrap();
        let summary: Vec<(&str, bool)> = surfaces
            .iter()
            .map(|s| (s.surface_ref.surface_id.as_str(), s.focused))
            .collect();
        assert_eq!(
            summary,
            [("block:a", false), ("block:b", false), ("block:c", true)]
        );
        // Listed blocks become addressable.
        client.focus_surface("block:b").unwrap();
    }
}
