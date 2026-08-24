use crate::hooks::HookAction;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RepoConfig {
    pub path: PathBuf,
    pub editor: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub post_create: Vec<HookAction>,
    /// Extra idle tabs (CMux surfaces) stacked behind the repo's editor pane
    /// in the top row, each `cd`'d into the repo's worktree with nothing
    /// autostarting. Switch between them with ⌘[ / ⌘]. Only takes effect
    /// when editors are attached (`detached_editor = false`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub additional_panels: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl RepoConfig {
    pub fn new(path: impl Into<PathBuf>, editor: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            editor: editor.into(),
            base_branch: None,
            post_create: Vec::new(),
            additional_panels: 0,
        }
    }
}
