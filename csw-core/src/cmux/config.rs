//! Configuration schema for the CMux integration.
//!
//! [`GlobalCmuxConfig`] is the top-level `[cmux]` block in `config.toml`.
//! An `enabled` kill-switch (default true) and a `replace_simple_workspace`
//! knob (default true) controlling whether `csw start` reshapes the current
//! CMux workspace in place when it has only a single pane and surface,
//! instead of creating a new sidebar entry.
//!
//! The pane layout itself (which repos get a pane, what runs in each, and
//! the top/bottom split) is fixed — see `cmux::build` — so there is no
//! longer any per-repo pane configuration here.

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
}
