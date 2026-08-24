//! Workspace build algorithm — turns a list of repo contributors into a CMux
//! workspace with a fixed layout: a top row of one pane per repo (running
//! that repo's editor, when editors are attached), and a single bottom pane
//! running the task-level agent command, rooted at the task's root folder.
//!
//! Idempotent: a workspace named `csw/<task-id>` is reused if it already
//! exists. No reconcile — config drift since the workspace was first built
//! is silently ignored.
//!
//! When `csw start` runs inside a CMux surface and the current workspace
//! has exactly one pane and one surface (no splits, no extra tabs), the
//! build path can opt to reshape that current workspace in place instead
//! of opening a new sidebar entry. The current workspace is identified
//! from the `CMUX_WORKSPACE_ID` env var; the gating is controlled by the
//! caller (the config knob plus a per-invocation override flag).
//!
//! Every step is best-effort. The first error surfaces as a single
//! [`CmuxError`] return; partial workspaces (e.g. half the splits applied)
//! are left in place rather than rolled back, since the user can just close
//! the sidebar entry by hand if it's a mess.

use super::client::{CmuxClient, CmuxError, SplitDirection, SurfaceRef, shell_quote};
use std::path::{Path, PathBuf};

/// One repo's contribution to the workspace's top row.
#[derive(Debug, Clone)]
pub struct Contributor {
    pub repo: String,
    pub worktree_path: PathBuf,
    /// The repo's `editor` command. `None`/empty means the pane just `cd`s
    /// into the worktree with no command autostarting.
    pub editor_cmd: Option<String>,
    /// Extra idle tabs (CMux surfaces) stacked behind the editor surface,
    /// each `cd`'d into the same worktree with nothing autostarting.
    pub additional_panels: u32,
}

/// Task-level layout inputs, orthogonal to workspace-adoption behavior
/// ([`BuildOptions`]).
#[derive(Debug, Clone)]
pub struct WorkspaceSpec {
    /// Root folder for the task, visible to every repo it was started with.
    /// The bottom pane's agent command runs here.
    pub task_root: PathBuf,
    /// Command for the bottom pane. `None` means no bottom pane is built.
    pub agent_cmd: Option<String>,
    /// When `true`, the top row is built (one pane per contributor running
    /// its `editor_cmd`). When `false` (detached editors), no top row is
    /// built — editors are expected to have been spawned as detached
    /// processes elsewhere.
    pub editors_attached: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildOutcome {
    /// Workspace was found by name and just focused — no new splits.
    Reused { workspace_id: String, name: String },
    /// Workspace was created from scratch and populated.
    Created { workspace_id: String, name: String },
    /// The current workspace was simple (single pane, single surface) and
    /// got reshaped in place: renamed, recolored, then populated with the
    /// task's layout. Surfaced separately from `Created` so the caller can
    /// tell the user we took over their current sidebar entry instead of
    /// opening a new one.
    Adopted { workspace_id: String, name: String },
    /// Nothing to show — no top row (editors not attached, or zero
    /// contributors) and no agent command configured.
    NoContributors,
}

impl BuildOutcome {
    pub fn workspace_name(&self) -> Option<&str> {
        match self {
            BuildOutcome::Reused { name, .. }
            | BuildOutcome::Created { name, .. }
            | BuildOutcome::Adopted { name, .. } => Some(name),
            BuildOutcome::NoContributors => None,
        }
    }
}

/// Caller-facing knobs for [`build_workspace`]. Kept as a struct so the
/// signature doesn't grow a long boolean tail every time a new toggle
/// shows up.
#[derive(Debug, Clone, Copy, Default)]
pub struct BuildOptions {
    /// When true, reshape the current CMux workspace in place if it has
    /// exactly one pane and one surface. False suppresses the adoption
    /// path and always creates a new workspace (the historical behavior).
    pub replace_simple_workspace: bool,
    /// Per-invocation override that wins over `replace_simple_workspace`:
    /// when true, never adopt — always create a new workspace.
    pub force_new_workspace: bool,
}

/// Separator used between the task id and the human title in the workspace
/// sidebar name. Chosen so the boundary is visually unambiguous and easy to
/// match programmatically.
const TITLE_SEPARATOR: &str = " · ";

/// Compute the workspace sidebar title from a task id and an optional human
/// title. `"PROJ-123"` alone, or `"PROJ-123 · Fix navbar bug"` if a title is
/// present.
pub fn workspace_name_for(task_id: &str, title: Option<&str>) -> String {
    match title {
        Some(t) if !t.trim().is_empty() => format!("{task_id}{TITLE_SEPARATOR}{}", t.trim()),
        _ => task_id.to_string(),
    }
}

/// Does `entry_name` belong to this task? Matches both the no-title form
/// (exact equality) and the with-title form (prefix `<task-id> · `).
fn matches_task(entry_name: &str, task_id: &str) -> bool {
    if entry_name == task_id {
        return true;
    }
    let prefix = format!("{task_id}{TITLE_SEPARATOR}");
    entry_name.starts_with(&prefix)
}

/// Deterministic per-task accent color. The same task id always picks the
/// same color across runs (a stable FNV-1a hash mod palette size), so a
/// rebuilt workspace looks the same as the original one in the sidebar.
pub fn color_for_task(task_id: &str) -> &'static str {
    const PALETTE: &[&str] = &[
        "#0277BD", // dark blue
        "#2E7D32", // dark green
        "#C62828", // dark red
        "#EF6C00", // dark orange
        "#6A1B9A", // dark purple
        "#00838F", // dark cyan
        "#283593", // dark indigo
        "#AD1457", // dark pink
        "#00695C", // dark teal
        "#4527A0", // deep purple
    ];
    let mut h: u32 = 2166136261; // FNV-1a offset basis
    for &b in task_id.as_bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619); // FNV-1a prime
    }
    PALETTE[(h as usize) % PALETTE.len()]
}

