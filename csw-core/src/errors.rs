use std::path::PathBuf;

#[derive(thiserror::Error, Debug)]
pub enum CswError {
    #[error("canonical clone not found at {0}")]
    CanonicalMissing(PathBuf),

    #[error("{path} exists but is not a git repository")]
    NotAGitRepo { path: PathBuf },

    #[error("{path} is on branch {actual}, expected {expected}")]
    WrongBranch {
        path: PathBuf,
        actual: String,
        expected: String,
    },

    #[error("working tree dirty in {path}")]
    Dirty { path: PathBuf, files: Vec<String> },

    #[error("unpushed commits in {path}")]
    Unpushed { path: PathBuf, count: usize },

    #[error("repo '{0}' not found in config")]
    UnknownRepo(String),

    #[error("repo '{0}' already exists in config")]
    RepoAlreadyExists(String),

    #[error("editor template must contain {{path}}")]
    EditorTemplateMissingPath,

    #[error("editor template could not be parsed")]
    EditorTemplateUnparseable,

    #[error("could not resolve username (no config, no git email, no $USER)")]
    UsernameUnresolvable,

    #[error("could not infer task from current directory")]
    TaskInferenceFailed,

    #[error("git command failed: {0}")]
    GitCommandFailed(String),
}
