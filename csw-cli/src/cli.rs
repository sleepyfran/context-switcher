use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "csw",
    version,
    about = "Per-task git worktrees across one or more repos, with editor/CMux integration.",
    propagate_version = true
)]
pub struct Cli {
    #[command(flatten)]
    pub verbosity: Verbosity,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Debug, Clone, Copy)]
pub struct Verbosity {
    /// Suppress non-error output.
    #[arg(long, short, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Print every git command and its output.
    #[arg(long, short, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Create or resume a task copy and open the editor.
    Start(StartArgs),

    /// Drop into a subshell rooted at one of a task's copies.
    Nav(NavArgs),

    /// Fetch + prune the canonical clones, so subsequent operations start from
    /// fresh remote-tracking refs.
    Fetch(FetchArgs),

    /// Fetch + fast-forward each canonical clone's base branch to its upstream.
    Pull(PullArgs),

    /// Mark a task done: delete the copy(ies), with safety checks.
    #[command(visible_alias = "rm")]
    Done(DoneArgs),

    /// Show the state of a task: dirty / ahead-behind / merged across all copies.
    Status(StatusArgs),

    /// List all task copies on disk.
    List(ListArgs),

    /// Change a task's title. Updates every per-repo sidecar and the
    /// CMux workspace label (when running inside CMux).
    Retitle(RetitleArgs),

    /// Manage the configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(Args, Debug)]
pub struct RetitleArgs {
    /// Task identifier or new title.
    ///
    /// When two positionals are supplied, the first is the task id and the
    /// second is the title. When only one is supplied, it's the title and
    /// the task is inferred from the current directory. Pass an empty
    /// string to clear the existing title.
    pub first: String,

    /// New title (when a task id is given as the first positional).
    pub title: Option<String>,

    /// Suppress the CMux workspace rename for this invocation.
    #[arg(long)]
    pub no_cmux: bool,
}

#[derive(Args, Debug)]
pub struct StartArgs {
    /// Task identifier (e.g. `PROJ-123` or full `user/PROJ-123`).
    /// When omitted, csw lists existing task copies and lets you pick one.
    pub task_id: Option<String>,

    /// Repos to operate on, in addition to default_repos.
    #[arg(long, value_delimiter = ',', conflicts_with = "only")]
    pub repos: Vec<String>,

    /// Repos to operate on, replacing default_repos entirely.
    #[arg(long, value_delimiter = ',')]
    pub only: Vec<String>,

    /// Optional human title — stored as metadata in the task's sidecar.
    #[arg(long)]
    pub title: Option<String>,

    /// Skip launching the editor.
    #[arg(long)]
    pub no_editor: bool,

    /// Don't run the configured `post_create` hooks (file copies, commands).
    #[arg(long)]
    pub skip_hooks: bool,

    /// Suppress the CMux workspace integration for this invocation. No
    /// effect outside CMux or when the integration is globally disabled.
    #[arg(long)]
    pub no_cmux: bool,

    /// Always create a new CMux workspace, even when the current one is
    /// simple enough to be reshaped in place. Overrides the
    /// `cmux.replace_simple_workspace` config setting for this invocation.
    #[arg(long)]
    pub force_new_workspace: bool,

    /// Override the branch for a specific repo, e.g. `--branch backend=feature/x`.
    /// Repeatable for multiple repos. The default for any repo without an
    /// override is `<user>/<task-id>`.
    #[arg(long, value_name = "REPO=BRANCH")]
    pub branch: Vec<String>,
}

#[derive(Args, Debug)]
pub struct NavArgs {
    /// Task identifier. When omitted, csw lists existing task copies and
    /// lets you pick one (matches the bare `csw start` flow).
    pub task_id: Option<String>,

    /// When the task spans multiple repos, pick which one's copy to drop into.
    #[arg(long)]
    pub repo: Option<String>,

    /// Print the absolute path of the chosen copy to stdout instead of
    /// spawning a subshell. Useful for shell wrappers like
    /// `cs() { cd "$(csw nav --print-path "$@")"; }`.
    #[arg(long)]
    pub print_path: bool,
}

#[derive(Args, Debug)]
pub struct FetchArgs {
    /// Limit to a subset of configured repos. Comma-separated. When omitted,
    /// every configured repo is fetched.
    #[arg(long, value_delimiter = ',')]
    pub repos: Vec<String>,
}

#[derive(Args, Debug)]
pub struct PullArgs {
    /// Limit to a subset of configured repos. Comma-separated. When omitted,
    /// every configured repo is pulled.
    #[arg(long, value_delimiter = ',')]
    pub repos: Vec<String>,
}

#[derive(Args, Debug)]
pub struct DoneArgs {
    /// Task identifier — inferred from cwd when omitted.
    pub task_id: Option<String>,

    /// Skip dirty / unpushed-commit checks.
    #[arg(long, short)]
    pub force: bool,

    /// Auto-confirm the pushed-but-unmerged prompt.
    #[arg(long, short)]
    pub yes: bool,

    /// Don't delete the local branch in the canonical clone.
    #[arg(long)]
    pub keep_branch: bool,

    /// Don't close the matching CMux workspace. By default `csw done` closes
    /// `csw/<task-id>` after deleting the copies; pass this to leave it.
    #[arg(long)]
    pub keep_workspace: bool,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Task identifier — inferred from cwd when omitted.
    pub task_id: Option<String>,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Limit to a single repo.
    #[arg(long)]
    pub repo: Option<String>,

    /// Emit JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCommand {
    /// Print the effective configuration.
    Show,

    /// Set a top-level key (`base_dir`, `tasks_dir`, `username`, `default_repos`).
    Set { key: String, value: String },

    /// Open the config file in $VISUAL/$EDITOR for manual editing.
    Edit,

    /// Manage individual repos.
    #[command(subcommand)]
    Repo(RepoCommand),
}

#[derive(Subcommand, Debug)]
pub enum RepoCommand {
    /// Add a repo.
    Add {
        name: String,

        #[arg(long)]
        path: String,

        #[arg(long)]
        editor: String,

        #[arg(long)]
        base_branch: Option<String>,

        /// Append this repo to default_repos.
        #[arg(long)]
        default: bool,
    },

    /// Update a single field on a repo (`path` | `editor` | `base_branch`).
    Set {
        name: String,
        field: String,
        value: String,
    },

    /// Remove a repo.
    Remove { name: String },

    /// List configured repos.
    List,

    /// Replace `default_repos` wholesale.
    Default { names: Vec<String> },

    /// Manage post-create hooks for a repo.
    #[command(subcommand)]
    Hooks(HooksCommand),
}

#[derive(Subcommand, Debug)]
pub enum HooksCommand {
    /// Interactive wizard to add one or more hooks to a repo.
    Add { repo: String },

    /// List the hooks configured for a repo, with their indices.
    List { repo: String },

    /// Interactively pick a hook to delete.
    Remove { repo: String },

    /// Delete every hook configured for a repo (with confirmation).
    Clear { repo: String },
}
