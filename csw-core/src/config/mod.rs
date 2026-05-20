mod repo;

pub use repo::RepoConfig;

use crate::cmux::config::GlobalCmuxConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

const CONFIG_DIR: &str = "context-switcher";
const CONFIG_FILE: &str = "config.toml";
const TASKS_SUBDIR: &str = "tasks";

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Config {
    #[serde(default = "default_base_dir")]
    pub base_dir: PathBuf,

    /// Root directory under which `csw` materialises task worktrees, grouped
    /// by repo: `<tasks_dir>/<repo>/<user>-<task-id>`. Defaults to a `tasks/`
    /// subdirectory next to `config.toml`.
    #[serde(default = "default_tasks_dir")]
    pub tasks_dir: PathBuf,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_repos: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmux: Option<GlobalCmuxConfig>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repos: BTreeMap<String, RepoConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_dir: default_base_dir(),
            tasks_dir: default_tasks_dir(),
            username: None,
            default_repos: Vec::new(),
            cmux: None,
            repos: BTreeMap::new(),
        }
    }
}

fn default_base_dir() -> PathBuf {
    expand_home("~/Developer")
}

fn default_tasks_dir() -> PathBuf {
    config_path()
        .parent()
        .map(|p| p.join(TASKS_SUBDIR))
        .unwrap_or_else(|| PathBuf::from(TASKS_SUBDIR))
}

/// Expand a leading `~` against `$HOME`. Returns the input unchanged on failure.
pub fn expand_home(input: &str) -> PathBuf {
    let expanded = shellexpand::tilde_with_context(input, home_dir_string).into_owned();
    PathBuf::from(expanded)
}

fn home_dir_string() -> Option<String> {
    std::env::var("HOME").ok()
}

fn home_dir_path() -> Option<PathBuf> {
    home_dir_string().map(PathBuf::from)
}

/// Resolve the on-disk path for the config file, honouring `$XDG_CONFIG_HOME`.
pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir_path().map(|h| h.join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join(CONFIG_DIR).join(CONFIG_FILE)
}