/// Whether the layout has anything to build at all. Exposed so callers can
/// short-circuit before connecting to CMux at all.
pub fn has_content(contributors: &[Contributor], spec: &WorkspaceSpec) -> bool {
    let has_top = spec.editors_attached && !contributors.is_empty();
    let has_bottom = spec.agent_cmd.is_some();
    has_top || has_bottom
}

/// Build (or reuse) the workspace for this task.
pub fn build_workspace(
    client: &mut dyn CmuxClient,
    task_id: &str,
    title: Option<&str>,
    contributors: &[Contributor],
    spec: &WorkspaceSpec,
    options: BuildOptions,
) -> Result<BuildOutcome, CmuxError> {
    if !has_content(contributors, spec) {
        return Ok(BuildOutcome::NoContributors);
    }

    let name = workspace_name_for(task_id, title);

    let existing = client.list_workspaces()?;
    if let Some(ws) = existing.iter().find(|w| matches_task(&w.name, task_id)) {
        client.select_workspace(&ws.id)?;
        return Ok(BuildOutcome::Reused {
            workspace_id: ws.id.clone(),
            name: ws.name.clone(),
        });
    }

    // Try to take over the current workspace in place when (a) the caller
    // opted in via the config knob, (b) the user didn't override with
    // `--force-new-workspace`, and (c) the current workspace is actually
    // simple. Any failure or ambiguity falls through to the create path.
    if options.replace_simple_workspace && !options.force_new_workspace {
        if let Some(current_id) = current_workspace_id() {
            if let Some(adopted) = try_adopt_workspace(
                client,
                &existing,
                &current_id,
                task_id,
                &name,
                contributors,
                spec,
            )? {
                return Ok(adopted);
            }
        }
    }

    let created = client.new_workspace(&name)?;
    // Apply the accent color. Failure here is the same soft failure as any
    // other CMux call — surfaced via the caller's `CmuxOutcome`.
    client.set_workspace_color(&created.workspace_id, color_for_task(task_id))?;

    populate_workspace(client, &created.initial_surface, contributors, spec)?;
    client.select_workspace(&created.workspace_id)?;

    Ok(BuildOutcome::Created {
        workspace_id: created.workspace_id,
        name,
    })
}

/// Fan repos out left-to-right off `seed`, one pane per contributor, each
/// running its `editor_cmd`. Returns the created slots in repo order.
fn fan_out_repos(
    client: &mut dyn CmuxClient,
    seed: &SurfaceRef,
    contributors: &[Contributor],
) -> Result<Vec<SurfaceRef>, CmuxError> {
    let mut slots: Vec<SurfaceRef> = Vec::with_capacity(contributors.len());
    slots.push(seed.clone());
    for prev_idx in 0..contributors.len().saturating_sub(1) {
        let anchor = slots[prev_idx].clone();
        client.focus_surface(&anchor.surface_id)?;
        let new_slot = client.new_split(&anchor.surface_id, SplitDirection::Right)?;
        slots.push(new_slot);
    }

    for (slot, contributor) in slots.iter().zip(contributors) {
        client.focus_surface(&slot.surface_id)?;
        let cmd = pane_command(
            &contributor.worktree_path,
            contributor.editor_cmd.as_deref(),
        );
        client.send(&slot.surface_id, &cmd)?;

        for _ in 0..contributor.additional_panels {
            let tab = client.new_surface(&slot.pane_id)?;
            let tab_cmd = pane_command(&contributor.worktree_path, None);
            client.send(&tab.surface_id, &tab_cmd)?;
        }
        if contributor.additional_panels > 0 {
            // Bring the foreground editor surface back into focus after
            // stacking the extra idle tabs behind it.
            client.focus_surface(&slot.surface_id)?;
        }
    }

    Ok(slots)
}

