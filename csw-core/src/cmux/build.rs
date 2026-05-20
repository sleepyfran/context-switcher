//! Workspace build algorithm — turns a list of repo contributors into a CMux
//! workspace with one pane per repo, laid out left-to-right, with each
//! repo's per-repo `panes` config controlling the splits and tabs inside
//! its slot.
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

use super::client::{CmuxClient, CmuxError, SurfaceRef, shell_quote};
use super::config::{RepoCmuxConfig, SplitDirection};
use std::path::{Path, PathBuf};

/// CMux internally clamps every divider position to this range; matching the
/// bound here keeps our iteration from spinning against the clamp.
const RESIZE_RATIO_MIN: f32 = 0.1;
const RESIZE_RATIO_MAX: f32 = 0.9;

/// First-pass guess at the split's axis size, used to compute the initial
/// `amount` in pixels. CMux's response is self-describing (it reports the
/// old/new divider positions), so even a wrong guess converges to within
/// fractions of a percent on the corrective second call.
const RESIZE_INITIAL_AXIS_PIXELS_GUESS: f32 = 1200.0;

/// Stop iterating once the achieved ratio is within this much of the target.
const RESIZE_TOLERANCE: f32 = 0.005;

/// Up to two `pane.resize` calls per split: one with a guess, one corrective
/// using the real axis size derived from the first response.
const RESIZE_MAX_ATTEMPTS: usize = 2;

/// One repo's contribution to the workspace.
#[derive(Debug, Clone)]
pub struct Contributor {
    pub repo: String,
    pub worktree_path: PathBuf,
    pub layout: RepoCmuxConfig,
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
    /// No contributors → no workspace touched.
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

/// Build (or reuse) the workspace for this task.
pub fn build_workspace(
    client: &mut dyn CmuxClient,
    task_id: &str,
    title: Option<&str>,
    contributors: &[Contributor],
    options: BuildOptions,
) -> Result<BuildOutcome, CmuxError> {
    if contributors.is_empty() {
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
            if let Some(adopted) =
                try_adopt_workspace(client, &existing, &current_id, task_id, &name, contributors)?
            {
                return Ok(adopted);
            }
        }
    }

    let created = client.new_workspace(&name)?;
    // Apply the accent color. Failure here is the same soft failure as any
    // other CMux call — surfaced via the caller's `CmuxOutcome`.
    client.set_workspace_color(&created.workspace_id, color_for_task(task_id))?;

    populate_workspace(client, &created.initial_surface, contributors)?;
    client.select_workspace(&created.workspace_id)?;

    Ok(BuildOutcome::Created {
        workspace_id: created.workspace_id,
        name,
    })
}

/// Lay out one slot per contributor, anchored at `seed_surface`. The seed
/// becomes the first repo's slot; subsequent repos split off the previous
/// slot to the right. Within each slot the repo's own `panes` layout fans
/// out via [`build_repo_in_slot`]. After population the first surface is
/// focused so a user naturally lands there.
fn populate_workspace(
    client: &mut dyn CmuxClient,
    seed_surface: &SurfaceRef,
    contributors: &[Contributor],
) -> Result<(), CmuxError> {
    let mut slots: Vec<SurfaceRef> = Vec::with_capacity(contributors.len());
    slots.push(seed_surface.clone());
    for prev_idx in 0..contributors.len().saturating_sub(1) {
        let anchor = slots[prev_idx].clone();
        client.focus_surface(&anchor.surface_id)?;
        let new_slot = client.new_split(&anchor.surface_id, SplitDirection::Right)?;
        slots.push(new_slot);
    }

    for (slot, contributor) in slots.iter().zip(contributors) {
        build_repo_in_slot(client, slot, contributor)?;
    }

    client.focus_surface(&slots[0].surface_id)?;
    Ok(())
}

