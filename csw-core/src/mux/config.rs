//! Configuration schema for the multiplexer integration.
//!
//! [`MuxConfig`] is the top-level `[mux]` block in `config.toml` (the legacy
//! `[cmux]` table name is still accepted when reading):
//!
//! ```toml
//! [mux]
//! provider = "auto"                # auto | cmux | rex
//! enabled = true                   # global kill-switch
//! replace_simple_workspace = true  # adopt a single-pane current workspace
//! ```
//!
//! The pane layout itself is fixed — see `mux::build`.

use super::Provider;
use serde::{Deserialize, Serialize};

/// Which multiplexer csw should drive.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProviderChoice {
    /// Use whichever supported multiplexer csw is running inside (Rex is
    /// checked first, then CMux). Outside of both, the integration is a
    /// no-op.
    #[default]
    Auto,
    /// Only ever talk to CMux, and only when running inside it.
    Cmux,
    /// Only ever talk to Rex, and only when running inside it.
    Rex,
}

impl ProviderChoice {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Cmux => "cmux",
            Self::Rex => "rex",
        }
    }

    /// The providers this choice allows, in detection order.
    pub fn candidates(self) -> &'static [Provider] {
        match self {
            Self::Auto => &[Provider::Rex, Provider::Cmux],
            Self::Cmux => &[Provider::Cmux],
            Self::Rex => &[Provider::Rex],
        }
    }
}

impl std::str::FromStr for ProviderChoice {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "cmux" => Ok(Self::Cmux),
            "rex" => Ok(Self::Rex),
            other => Err(format!(
                "unknown provider '{other}' (expected: auto | cmux | rex)"
            )),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MuxConfig {
    #[serde(default)]
    pub provider: ProviderChoice,

    #[serde(default = "default_true")]
    pub enabled: bool,

    /// When `csw start` runs inside a multiplexer and the current workspace
    /// is "simple" (exactly one pane, one tab), reshape that workspace in
    /// place rather than spawning a new sidebar entry. Defaults to `true`;
    /// pass `--force-new-workspace` to override for a single invocation.
    #[serde(default = "default_true")]
    pub replace_simple_workspace: bool,
}

impl Default for MuxConfig {
    fn default() -> Self {
        Self {
            provider: ProviderChoice::Auto,
            enabled: true,
            replace_simple_workspace: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_enabled_auto_and_replacing() {
        let c = MuxConfig::default();
        assert!(c.enabled);
        assert!(c.replace_simple_workspace);
        assert_eq!(c.provider, ProviderChoice::Auto);
    }

    #[test]
    fn empty_table_deserialises_as_defaults() {
        let c: MuxConfig = toml::from_str("").unwrap();
        assert_eq!(c, MuxConfig::default());
    }

    #[test]
    fn replace_simple_workspace_can_be_disabled() {
        let c: MuxConfig = toml::from_str("replace_simple_workspace = false").unwrap();
        assert!(c.enabled);
        assert!(!c.replace_simple_workspace);
    }

    #[test]
    fn provider_parses_lowercase() {
        let c: MuxConfig = toml::from_str("provider = \"rex\"").unwrap();
        assert_eq!(c.provider, ProviderChoice::Rex);
    }

    #[test]
    fn unknown_provider_is_rejected() {
        assert!(toml::from_str::<MuxConfig>("provider = \"tmux\"").is_err());
        assert!("tmux".parse::<ProviderChoice>().is_err());
    }

    #[test]
    fn auto_prefers_rex_over_cmux() {
        assert_eq!(
            ProviderChoice::Auto.candidates(),
            &[Provider::Rex, Provider::Cmux]
        );
    }
}