impl Config {
    /// Load from the default location. Returns `Config::default()` if missing.
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path())
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("parsing config at {}", path.display()))?;
        cfg.expand_paths();
        Ok(cfg)
    }

    /// Save to the default location.
    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path())
    }

    /// Save atomically: write to a sibling temp file, fsync, rename.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }

        let serialized = toml::to_string_pretty(self).context("serialising config to TOML")?;

        let dir = path.parent().unwrap_or(Path::new("."));
        let mut tmp = tempfile::Builder::new()
            .prefix(".config-")
            .suffix(".toml.tmp")
            .tempfile_in(dir)
            .with_context(|| format!("creating temp file in {}", dir.display()))?;

        tmp.write_all(serialized.as_bytes())
            .context("writing serialised config to temp file")?;
        tmp.as_file_mut().sync_all().context("fsync temp config")?;

        tmp.persist(path)
            .map_err(|e| e.error)
            .with_context(|| format!("renaming temp config into place at {}", path.display()))?;

        Ok(())
    }

    /// Expand `~` in `base_dir`, `tasks_dir`, and per-repo `path` after
    /// deserialisation.
    fn expand_paths(&mut self) {
        if let Some(s) = self.base_dir.to_str() {
            self.base_dir = expand_home(s);
        }
        if let Some(s) = self.tasks_dir.to_str() {
            self.tasks_dir = expand_home(s);
        }
        for repo in self.repos.values_mut() {
            if let Some(s) = repo.path.to_str() {
                repo.path = expand_home(s);
            }
        }
    }

    /// Resolve a repo by name. `None` if not configured.
    pub fn repo(&self, name: &str) -> Option<&RepoConfig> {
        self.repos.get(name)
    }

    /// Compute the on-disk canonical path for a repo (absolute or `base_dir`-relative).
    pub fn canonical_path(&self, repo: &RepoConfig) -> PathBuf {
        if repo.path.is_absolute() {
            repo.path.clone()
        } else {
            self.base_dir.join(&repo.path)
        }
    }

    /// Directory under which this repo's task worktrees live, i.e.
    /// `<tasks_dir>/<repo_name>/`.
    pub fn repo_tasks_dir(&self, repo_name: &str) -> PathBuf {
        self.tasks_dir.join(repo_name)
    }

    /// Pretty TOML representation, used by `csw config show`.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serialising config to TOML")
    }

    /// Whether the CMux integration is enabled. Default is on; an absent
    /// `[cmux]` block reads as enabled. Only an explicit `enabled = false`
    /// turns it off.
    pub fn cmux_enabled(&self) -> bool {
        self.cmux.as_ref().map(|c| c.enabled).unwrap_or(true)
    }

    /// Whether `csw start` should reshape the current CMux workspace in
    /// place when it's simple (single pane, single surface) instead of
    /// creating a new sidebar entry. Defaults to on; an absent `[cmux]`
    /// block or absent `replace_simple_workspace = ...` reads as enabled.
    pub fn cmux_replace_simple_workspace(&self) -> bool {
        self.cmux
            .as_ref()
            .map(|c| c.replace_simple_workspace)
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn home_isolated_config(tmp: &TempDir) -> PathBuf {
        // Tests don't go through config_path() — they use load_from / save_to
        // with explicit paths to avoid mutating $HOME.
        tmp.path().join("config.toml")
    }

    #[test]
    fn missing_file_returns_default() {
        let tmp = TempDir::new().unwrap();
        let path = home_isolated_config(&tmp);
        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn round_trip_preserves_fields() {
        let tmp = TempDir::new().unwrap();
        let path = home_isolated_config(&tmp);

        let mut repos = BTreeMap::new();
        repos.insert(
            "frontend".into(),
            RepoConfig {
                path: PathBuf::from("/tmp/dev/frontend"),
                editor: "zed {path}".into(),
                base_branch: Some("develop".into()),
                post_create: Vec::new(),
                cmux: None,
            },
        );
        let cfg = Config {
            base_dir: PathBuf::from("/tmp/dev"),
            tasks_dir: PathBuf::from("/tmp/csw/tasks"),
            username: Some("alice".into()),
            default_repos: vec!["frontend".into()],
            cmux: None,
            repos,
        };

        cfg.save_to(&path).unwrap();
        let loaded = Config::load_from(&path).unwrap();
        assert_eq!(cfg, loaded);
    }

    #[test]
    fn home_tilde_is_expanded_on_load() {
        // SAFETY: setting HOME for the duration of this test.
        unsafe {
            std::env::set_var("HOME", "/home/test");
        }
        let tmp = TempDir::new().unwrap();
        let path = home_isolated_config(&tmp);
        std::fs::write(
            &path,
            r#"
base_dir = "~/Developer"
tasks_dir = "~/.config/context-switcher/tasks"

[repos.frontend]
path = "~/Developer/frontend"
editor = "zed {path}"
"#,
        )
        .unwrap();

        let cfg = Config::load_from(&path).unwrap();
        assert_eq!(cfg.base_dir, PathBuf::from("/home/test/Developer"));
        assert_eq!(
            cfg.tasks_dir,
            PathBuf::from("/home/test/.config/context-switcher/tasks")
        );
        assert_eq!(
            cfg.repos["frontend"].path,
            PathBuf::from("/home/test/Developer/frontend")
        );
    }

    fn cfg_with_base(base: &str) -> Config {
        Config {
            base_dir: PathBuf::from(base),
            ..Config::default()
        }
    }

    #[test]
    fn canonical_path_joins_relative_path() {
        let cfg = cfg_with_base("/x");
        let repo = RepoConfig::new("frontend", "zed {path}");
        assert_eq!(cfg.canonical_path(&repo), PathBuf::from("/x/frontend"));
    }

    #[test]
    fn canonical_path_keeps_absolute_path() {
        let cfg = cfg_with_base("/x");
        let repo = RepoConfig::new("/elsewhere/frontend", "zed {path}");
        assert_eq!(
            cfg.canonical_path(&repo),
            PathBuf::from("/elsewhere/frontend")
        );
    }

    #[test]
    fn repo_tasks_dir_joins_repo_name() {
        let cfg = Config {
            tasks_dir: PathBuf::from("/csw/tasks"),
            ..Config::default()
        };
        assert_eq!(
            cfg.repo_tasks_dir("frontend"),
            PathBuf::from("/csw/tasks/frontend")
        );
    }

    #[test]
    fn save_is_atomic_no_temp_left_behind() {
        let tmp = TempDir::new().unwrap();
        let path = home_isolated_config(&tmp);
        let cfg = Config::default();
        cfg.save_to(&path).unwrap();

        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "found stragglers: {:?}", entries);
        assert_eq!(entries[0], "config.toml");
    }
}