/// Attempt to reshape the given workspace in place. Returns
/// `Ok(Some(Adopted))` on success, `Ok(None)` when the workspace doesn't
/// exist on the CMux side or isn't simple enough to adopt, or `Err(...)`
/// if a CMux call genuinely failed.
fn try_adopt_workspace(
    client: &mut dyn CmuxClient,
    existing: &[super::client::Workspace],
    current_id: &str,
    task_id: &str,
    name: &str,
    contributors: &[Contributor],
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
    populate_workspace(client, &seed, contributors)?;
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

fn build_repo_in_slot(
    client: &mut dyn CmuxClient,
    slot: &SurfaceRef,
    contributor: &Contributor,
) -> Result<(), CmuxError> {
    let panes = &contributor.layout.panes;
    let Some(first) = panes.first() else {
        return Ok(());
    };

    // First pane: the slot's initial surface.
    client.focus_surface(&slot.surface_id)?;
    let cmd = pane_command(&contributor.worktree_path, first.cmd.as_deref());
    client.send(&slot.surface_id, &cmd)?;
    for tab in &first.tabs {
        let tab_ref = client.new_surface(&slot.pane_id)?;
        let tab_cmd = pane_command(&contributor.worktree_path, tab.cmd.as_deref());
        client.send(&tab_ref.surface_id, &tab_cmd)?;
    }
    // Bring the foreground tab back into focus after stacking tabs behind it.
    if !first.tabs.is_empty() {
        client.focus_surface(&slot.surface_id)?;
    }

    // Subsequent panes — each splits off the previous one.
    let mut anchor: SurfaceRef = slot.clone();
    for pane in &panes[1..] {
        let direction = pane.split.unwrap_or(SplitDirection::Right);
        client.focus_surface(&anchor.surface_id)?;
        let new_pane = client.new_split(&anchor.surface_id, direction)?;
        // Resize is best-effort: if the CMux build doesn't support
        // `pane.resize` (older builds) or the call fails for any reason,
        // we keep the default 50/50 split and continue laying out the rest
        // of the workspace.
        if let Some(target) = pane.size {
            let _ = apply_pane_size(client, &anchor, &new_pane, direction, target);
        }
        let cmd = pane_command(&contributor.worktree_path, pane.cmd.as_deref());
        client.send(&new_pane.surface_id, &cmd)?;
        for tab in &pane.tabs {
            let tab_ref = client.new_surface(&new_pane.pane_id)?;
            let tab_cmd = pane_command(&contributor.worktree_path, tab.cmd.as_deref());
            client.send(&tab_ref.surface_id, &tab_cmd)?;
        }
        if !pane.tabs.is_empty() {
            client.focus_surface(&new_pane.surface_id)?;
        }
        anchor = new_pane;
    }

    Ok(())
}

/// Adjust the divider of a freshly created split so the new pane occupies
/// roughly `target` of the split axis.
///
/// CMux's `pane.resize` is a *delta in pixels* against the split's axis
/// size; it doesn't expose a "set divider to X" primitive. We do up to two
/// passes:
///
/// 1. First call: guess axis pixels. Compute `amount` from the requested
///    ratio delta and send the resize.
/// 2. Read the response's old/new divider positions. If the divider didn't
///    move at all (clamped or method unsupported) or we're already close
///    enough, stop. Otherwise derive the real axis pixel size from the
///    observed delta and send a corrective resize.
///
/// Which pane and direction to nudge depends on whether we're growing or
/// shrinking the new pane: CMux only accepts `amount > 0` and requires the
/// target pane to actually have an edge on the requested side.
fn apply_pane_size(
    client: &mut dyn CmuxClient,
    anchor: &SurfaceRef,
    new_pane: &SurfaceRef,
    split: SplitDirection,
    target: f32,
) -> Result<(), CmuxError> {
    let target = target.clamp(RESIZE_RATIO_MIN, RESIZE_RATIO_MAX);
    let mut current_new_ratio: f32 = 0.5;
    let mut axis_pixels = RESIZE_INITIAL_AXIS_PIXELS_GUESS;

    for _ in 0..RESIZE_MAX_ATTEMPTS {
        let delta = target - current_new_ratio;
        if delta.abs() < RESIZE_TOLERANCE {
            return Ok(());
        }

        // To grow the new pane we move the divider away from it, which
        // means resizing the new pane in the direction opposite to the
        // split. To shrink the new pane we resize the anchor (original
        // pane) in the split direction, since `amount` must stay positive.
        let (target_pane, direction) = if delta > 0.0 {
            (new_pane, opposite(split))
        } else {
            (anchor, split)
        };

        let amount = (delta.abs() * axis_pixels).round() as i32;
        if amount <= 0 {
            return Ok(());
        }

        let outcome = client.resize_pane(&target_pane.pane_id, direction, amount)?;
        let observed = (outcome.new_divider_position - outcome.old_divider_position).abs();

        // If the divider didn't budge, we're either against the [0.1, 0.9]
        // clamp or the call was a no-op; either way more iteration won't
        // help.
        if observed < f32::EPSILON {
            return Ok(());
        }

        // Refine axis-pixel size from the actual observed delta so the next
        // attempt is accurate to within rounding.
        axis_pixels = amount as f32 / observed;

        // For right/down splits the new pane is the second child of the
        // split, so its ratio is `1 - divider`. For left/up the new pane is
        // the first child, so its ratio equals the divider position.
        current_new_ratio = match split {
            SplitDirection::Right | SplitDirection::Down => 1.0 - outcome.new_divider_position,
            SplitDirection::Left | SplitDirection::Up => outcome.new_divider_position,
        };
    }

    Ok(())
}

fn opposite(d: SplitDirection) -> SplitDirection {
    match d {
        SplitDirection::Left => SplitDirection::Right,
        SplitDirection::Right => SplitDirection::Left,
        SplitDirection::Up => SplitDirection::Down,
        SplitDirection::Down => SplitDirection::Up,
    }
}

/// Build the shell snippet sent to a pane. Leading space exploits
/// `HISTCONTROL=ignorespace` to keep the cd line out of shell history; the
/// cd command itself uses single-quoted shell-quoting so paths with spaces
/// or special characters survive intact. A `None` command produces just the
/// cd — the pane lands on a shell prompt rooted in the worktree, nothing
/// autostarting.
fn pane_command(worktree: &Path, cmd: Option<&str>) -> String {
    let path = worktree.to_string_lossy();
    let quoted = shell_quote(&path);
    match cmd {
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
    use crate::cmux::config::{PaneSpec, TabSpec};
    use std::path::PathBuf;

    fn contributor(name: &str, worktree: &str, panes: Vec<PaneSpec>) -> Contributor {
        Contributor {
            repo: name.into(),
            worktree_path: PathBuf::from(worktree),
            layout: RepoCmuxConfig { panes },
        }
    }

    fn pane(cmd: &str) -> PaneSpec {
        PaneSpec {
            cmd: Some(cmd.into()),
            split: None,
            size: None,
            tabs: Vec::new(),
        }
    }

    fn pane_split(cmd: &str, dir: SplitDirection) -> PaneSpec {
        PaneSpec {
            cmd: Some(cmd.into()),
            split: Some(dir),
            size: None,
            tabs: Vec::new(),
        }
    }

    fn pane_split_size(cmd: &str, dir: SplitDirection, size: f32) -> PaneSpec {
        PaneSpec {
            cmd: Some(cmd.into()),
            split: Some(dir),
            size: Some(size),
            tabs: Vec::new(),
        }
    }

    #[test]
    fn no_contributors_short_circuits_without_calls() {
        let mut client = RecordingClient::new();
        let outcome =
            build_workspace(&mut client, "PROJ-1", None, &[], BuildOptions::default()).unwrap();
        assert_eq!(outcome, BuildOutcome::NoContributors);
        assert!(client.calls().is_empty());
    }

    #[test]
    fn existing_workspace_is_reused_no_splits() {
        let mut client = RecordingClient::new().with_existing(vec![Workspace {
            id: "ws-existing".into(),
            name: "PROJ-1".into(),
        }]);
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![pane("ls")],
        )];

        let outcome = build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
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
        // The workspace was created on a prior run with a title; the resume
        // call doesn't supply one. We should still find and focus it via
        // the task-id prefix rule, not create a duplicate.
        let mut client = RecordingClient::new().with_existing(vec![Workspace {
            id: "ws-with-title".into(),
            name: "PROJ-1 · Fix navbar bug".into(),
        }]);
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![pane("ls")],
        )];

        let outcome = build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
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
        // Must start with #; all palette entries are 7 chars #RRGGBB.
        assert!(a.starts_with('#') && a.len() == 7, "got {a}");
    }

    #[test]
    fn workspace_create_applies_deterministic_color() {
        let mut client = RecordingClient::new();
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![pane("ls")],
        )];

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            BuildOptions::default(),
        )
        .unwrap();

        let color_call = client.calls().into_iter().find_map(|c| match c {
            Call::SetColor { color, .. } => Some(color),
            _ => None,
        });
        assert_eq!(color_call.as_deref(), Some(color_for_task("PROJ-1")));
    }

    #[test]
    fn workspace_create_uses_title_when_provided() {
        let mut client = RecordingClient::new();
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![pane("ls")],
        )];

        build_workspace(
            &mut client,
            "PROJ-1",
            Some("Fix navbar bug"),
            &contribs,
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
    fn single_repo_single_pane_sends_one_command() {
        let mut client = RecordingClient::new();
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![pane("pnpm dev")],
        )];

        let outcome = build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            BuildOptions::default(),
        )
        .unwrap();
        assert!(matches!(outcome, BuildOutcome::Created { .. }));

        let calls = client.calls();
        // Expect: list, new_workspace, set_color, focus(initial),
        // send(initial,...), focus(initial) [final], select_workspace.
        assert!(matches!(calls[0], Call::ListWorkspaces));
        assert!(matches!(calls[1], Call::NewWorkspace { ref title } if title == "PROJ-1"));
        assert!(matches!(calls[2], Call::SetColor { .. }));
        let send = calls
            .iter()
            .find_map(|c| match c {
                Call::Send { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(send.starts_with(" cd "));
        assert!(send.contains("/csw/tasks/fe/alice-PROJ-1"));
        assert!(send.ends_with("pnpm dev\n"));
    }

    #[test]
    fn two_repos_create_left_to_right_slots() {
        let mut client = RecordingClient::new();
        let contribs = vec![
            contributor("fe", "/csw/tasks/fe/alice-PROJ-1", vec![pane("pnpm dev")]),
            contributor("be", "/csw/tasks/be/alice-PROJ-1", vec![pane("pnpm dev")]),
        ];

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            BuildOptions::default(),
        )
        .unwrap();

        // The first inter-repo split should be a right-split off the first
        // (initial) slot.
        let calls = client.calls();
        let first_split = calls
            .iter()
            .find_map(|c| match c {
                Call::NewSplit { anchor, direction } => Some((anchor.clone(), *direction)),
                _ => None,
            })
            .unwrap();
        assert_eq!(first_split.1, SplitDirection::Right);
        // Anchor was the initial surface.
        assert!(first_split.0.starts_with("surface-"));
    }

    #[test]
    fn pane_with_split_right_creates_split_to_anchor() {
        let mut client = RecordingClient::new();
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![
                pane("pnpm dev"),
                pane_split("claude", SplitDirection::Right),
            ],
        )];

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            BuildOptions::default(),
        )
        .unwrap();

        let splits: Vec<_> = client
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::NewSplit { anchor, direction } => Some((anchor, direction)),
                _ => None,
            })
            .collect();
        // Only one split since it's a single-repo task with two panes.
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].1, SplitDirection::Right);
    }

    #[test]
    fn pane_without_size_does_not_call_resize() {
        let mut client = RecordingClient::new();
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![
                pane("pnpm dev"),
                pane_split("claude", SplitDirection::Right),
            ],
        )];

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            BuildOptions::default(),
        )
        .unwrap();

        assert!(
            !client
                .calls()
                .iter()
                .any(|c| matches!(c, Call::ResizePane { .. }))
        );
    }

    #[test]
    fn pane_with_size_emits_resize_on_new_pane() {
        // simulated_axis_pixels = 1600; target 0.7 → need to grow new pane
        // from 0.5 to 0.7 → 0.2 of axis → 320 px first call. Initial guess
        // of 1200 → first amount = 240. After first call the helper derives
        // the real axis (1600) and corrects with a second call.
        let mut client = RecordingClient::new().with_simulated_axis_pixels(1600.0);
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![
                pane("pnpm dev"),
                pane_split_size("claude", SplitDirection::Right, 0.7),
            ],
        )];

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            BuildOptions::default(),
        )
        .unwrap();

        let resizes: Vec<_> = client
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::ResizePane {
                    pane,
                    direction,
                    amount,
                } => Some((pane, direction, amount)),
                _ => None,
            })
            .collect();

        assert!(!resizes.is_empty(), "expected at least one ResizePane call");
        // Growing the right-pane means resizing it leftward.
        assert!(
            resizes
                .iter()
                .all(|(_, dir, _)| *dir == SplitDirection::Left)
        );
        // All resize calls target the new pane (the split's second child).
        let first_pane = &resizes[0].0;
        assert!(resizes.iter().all(|(p, _, _)| p == first_pane));
        // First call uses the initial guess (1200 * 0.2 = 240).
        assert_eq!(resizes[0].2, 240);
        // Second corrective call should be small (axis derived to 1600,
        // remaining gap 0.2 - 0.15 = 0.05, → 0.05 * 1600 = 80).
        if resizes.len() > 1 {
            assert!(
                resizes[1].2 > 0 && resizes[1].2 < 240,
                "got {}",
                resizes[1].2
            );
        }
    }

    #[test]
    fn pane_size_below_half_resizes_anchor_in_split_direction() {
        // Target < 0.5 → shrink new pane → resize anchor in split direction.
        let mut client = RecordingClient::new().with_simulated_axis_pixels(1600.0);
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![
                pane("pnpm dev"),
                pane_split_size("claude", SplitDirection::Right, 0.3),
            ],
        )];

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            BuildOptions::default(),
        )
        .unwrap();

        let resize_directions: Vec<SplitDirection> = client
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::ResizePane { direction, .. } => Some(direction),
                _ => None,
            })
            .collect();

        assert!(!resize_directions.is_empty());
        // Shrinking the right new pane → resize the anchor (left, original
        // pane) in the split direction ("right" pushes the divider right,
        // which makes the new pane narrower).
        assert!(
            resize_directions
                .iter()
                .all(|d| *d == SplitDirection::Right)
        );
    }

    #[test]
    fn pane_size_with_down_split_targets_vertical_axis() {
        // Sanity-check the vertical case: split = "down", target 0.7 (new
        // bottom pane gets 70% height). Growth direction = "up".
        let mut client = RecordingClient::new().with_simulated_axis_pixels(1600.0);
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![
                pane("pnpm dev"),
                pane_split_size("claude", SplitDirection::Down, 0.7),
            ],
        )];

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            BuildOptions::default(),
        )
        .unwrap();

        let resize_directions: Vec<SplitDirection> = client
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::ResizePane { direction, .. } => Some(direction),
                _ => None,
            })
            .collect();

        assert!(!resize_directions.is_empty());
        assert!(resize_directions.iter().all(|d| *d == SplitDirection::Up));
    }

    #[test]
    fn pane_size_out_of_range_is_clamped() {
        // Asking for 0.99 should clamp to 0.9 internally; we still get a
        // resize call (the new pane needs to grow from 0.5).
        let mut client = RecordingClient::new().with_simulated_axis_pixels(1600.0);
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![
                pane("pnpm dev"),
                pane_split_size("claude", SplitDirection::Right, 0.99),
            ],
        )];

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            BuildOptions::default(),
        )
        .unwrap();

        let max_amount = client
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::ResizePane { amount, .. } => Some(amount),
                _ => None,
            })
            .max()
            .expect("expected a resize call");
        // 0.9 - 0.5 = 0.4 of axis. First-call guess = 0.4 * 1200 = 480.
        // We must not have produced an amount that targets 0.99 (which
        // would be 0.49 * something).
        assert!(max_amount <= 480, "got {max_amount}");
    }

    #[test]
    fn pane_size_converges_to_target_after_two_attempts() {
        // Direct test of the iterative helper: given a known simulated axis
        // pixel size that disagrees with our initial guess, the helper
        // should still land within RESIZE_TOLERANCE of the target on the
        // second pass.
        let mut client = RecordingClient::new().with_simulated_axis_pixels(1600.0);
        let anchor = SurfaceRef {
            surface_id: "surface-anchor".into(),
            pane_id: "pane-anchor".into(),
        };
        let new_pane = SurfaceRef {
            surface_id: "surface-new".into(),
            pane_id: "pane-new".into(),
        };

        apply_pane_size(&mut client, &anchor, &new_pane, SplitDirection::Right, 0.7).unwrap();

        // The mock tracks divider state per pane. After both calls, the
        // tracked divider for the new pane should produce a new-pane ratio
        // very close to 0.7. New pane is the second child for a right
        // split, so ratio = 1 - divider.
        let divider = client.divider_for(&new_pane.pane_id).unwrap_or(0.5);
        let achieved = 1.0 - divider;
        assert!(
            (achieved - 0.7).abs() < 0.01,
            "expected ~0.7, got {achieved} (divider={divider})"
        );
    }

    #[test]
    fn pane_size_target_near_50_50_makes_no_call() {
        // Target within tolerance of 0.5 → no resize needed (we open at
        // 0.5 already).
        let mut client = RecordingClient::new();
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![
                pane("pnpm dev"),
                pane_split_size("claude", SplitDirection::Right, 0.5),
            ],
        )];

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            BuildOptions::default(),
        )
        .unwrap();
        assert!(
            !client
                .calls()
                .iter()
                .any(|c| matches!(c, Call::ResizePane { .. }))
        );
    }

    #[test]
    fn pane_with_tabs_creates_new_surface_per_tab() {
        let mut client = RecordingClient::new();
        let mut pane = pane("pnpm dev");
        pane.tabs = vec![
            TabSpec {
                cmd: Some("sh".into()),
            },
            TabSpec {
                cmd: Some("less log".into()),
            },
        ];
        let contribs = vec![contributor("fe", "/csw/tasks/fe/alice-PROJ-1", vec![pane])];

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            BuildOptions::default(),
        )
        .unwrap();

        let new_surfaces: Vec<_> = client
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                Call::NewSurface { pane } => Some(pane),
                _ => None,
            })
            .collect();
        assert_eq!(new_surfaces.len(), 2, "expected 2 new surfaces for 2 tabs");
    }

    #[test]
    fn pane_command_uses_leading_space_and_shell_quoting() {
        let s = pane_command(Path::new("/path with spaces"), Some("cmd"));
        assert!(s.starts_with(" cd '"), "leading space + single quote: {s}");
        assert!(s.contains("/path with spaces"));
        assert!(s.ends_with("cmd\n"));
    }

    #[test]
    fn pane_command_without_cmd_just_cds() {
        let s = pane_command(Path::new("/csw/tasks/fe/alice-PROJ-1"), None);
        assert_eq!(s, " cd '/csw/tasks/fe/alice-PROJ-1'\n");
    }

    #[test]
    fn pane_without_cmd_still_sends_cd() {
        let mut client = RecordingClient::new();
        let bare_pane = PaneSpec {
            cmd: None,
            split: None,
            size: None,
            tabs: Vec::new(),
        };
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![bare_pane],
        )];

        build_workspace(
            &mut client,
            "PROJ-1",
            None,
            &contribs,
            BuildOptions::default(),
        )
        .unwrap();

        let send = client
            .calls()
            .into_iter()
            .find_map(|c| match c {
                Call::Send { text, .. } => Some(text),
                _ => None,
            })
            .unwrap();
        // No `&&` — just the cd, no command piggy-backing on it.
        assert!(send.starts_with(" cd '"), "got: {send}");
        assert!(!send.contains("&&"), "got: {send}");
        assert!(send.ends_with("'\n"), "got: {send}");
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
        // Two surfaces in one pane → multi-tab → not simple.
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

        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![pane("ls")],
        )];
        let outcome = try_adopt_workspace(
            &mut client,
            std::slice::from_ref(&workspace),
            "ws-current",
            "PROJ-1",
            "PROJ-1",
            &contribs,
        )
        .unwrap();

        assert!(matches!(
            outcome,
            Some(BuildOutcome::Adopted { ref workspace_id, ref name })
                if workspace_id == "ws-current" && name == "PROJ-1"
        ));

        let calls = client.calls();
        // The adoption path renames + recolors the existing workspace and
        // never calls `workspace.create`.
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
    fn try_adopt_workspace_uses_per_task_color_on_rename() {
        let workspace = Workspace {
            id: "ws-current".into(),
            name: "scratch".into(),
        };
        let mut client = RecordingClient::new()
            .with_existing(vec![workspace.clone()])
            .with_workspace_surfaces("ws-current", vec![simple_surface("a")]);
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![pane("ls")],
        )];

        try_adopt_workspace(
            &mut client,
            std::slice::from_ref(&workspace),
            "ws-current",
            "PROJ-1",
            "PROJ-1",
            &contribs,
        )
        .unwrap();

        let color = client.calls().into_iter().find_map(|c| match c {
            Call::SetColor { color, .. } => Some(color),
            _ => None,
        });
        assert_eq!(color.as_deref(), Some(color_for_task("PROJ-1")));
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
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![pane("ls")],
        )];

        let outcome = try_adopt_workspace(
            &mut client,
            std::slice::from_ref(&workspace),
            "ws-ghost",
            "PROJ-1",
            "PROJ-1",
            &contribs,
        )
        .unwrap();
        assert!(outcome.is_none());
        // None of the mutation calls should have fired.
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
            // Two surfaces → not simple.
            .with_workspace_surfaces("ws-current", vec![simple_surface("a"), simple_surface("b")]);
        let contribs = vec![contributor(
            "fe",
            "/csw/tasks/fe/alice-PROJ-1",
            vec![pane("ls")],
        )];

        let outcome = try_adopt_workspace(
            &mut client,
            std::slice::from_ref(&workspace),
            "ws-current",
            "PROJ-1",
            "PROJ-1",
            &contribs,
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
        // Even with a simple current workspace, replace_simple_workspace=false
        // forces the create path.
        let workspace = Workspace {
            id: "ws-current".into(),
            name: "scratch".into(),
        };

        let outcome = with_workspace_env_var("ws-current", || {
            let mut client = RecordingClient::new()
                .with_existing(vec![workspace.clone()])
                .with_workspace_surfaces("ws-current", vec![simple_surface("a")]);
            let contribs = vec![contributor(
                "fe",
                "/csw/tasks/fe/alice-PROJ-1",
                vec![pane("ls")],
            )];
            let outcome = build_workspace(
                &mut client,
                "PROJ-1",
                None,
                &contribs,
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
            let contribs = vec![contributor(
                "fe",
                "/csw/tasks/fe/alice-PROJ-1",
                vec![pane("ls")],
            )];
            let outcome = build_workspace(
                &mut client,
                "PROJ-1",
                None,
                &contribs,
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
            let contribs = vec![contributor(
                "fe",
                "/csw/tasks/fe/alice-PROJ-1",
                vec![pane("ls")],
            )];
            let outcome =
                build_workspace(&mut client, "PROJ-1", None, &contribs, adopt_options()).unwrap();
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
        // The current workspace is simple, but a different workspace
        // already matches the task id — reuse it instead of adopting.
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
            let contribs = vec![contributor(
                "fe",
                "/csw/tasks/fe/alice-PROJ-1",
                vec![pane("ls")],
            )];
            let outcome =
                build_workspace(&mut client, "PROJ-1", None, &contribs, adopt_options()).unwrap();
            (outcome, client.calls())
        });

        assert!(matches!(
            outcome.0,
            BuildOutcome::Reused { ref workspace_id, .. } if workspace_id == "ws-existing"
        ));
        // No rename or create on the current workspace.
        assert!(!outcome.1.iter().any(|c| matches!(
            c,
            Call::RenameWorkspace { id, .. } if id == "ws-current"
        )));
    }
}