/// Lay out the workspace anchored at `seed_surface`, following `spec`:
/// top row of repo editors, bottom agent pane, or just one of the two.
fn populate_workspace(
    client: &mut dyn CmuxClient,
    seed_surface: &SurfaceRef,
    contributors: &[Contributor],
    spec: &WorkspaceSpec,
) -> Result<(), CmuxError> {
    let has_top = spec.editors_attached && !contributors.is_empty();
    let has_bottom = spec.agent_cmd.is_some();

    if has_top && has_bottom {
        client.focus_surface(&seed_surface.surface_id)?;
        let bottom = client.new_split(&seed_surface.surface_id, SplitDirection::Down)?;
        let top_slots = fan_out_repos(client, seed_surface, contributors)?;
        let agent_text = pane_command(&spec.task_root, spec.agent_cmd.as_deref());
        client.send(&bottom.surface_id, &agent_text)?;
        client.focus_surface(&top_slots[0].surface_id)?;
    } else if has_top {
        let top_slots = fan_out_repos(client, seed_surface, contributors)?;
        client.focus_surface(&top_slots[0].surface_id)?;
    } else {
        // has_bottom only — the caller already verified has_top || has_bottom.
        let agent_text = pane_command(&spec.task_root, spec.agent_cmd.as_deref());
        client.send(&seed_surface.surface_id, &agent_text)?;
        client.focus_surface(&seed_surface.surface_id)?;
    }
    Ok(())
}

/// Attempt to reshape the given workspace in place. Returns
/// `Ok(Some(Adopted))` on success, `Ok(None)` when the workspace doesn't
/// exist on the CMux side or isn't simple enough to adopt, or `Err(...)`
/// if a CMux call genuinely failed.
#[allow(clippy::too_many_arguments)]
fn try_adopt_workspace(
    client: &mut dyn CmuxClient,
    existing: &[super::client::Workspace],
    current_id: &str,
    task_id: &str,
    name: &str,
    contributors: &[Contributor],
    spec: &WorkspaceSpec,
) -> Result<Option<BuildOutcome>, CmuxError> {
    // The env var should always point at one of the listed workspaces; if
    // it doesn't (e.g. CMux closed it from under us), fall back to create
    // rather than risk renaming the wrong workspace.
    if !existing.iter().any(|w| w.id == current_id) {
        return Ok(None);
    }

    let surfaces = client.list_surfaces(current_id)?;
    let Some(seed) = simple_workspace_surface(&surfaces) else {
        return Ok(None);
    };

    client.rename_workspace(current_id, name)?;
    client.set_workspace_color(current_id, color_for_task(task_id))?;
    populate_workspace(client, &seed, contributors, spec)?;
    client.select_workspace(current_id)?;

    Ok(Some(BuildOutcome::Adopted {
        workspace_id: current_id.to_string(),
        name: name.to_string(),
    }))
}

/// `CMUX_WORKSPACE_ID` as a non-empty string. Empty values (some CMux
/// builds export the var blank) are treated as absent.
fn current_workspace_id() -> Option<String> {
    let raw = std::env::var(super::WORKSPACE_ENV_VAR).ok()?;
    if raw.is_empty() { None } else { Some(raw) }
}

/// Return the single surface of a workspace iff it qualifies as "simple":
/// exactly one surface, in exactly one pane. Anything else (zero, multiple
/// surfaces, multiple distinct panes) returns `None`.
fn simple_workspace_surface(surfaces: &[super::client::SurfaceInfo]) -> Option<SurfaceRef> {
    if surfaces.len() != 1 {
        return None;
    }
    let only = &surfaces[0];
    // Belt-and-suspenders: also confirm there's exactly one pane. A single
    // surface should already imply a single pane, but this guards against
    // a future CMux that reports surfaces differently.
    let mut pane_ids = std::collections::BTreeSet::new();
    for s in surfaces {
        pane_ids.insert(&s.surface_ref.pane_id);
    }
    if pane_ids.len() != 1 {
        return None;
    }
    Some(only.surface_ref.clone())
}

/// Build the shell snippet sent to a pane. Leading space exploits
/// `HISTCONTROL=ignorespace` to keep the cd line out of shell history; the
/// cd command itself uses single-quoted shell-quoting so paths with spaces
/// or special characters survive intact. A `None`/empty command produces
/// just the cd — the pane lands on a shell prompt rooted in the worktree,
/// nothing autostarting.
fn pane_command(worktree: &Path, cmd: Option<&str>) -> String {
    let path = worktree.to_string_lossy();
    let quoted = shell_quote(&path);
    match cmd.filter(|c| !c.is_empty()) {
        Some(c) => format!(" cd {quoted} && {c}\n"),
        None => format!(" cd {quoted}\n"),
    }
}

/// Close the workspace for this task if it exists. Matches by the same
/// task-id-prefix rule [`build_workspace`] uses for lookup. Returns
/// `Ok(true)` if something was closed, `Ok(false)` if no matching workspace
/// was found.
pub fn close_workspace_for(client: &mut dyn CmuxClient, task_id: &str) -> Result<bool, CmuxError> {
    let existing = client.list_workspaces()?;
    let Some(ws) = existing
        .into_iter()
        .find(|w| matches_task(&w.name, task_id))
    else {
        return Ok(false);
    };
    client.close_workspace(&ws.id)?;
    Ok(true)
}

