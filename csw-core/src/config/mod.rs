mod repo;

pub use repo::RepoConfig;

use crate::mux::config::{MuxConfig, ProviderChoice};
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

    /// Command run in the bottom multiplexer pane, rooted at the task's root
    /// folder (visible to every repo the task was started with). No
    /// `{path}` placeholder — csw always `cd`s into the task root before
    /// running it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,

    /// When `true`, restores the legacy behavior: each repo's `editor`
    /// launches as a detached GUI process (`editor::spawn`), independent of
    /// the multiplexer, and no top-row editor panes are built. Defaults to
    /// `false` (attached): editors run as multiplexer panes in the top row.
    #[serde(default)]
    pub detached_editor: bool,

    /// `[mux]` table. Older configs spelled it `[cmux]`; that name is still
    /// accepted on read and rewritten as `[mux]` on the next save.
    #[serde(default, alias = "cmux", skip_serializing_if = "Option::is_none")]
    pub mux: Option<MuxConfig>,

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
            agent: None,
            detached_editor: false,
            mux: None,
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

    /// Root directory for a single task's worktrees, i.e.
    /// `<tasks_dir>/<user>-<task_id>/`. Every repo the task was started
    /// with lives as a subdirectory beneath this.
    pub fn task_root(&self, user: &str, task_id: &str) -> PathBuf {
        self.tasks_dir
            .join(crate::paths::worktree_dirname(user, task_id))
    }

    /// Pretty TOML representation, used by `csw config show`.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).context("serialising config to TOML")
    }

    /// Whether the multiplexer integration is enabled. Default is on; an
    /// absent `[mux]` block reads as enabled. Only an explicit
    /// `enabled = false` turns it off.
    pub fn mux_enabled(&self) -> bool {
        self.mux.as_ref().map(|c| c.enabled).unwrap_or(true)
    }

    /// Which multiplexer(s) csw may drive. Defaults to `auto`.
    pub fn mux_provider(&self) -> ProviderChoice {
        self.mux.as_ref().map(|c| c.provider).unwrap_or_default()
    }

    /// Whether `csw start` should reshape the current workspace in place
    /// when it's simple (single pane, single tab) instead of creating a new
    /// sidebar entry. Defaults to on.
    pub fn mux_replace_simple_workspace(&self) -> bool {
        self.mux
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
                additional_panels: 2,
            },
        );
        let cfg = Config {
            base_dir: PathBuf::from("/tmp/dev"),
            tasks_dir: PathBuf::from("/tmp/csw/tasks"),
            username: Some("alice".into()),
            default_repos: vec!["frontend".into()],
            agent: Some("claude".into()),
            detached_editor: true,
            mux: None,
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
    fn task_root_joins_user_and_task_id() {
        let cfg = Config {
            tasks_dir: PathBuf::from("/csw/tasks"),
            ..Config::default()
        };
        assert_eq!(
            cfg.task_root("alice", "PROJ-1"),
            PathBuf::from("/csw/tasks/alice-PROJ-1")
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
