use crate::cmux::config::RepoCmuxConfig;
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmux: Option<RepoCmuxConfig>,
}

impl RepoConfig {
    pub fn new(path: impl Into<PathBuf>, editor: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            editor: editor.into(),
            base_branch: None,
            post_create: Vec::new(),
            cmux: None,
        }
    }
}