/// Rename the workspace for this task if it exists. Computes the new sidebar
/// name via [`workspace_name_for`], so passing `None` resets the label to
/// just the task id. Returns the new name on success, or `Ok(None)` if no
/// matching workspace was found.
pub fn rename_workspace_for(
    client: &mut dyn CmuxClient,
    task_id: &str,
    title: Option<&str>,
) -> Result<Option<String>, CmuxError> {
    let existing = client.list_workspaces()?;
    let Some(ws) = existing
        .into_iter()
        .find(|w| matches_task(&w.name, task_id))
    else {
        return Ok(None);
    };
    let new_name = workspace_name_for(task_id, title);
    client.rename_workspace(&ws.id, &new_name)?;
    Ok(Some(new_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmux::client::Workspace;
    use crate::cmux::client::testing::{Call, RecordingClient};
    use std::path::PathBuf;

    fn contributor(name: &str, worktree: &str, editor_cmd: Option<&str>) -> Contributor {
        Contributor {
            repo: name.into(),
            worktree_path: PathBuf::from(worktree),
            editor_cmd: editor_cmd.map(String::from),
            additional_panels: 0,
        }
    }

    fn contributor_with_panels(
        name: &str,
        worktree: &str,
        editor_cmd: Option<&str>,
        additional_panels: u32,
    ) -> Contributor {
        Contributor {
            additional_panels,
            ..contributor(name, worktree, editor_cmd)
        }
    }

    fn attached_spec(task_root: &str, agent_cmd: Option<&str>) -> WorkspaceSpec {
        WorkspaceSpec {
            task_root: PathBuf::from(task_root),
            agent_cmd: agent_cmd.map(String::from),
            editors_attached: true,
        }
    }

    fn detached_spec(task_root: &str, agent_cmd: Option<&str>) -> WorkspaceSpec {
        WorkspaceSpec {
            task_root: PathBuf::from(task_root),
            agent_cmd: agent_cmd.map(String::from),
            editors_attached: false,
        }
    }

    #[test]
    fn no_content_short_circuits_without_calls() {
        let mut client = RecordingClient::new();
        let spec = detached_spec("/csw/tasks/alice-PROJ-1", None);
        let outcome =
            build_workspace(&mut client, "PROJ-1", None, &[], &spec, BuildOptions::default())
                .unwrap();
        assert_eq!(outcome, BuildOutcome::NoContributors);
        assert!(client.calls().is_empty());
    }

    #[test]
    fn detached_with_no_editors_and_no_agent_is_no_contributors() {
        let mut client = RecordingClient::new();
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/alice-PROJ-1/fe",
            Some("pnpm dev"),
        )];
        let spec = detached_spec("/csw/tasks/alice-PROJ-1", None);
        let outcome = build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            &spec,
            BuildOptions::default(),
        )
        .unwrap();
        // Detached editors never build a top row regardless of contributors.
        assert_eq!(outcome, BuildOutcome::NoContributors);
    }

    #[test]
    fn existing_workspace_is_reused_no_splits() {
        let mut client = RecordingClient::new().with_existing(vec![Workspace {
            id: "ws-existing".into(),
            name: "PROJ-1".into(),
        }]);
        let contribs = vec![contributor("fe", "/csw/tasks/alice-PROJ-1/fe", None)];
        let spec = attached_spec("/csw/tasks/alice-PROJ-1", Some("claude"));

        let outcome = build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            &spec,
            BuildOptions::default(),
        )
        .unwrap();
        assert!(matches!(outcome, BuildOutcome::Reused { .. }));

        let calls = client.calls();
        assert_eq!(calls.len(), 2);
        assert!(matches!(calls[0], Call::ListWorkspaces));
        assert!(matches!(&calls[1], Call::SelectWorkspace { id } if id == "ws-existing"));
    }

    #[test]
    fn existing_workspace_is_reused_when_only_task_id_matches() {
        let mut client = RecordingClient::new().with_existing(vec![Workspace {
            id: "ws-with-title".into(),
            name: "PROJ-1 · Fix navbar bug".into(),
        }]);
        let contribs = vec![contributor("fe", "/csw/tasks/alice-PROJ-1/fe", None)];
        let spec = attached_spec("/csw/tasks/alice-PROJ-1", Some("claude"));

        let outcome = build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            &spec,
            BuildOptions::default(),
        )
        .unwrap();
        assert!(
            matches!(outcome, BuildOutcome::Reused { ref name, .. } if name == "PROJ-1 · Fix navbar bug")
        );
    }

    #[test]
    fn workspace_name_falls_back_to_task_id_without_title() {
        assert_eq!(workspace_name_for("PROJ-1", None), "PROJ-1");
        assert_eq!(workspace_name_for("PROJ-1", Some("")), "PROJ-1");
        assert_eq!(workspace_name_for("PROJ-1", Some("   ")), "PROJ-1");
    }

    #[test]
    fn workspace_name_joins_task_id_and_title() {
        assert_eq!(
            workspace_name_for("PROJ-1", Some("Fix navbar bug")),
            "PROJ-1 · Fix navbar bug"
        );
    }

    #[test]
    fn matches_task_handles_both_forms() {
        assert!(matches_task("PROJ-1", "PROJ-1"));
        assert!(matches_task("PROJ-1 · Fix bug", "PROJ-1"));
        assert!(!matches_task("PROJ-12", "PROJ-1")); // not a prefix collision
        assert!(!matches_task("PROJ-1-extra", "PROJ-1"));
        assert!(!matches_task("other", "PROJ-1"));
    }

    #[test]
    fn color_for_task_is_deterministic_and_from_palette() {
        let a = color_for_task("PROJ-1");
        let b = color_for_task("PROJ-1");
        assert_eq!(a, b, "same task should always produce the same color");
        assert!(a.starts_with('#') && a.len() == 7, "got {a}");
    }

    #[test]
    fn workspace_create_uses_title_when_provided() {
        let mut client = RecordingClient::new();
        let contribs = vec![contributor("fe", "/csw/tasks/alice-PROJ-1/fe", None)];
        let spec = attached_spec("/csw/tasks/alice-PROJ-1", None);

        build_workspace(
            &mut client,
            "PROJ-1",
            Some("Fix navbar bug"),
            &contribs,
            &spec,
            BuildOptions::default(),
        )
        .unwrap();

        let title = client.calls().into_iter().find_map(|c| match c {
            Call::NewWorkspace { title } => Some(title),
            _ => None,
        });
        assert_eq!(title.as_deref(), Some("PROJ-1 · Fix navbar bug"));
    }

    #[test]
    fn attached_single_repo_no_agent_sends_editor_only() {
        let mut client = RecordingClient::new();
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/alice-PROJ-1/fe",
            Some("pnpm dev"),
        )];
        let spec = attached_spec("/csw/tasks/alice-PROJ-1", None);

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            &spec,
            BuildOptions::default(),
        )
        .unwrap();

        // No down-split since there's no agent to seat at the bottom.
        assert!(
            !client
                .calls()
                .iter()
                .any(|c| matches!(c, Call::NewSplit { direction: SplitDirection::Down, .. }))
        );
        let send = client
            .calls()
            .into_iter()
            .find_map(|c| match c {
                Call::Send { text, .. } => Some(text),
                _ => None,
            })
            .unwrap();
        assert!(send.contains("/csw/tasks/alice-PROJ-1/fe"));
        assert!(send.ends_with("pnpm dev\n"));
    }

    #[test]
    fn attached_with_agent_splits_down_first() {
        let mut client = RecordingClient::new();
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/alice-PROJ-1/fe",
            Some("nvim"),
        )];
        let spec = attached_spec("/csw/tasks/alice-PROJ-1", Some("claude"));

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            &spec,
            BuildOptions::default(),
        )
        .unwrap();

        let down_splits: Vec<_> = client
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::NewSplit { direction: SplitDirection::Down, .. }))
            .collect();
        assert_eq!(down_splits.len(), 1, "expected exactly one down-split");

        let sends: Vec<String> = client
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Send { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert!(sends.iter().any(|s| s.contains("nvim")));
        assert!(
            sends
                .iter()
                .any(|s| s.contains("/csw/tasks/alice-PROJ-1") && s.ends_with("claude\n"))
        );
    }

    #[test]
    fn two_repos_create_left_to_right_top_row() {
        let mut client = RecordingClient::new();
        let contribs = vec![
            contributor("fe", "/csw/tasks/alice-PROJ-1/fe", Some("pnpm dev")),
            contributor("be", "/csw/tasks/alice-PROJ-1/be", Some("pnpm dev")),
        ];
        let spec = attached_spec("/csw/tasks/alice-PROJ-1", Some("claude"));

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            &spec,
            BuildOptions::default(),
        )
        .unwrap();

        let right_splits: Vec<_> = client
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::NewSplit { direction: SplitDirection::Right, .. }))
            .collect();
        assert_eq!(right_splits.len(), 1, "two repos need exactly one right-split");
    }

    #[test]
    fn additional_panels_open_extra_idle_tabs_behind_editor() {
        let mut client = RecordingClient::new();
        let contribs = vec![contributor_with_panels(
            "fe",
            "/csw/tasks/alice-PROJ-1/fe",
            Some("nvim"),
            2,
        )];
        let spec = attached_spec("/csw/tasks/alice-PROJ-1", None);

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            &spec,
            BuildOptions::default(),
        )
        .unwrap();

        let new_surfaces: Vec<_> = client
            .calls()
            .into_iter()
            .filter(|c| matches!(c, Call::NewSurface { .. }))
            .collect();
        assert_eq!(new_surfaces.len(), 2, "expected 2 extra idle tabs");

        // The extra tabs just cd — no command appended.
        let sends: Vec<String> = client
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::Send { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(sends.len(), 3, "editor send + 2 idle tab sends");
        let idle_sends: Vec<&String> = sends.iter().filter(|s| !s.contains("nvim")).collect();
        assert_eq!(idle_sends.len(), 2);
        assert!(idle_sends.iter().all(|s| !s.contains("&&")));
    }

    #[test]
    fn zero_additional_panels_opens_no_extra_tabs() {
        let mut client = RecordingClient::new();
        let contribs = vec![contributor("fe", "/csw/tasks/alice-PROJ-1/fe", Some("nvim"))];
        let spec = attached_spec("/csw/tasks/alice-PROJ-1", None);

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            &spec,
            BuildOptions::default(),
        )
        .unwrap();

        assert!(
            !client
                .calls()
                .iter()
                .any(|c| matches!(c, Call::NewSurface { .. }))
        );
    }

    #[test]
    fn detached_editors_with_agent_builds_single_full_pane() {
        let mut client = RecordingClient::new();
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/alice-PROJ-1/fe",
            Some("pnpm dev"),
        )];
        let spec = detached_spec("/csw/tasks/alice-PROJ-1", Some("claude"));

        let outcome = build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            &spec,
            BuildOptions::default(),
        )
        .unwrap();
        assert!(matches!(outcome, BuildOutcome::Created { .. }));

        // No splits at all — single full pane.
        assert!(!client.calls().iter().any(|c| matches!(c, Call::NewSplit { .. })));
        let send = client
            .calls()
            .into_iter()
            .find_map(|c| match c {
                Call::Send { text, .. } => Some(text),
                _ => None,
            })
            .unwrap();
        assert!(send.contains("/csw/tasks/alice-PROJ-1"));
        assert!(send.ends_with("claude\n"));
        assert!(!send.contains("pnpm dev"));
    }

    #[test]
    fn pane_without_cmd_just_cds() {
        let s = pane_command(Path::new("/csw/tasks/alice-PROJ-1/fe"), None);
        assert_eq!(s, " cd '/csw/tasks/alice-PROJ-1/fe'\n");
    }

    #[test]
    fn pane_command_uses_leading_space_and_shell_quoting() {
        let s = pane_command(Path::new("/path with spaces"), Some("cmd"));
        assert!(s.starts_with(" cd '"), "leading space + single quote: {s}");
        assert!(s.contains("/path with spaces"));
        assert!(s.ends_with("cmd\n"));
    }

    #[test]
    fn close_workspace_returns_false_when_absent() {
        let mut client = RecordingClient::new();
        let closed = close_workspace_for(&mut client, "PROJ-1").unwrap();
        assert!(!closed);
    }

    #[test]
    fn rename_workspace_returns_none_when_absent() {
        let mut client = RecordingClient::new();
        let result = rename_workspace_for(&mut client, "PROJ-1", Some("Fix")).unwrap();
        assert_eq!(result, None);
        assert!(
            !client
                .calls()
                .iter()
                .any(|c| matches!(c, Call::RenameWorkspace { .. }))
        );
    }

    #[test]
    fn rename_workspace_renames_matching_workspace_with_title() {
        let mut client = RecordingClient::new().with_existing(vec![Workspace {
            id: "ws-target".into(),
            name: "PROJ-1".into(),
        }]);
        let result = rename_workspace_for(&mut client, "PROJ-1", Some("Fix navbar bug")).unwrap();
        assert_eq!(result.as_deref(), Some("PROJ-1 · Fix navbar bug"));

        let renamed = client.calls().into_iter().find_map(|c| match c {
            Call::RenameWorkspace { id, title } => Some((id, title)),
            _ => None,
        });
        assert_eq!(
            renamed,
            Some(("ws-target".into(), "PROJ-1 · Fix navbar bug".into()))
        );
    }

    #[test]
    fn rename_workspace_with_none_resets_to_task_id() {
        let mut client = RecordingClient::new().with_existing(vec![Workspace {
            id: "ws-target".into(),
            name: "PROJ-1 · Old title".into(),
        }]);
        let result = rename_workspace_for(&mut client, "PROJ-1", None).unwrap();
        assert_eq!(result.as_deref(), Some("PROJ-1"));

        let renamed_title = client.calls().into_iter().find_map(|c| match c {
            Call::RenameWorkspace { title, .. } => Some(title),
            _ => None,
        });
        assert_eq!(renamed_title.as_deref(), Some("PROJ-1"));
    }

    #[test]
    fn rename_workspace_with_empty_title_resets_to_task_id() {
        let mut client = RecordingClient::new().with_existing(vec![Workspace {
            id: "ws-target".into(),
            name: "PROJ-1 · Old title".into(),
        }]);
        let result = rename_workspace_for(&mut client, "PROJ-1", Some("")).unwrap();
        assert_eq!(result.as_deref(), Some("PROJ-1"));
    }

    #[test]
    fn close_workspace_closes_matching_workspace() {
        let mut client = RecordingClient::new().with_existing(vec![
            Workspace {
                id: "ws-x".into(),
                name: "unrelated".into(),
            },
            Workspace {
                id: "ws-target".into(),
                name: "PROJ-1 · Fix navbar bug".into(),
            },
        ]);

        let closed = close_workspace_for(&mut client, "PROJ-1").unwrap();
        assert!(closed);
        let closed_call = client
            .calls()
            .into_iter()
            .find_map(|c| match c {
                Call::CloseWorkspace { id } => Some(id),
                _ => None,
            })
            .unwrap();
        assert_eq!(closed_call, "ws-target");
    }

    // ---- in-place adoption -------------------------------------------------

    use crate::cmux::client::SurfaceInfo;
    use std::sync::Mutex;

    /// Tests that mutate `CMUX_WORKSPACE_ID` need to be serialised so they
    /// don't trample each other's env-var setup when cargo runs tests in
    /// parallel. The actual env-var read/write stays `unsafe` (it's
    /// process-wide state) but this guard ensures no two tests in this
    /// module are racing over the same key simultaneously.
    static ENV_VAR_GUARD: Mutex<()> = Mutex::new(());

    /// Acquire the env-var guard, set `CMUX_WORKSPACE_ID = value`, run
    /// `body`, then restore the prior value. The guard is held for the
    /// entire critical section. Returns whatever `body` returns.
    fn with_workspace_env_var<R>(value: &str, body: impl FnOnce() -> R) -> R {
        let _lock = ENV_VAR_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var_os(super::super::WORKSPACE_ENV_VAR);
        unsafe {
            std::env::set_var(super::super::WORKSPACE_ENV_VAR, value);
        }
        let result = body();
        unsafe {
            match previous {
                Some(v) => std::env::set_var(super::super::WORKSPACE_ENV_VAR, v),
                None => std::env::remove_var(super::super::WORKSPACE_ENV_VAR),
            }
        }
        result
    }

    fn simple_surface(id: &str) -> SurfaceInfo {
        SurfaceInfo {
            surface_ref: SurfaceRef {
                surface_id: format!("surface-{id}"),
                pane_id: format!("pane-{id}"),
            },
            focused: true,
        }
    }

    fn adopt_options() -> BuildOptions {
        BuildOptions {
            replace_simple_workspace: true,
            force_new_workspace: false,
        }
    }

    #[test]
    fn simple_workspace_surface_accepts_single_surface() {
        let surfaces = vec![simple_surface("a")];
        assert!(simple_workspace_surface(&surfaces).is_some());
    }

    #[test]
    fn simple_workspace_surface_rejects_multiple_surfaces_same_pane() {
        let surfaces = vec![
            SurfaceInfo {
                surface_ref: SurfaceRef {
                    surface_id: "surface-a".into(),
                    pane_id: "pane-shared".into(),
                },
                focused: true,
            },
            SurfaceInfo {
                surface_ref: SurfaceRef {
                    surface_id: "surface-b".into(),
                    pane_id: "pane-shared".into(),
                },
                focused: false,
            },
        ];
        assert!(simple_workspace_surface(&surfaces).is_none());
    }

    #[test]
    fn simple_workspace_surface_rejects_multiple_panes() {
        let surfaces = vec![simple_surface("a"), simple_surface("b")];
        assert!(simple_workspace_surface(&surfaces).is_none());
    }

    #[test]
    fn simple_workspace_surface_rejects_empty() {
        assert!(simple_workspace_surface(&[]).is_none());
    }

    #[test]
    fn try_adopt_workspace_adopts_simple_current() {
        let workspace = Workspace {
            id: "ws-current".into(),
            name: "scratch".into(),
        };
        let mut client = RecordingClient::new()
            .with_existing(vec![workspace.clone()])
            .with_workspace_surfaces("ws-current", vec![simple_surface("a")]);

        let contribs = vec![contributor("fe", "/csw/tasks/alice-PROJ-1/fe", None)];
        let spec = attached_spec("/csw/tasks/alice-PROJ-1", Some("claude"));
        let outcome = try_adopt_workspace(
            &mut client,
            std::slice::from_ref(&workspace),
            "ws-current",
            "PROJ-1",
            "PROJ-1",
            &contribs,
            &spec,
        )
        .unwrap();

        assert!(matches!(
            outcome,
            Some(BuildOutcome::Adopted { ref workspace_id, ref name })
                if workspace_id == "ws-current" && name == "PROJ-1"
        ));

        let calls = client.calls();
        assert!(calls.iter().any(|c| matches!(
            c,
            Call::RenameWorkspace { id, title } if id == "ws-current" && title == "PROJ-1"
        )));
        assert!(calls.iter().any(|c| matches!(
            c,
            Call::SetColor { id, .. } if id == "ws-current"
        )));
        assert!(!calls.iter().any(|c| matches!(c, Call::NewWorkspace { .. })));
    }

    #[test]
    fn try_adopt_workspace_returns_none_when_current_id_unknown() {
        let workspace = Workspace {
            id: "ws-current".into(),
            name: "scratch".into(),
        };
        let mut client = RecordingClient::new()
            .with_existing(vec![workspace.clone()])
            .with_workspace_surfaces("ws-current", vec![simple_surface("a")]);
        let contribs = vec![contributor("fe", "/csw/tasks/alice-PROJ-1/fe", None)];
        let spec = attached_spec("/csw/tasks/alice-PROJ-1", Some("claude"));

        let outcome = try_adopt_workspace(
            &mut client,
            std::slice::from_ref(&workspace),
            "ws-ghost",
            "PROJ-1",
            "PROJ-1",
            &contribs,
            &spec,
        )
        .unwrap();
        assert!(outcome.is_none());
        assert!(
            !client
                .calls()
                .iter()
                .any(|c| matches!(c, Call::RenameWorkspace { .. } | Call::SetColor { .. }))
        );
    }

    #[test]
    fn try_adopt_workspace_returns_none_when_not_simple() {
        let workspace = Workspace {
            id: "ws-current".into(),
            name: "scratch".into(),
        };
        let mut client = RecordingClient::new()
            .with_existing(vec![workspace.clone()])
            .with_workspace_surfaces("ws-current", vec![simple_surface("a"), simple_surface("b")]);
        let contribs = vec![contributor("fe", "/csw/tasks/alice-PROJ-1/fe", None)];
        let spec = attached_spec("/csw/tasks/alice-PROJ-1", Some("claude"));

        let outcome = try_adopt_workspace(
            &mut client,
            std::slice::from_ref(&workspace),
            "ws-current",
            "PROJ-1",
            "PROJ-1",
            &contribs,
            &spec,
        )
        .unwrap();
        assert!(outcome.is_none());
        assert!(
            !client
                .calls()
                .iter()
                .any(|c| matches!(c, Call::RenameWorkspace { .. }))
        );
    }

    #[test]
    fn build_workspace_skips_adoption_when_option_disabled() {
        let workspace = Workspace {
            id: "ws-current".into(),
            name: "scratch".into(),
        };

        let outcome = with_workspace_env_var("ws-current", || {
            let mut client = RecordingClient::new()
                .with_existing(vec![workspace.clone()])
                .with_workspace_surfaces("ws-current", vec![simple_surface("a")]);
            let contribs = vec![contributor("fe", "/csw/tasks/alice-PROJ-1/fe", None)];
            let spec = attached_spec("/csw/tasks/alice-PROJ-1", Some("claude"));
            let outcome = build_workspace(
                &mut client,
                "PROJ-1",
                None,
                &contribs,
                &spec,
                BuildOptions {
                    replace_simple_workspace: false,
                    force_new_workspace: false,
                },
            )
            .unwrap();
            (outcome, client.calls())
        });

        assert!(matches!(outcome.0, BuildOutcome::Created { .. }));
        assert!(
            outcome
                .1
                .iter()
                .any(|c| matches!(c, Call::NewWorkspace { .. }))
        );
    }

    #[test]
    fn build_workspace_skips_adoption_when_force_new() {
        let workspace = Workspace {
            id: "ws-current".into(),
            name: "scratch".into(),
        };

        let outcome = with_workspace_env_var("ws-current", || {
            let mut client = RecordingClient::new()
                .with_existing(vec![workspace.clone()])
                .with_workspace_surfaces("ws-current", vec![simple_surface("a")]);
            let contribs = vec![contributor("fe", "/csw/tasks/alice-PROJ-1/fe", None)];
            let spec = attached_spec("/csw/tasks/alice-PROJ-1", Some("claude"));
            let outcome = build_workspace(
                &mut client,
                "PROJ-1",
                None,
                &contribs,
                &spec,
                BuildOptions {
                    replace_simple_workspace: true,
                    force_new_workspace: true,
                },
            )
            .unwrap();
            (outcome, client.calls())
        });

        assert!(matches!(outcome.0, BuildOutcome::Created { .. }));
        assert!(
            outcome
                .1
                .iter()
                .any(|c| matches!(c, Call::NewWorkspace { .. }))
        );
    }

    #[test]
    fn build_workspace_adopts_simple_current_via_env_var() {
        let workspace = Workspace {
            id: "ws-current".into(),
            name: "scratch".into(),
        };

        let outcome = with_workspace_env_var("ws-current", || {
            let mut client = RecordingClient::new()
                .with_existing(vec![workspace.clone()])
                .with_workspace_surfaces("ws-current", vec![simple_surface("a")]);
            let contribs = vec![contributor("fe", "/csw/tasks/alice-PROJ-1/fe", None)];
            let spec = attached_spec("/csw/tasks/alice-PROJ-1", Some("claude"));
            let outcome =
                build_workspace(&mut client, "PROJ-1", None, &contribs, &spec, adopt_options())
                    .unwrap();
            (outcome, client.calls())
        });

        assert!(matches!(
            outcome.0,
            BuildOutcome::Adopted { ref name, .. } if name == "PROJ-1"
        ));
        assert!(
            !outcome
                .1
                .iter()
                .any(|c| matches!(c, Call::NewWorkspace { .. }))
        );
    }

    #[test]
    fn build_workspace_reuse_by_name_wins_over_adoption() {
        let existing_match = Workspace {
            id: "ws-existing".into(),
            name: "PROJ-1".into(),
        };
        let current = Workspace {
            id: "ws-current".into(),
            name: "scratch".into(),
        };

        let outcome = with_workspace_env_var("ws-current", || {
            let mut client = RecordingClient::new()
                .with_existing(vec![existing_match, current])
                .with_workspace_surfaces("ws-current", vec![simple_surface("a")]);
            let contribs = vec![contributor("fe", "/csw/tasks/alice-PROJ-1/fe", None)];
            let spec = attached_spec("/csw/tasks/alice-PROJ-1", Some("claude"));
            let outcome =
                build_workspace(&mut client, "PROJ-1", None, &contribs, &spec, adopt_options())
                    .unwrap();
            (outcome, client.calls())
        });

        assert!(matches!(
            outcome.0,
            BuildOutcome::Reused { ref workspace_id, .. } if workspace_id == "ws-existing"
        ));
        assert!(!outcome.1.iter().any(|c| matches!(
            c,
            Call::RenameWorkspace { id, .. } if id == "ws-current"
        )));
    }
}
