//! Configuration schema for the CMux integration.
//!
//! Two scopes live here:
//! * [`GlobalCmuxConfig`] — the top-level `[cmux]` block in `config.toml`.
//!   An `enabled` kill-switch (default true) and a
//!   `replace_simple_workspace` knob (default true) controlling whether
//!   `csw start` reshapes the current CMux workspace in place when it has
//!   only a single pane and surface, instead of creating a new sidebar
//!   entry.
//! * [`RepoCmuxConfig`] — the per-repo `[repos.<name>.cmux]` block. Presence
//!   of this block with a non-empty `panes` list is how a repo opts in to
//!   participating in the workspace.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct GlobalCmuxConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,

    /// When `csw start` runs inside CMux and the current workspace is
    /// "simple" (exactly one pane and one surface), reshape that workspace
    /// in place rather than spawning a new sidebar entry. Defaults to
    /// `true`; pass `--force-new-workspace` on the command line to override
    /// for a single invocation.
    #[serde(default = "default_replace_simple_workspace")]
    pub replace_simple_workspace: bool,
}

impl Default for GlobalCmuxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            replace_simple_workspace: true,
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_replace_simple_workspace() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct RepoCmuxConfig {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<PaneSpec>,
}

impl RepoCmuxConfig {
    /// A repo participates iff it has at least one pane configured. An empty
    /// `panes` block (or no block at all) means the repo is not part of the
    /// workspace.
    pub fn participates(&self) -> bool {
        !self.panes.is_empty()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PaneSpec {
    /// Foreground command for the pane's initial surface. When absent, the
    /// pane is just `cd`'d into the task copy — useful for an idle shell
    /// you can type into without something autostarting (e.g. a dev server
    /// you only want to run sometimes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,

    /// Direction to split *from the previous pane in this repo* to obtain
    /// this pane. The first pane in a repo has no split — it inherits the
    /// repo's slot in the workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<SplitDirection>,

    /// Target ratio of this pane along its split axis, between 0 and 1.
    /// Only honored when `split` is set; the first pane in a repo has no
    /// split so this field is ignored there. CMux only exposes pixel-delta
    /// resizing (`pane.resize`), not absolute ratios, and clamps the
    /// divider to [0.1, 0.9] internally, so the achieved ratio is
    /// best-effort. Out-of-range values are clamped to the same window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,

    /// Additional tabs (CMux "surfaces") stacked behind the foreground `cmd`.
    /// On creation the foreground tab is focused; users flip between them
    /// with ⌘[ / ⌘].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tabs: Vec<TabSpec>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    Left,
    Right,
    Up,
    Down,
}

impl SplitDirection {
    /// CMux's socket API expects directional splits by these string tokens.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TabSpec {
    /// Command for this tab. When absent, the tab is just a shell `cd`'d
    /// into the task copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_default_is_enabled() {
        assert!(GlobalCmuxConfig::default().enabled);
    }

    #[test]
    fn global_default_replaces_simple_workspace() {
        assert!(GlobalCmuxConfig::default().replace_simple_workspace);
    }

    #[test]
    fn empty_global_table_deserialises_as_enabled() {
        let g: GlobalCmuxConfig = toml::from_str("").unwrap();
        assert!(g.enabled);
        assert!(g.replace_simple_workspace);
    }

    #[test]
    fn replace_simple_workspace_can_be_disabled() {
        let g: GlobalCmuxConfig = toml::from_str("replace_simple_workspace = false").unwrap();
        assert!(g.enabled);
        assert!(!g.replace_simple_workspace);
    }

    #[test]
    fn participates_only_when_panes_present() {
        assert!(!RepoCmuxConfig::default().participates());
        let with_pane = RepoCmuxConfig {
            panes: vec![PaneSpec {
                cmd: Some("ls".into()),
                split: None,
                size: None,
                tabs: Vec::new(),
            }],
        };
        assert!(with_pane.participates());
    }

    #[test]
    fn pane_spec_round_trips() {
        let cfg: RepoCmuxConfig = toml::from_str(
            r#"
panes = [
  { cmd = "pnpm dev" },
  { cmd = "claude", split = "right", tabs = [{ cmd = "sh" }] },
]
"#,
        )
        .unwrap();
        assert_eq!(cfg.panes.len(), 2);
        assert_eq!(cfg.panes[0].cmd.as_deref(), Some("pnpm dev"));
        assert!(cfg.panes[0].split.is_none());
        assert_eq!(cfg.panes[1].split, Some(SplitDirection::Right));
        assert_eq!(cfg.panes[1].tabs[0].cmd.as_deref(), Some("sh"));
    }

    #[test]
    fn pane_without_cmd_deserialises() {
        let cfg: RepoCmuxConfig = toml::from_str(
            r#"
panes = [
  {},
  { split = "right" },
]
"#,
        )
        .unwrap();
        assert!(cfg.panes[0].cmd.is_none());
        assert!(cfg.panes[1].cmd.is_none());
        assert_eq!(cfg.panes[1].split, Some(SplitDirection::Right));
    }

    #[test]
    fn split_direction_serialises_lowercase() {
        let s = toml::to_string(&PaneSpec {
            cmd: Some("ls".into()),
            split: Some(SplitDirection::Down),
            size: None,
            tabs: Vec::new(),
        })
        .unwrap();
        assert!(s.contains(r#"split = "down""#), "got: {s}");
    }

    #[test]
    fn pane_size_round_trips() {
        let cfg: RepoCmuxConfig = toml::from_str(
            r#"
panes = [
  { cmd = "pnpm dev" },
  { cmd = "claude", split = "right", size = 0.7 },
]
"#,
        )
        .unwrap();
        assert_eq!(cfg.panes[0].size, None);
        assert_eq!(cfg.panes[1].size, Some(0.7));

        // Round-trip through serialisation: the size field should survive.
        let s = toml::to_string(&cfg).unwrap();
        assert!(s.contains("size = 0.7"), "got: {s}");
    }

    #[test]
    fn pane_size_is_optional() {
        let cfg: RepoCmuxConfig = toml::from_str(
            r#"
panes = [
  { cmd = "pnpm dev" },
]
"#,
        )
        .unwrap();
        assert_eq!(cfg.panes[0].size, None);

        // Absent `size` should round-trip as absent, not as `size = null`.
        let s = toml::to_string(&cfg).unwrap();
        assert!(!s.contains("size"), "got: {s}");
    }
}
