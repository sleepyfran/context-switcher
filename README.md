# csw

A Rust CLI I built for myself. It wraps `git worktree` for tasks that span one or more repos, with hooks for the rest of my workflow: CMux workspace orchestration, an agent that gets dropped into the task root, and whatever else I might add later.

## Why it exists

Since I started using agentic coding more and more I find myself doing a lot of switching in between different contexts. These contexts usually includes different repos checked-out on different tasks and since, _gasp_, I still want to be in control of the code I output I end up always switching branches, retesting here and there... It was just too messy to keep track of by hand, so that's where `csw` enters the picture. One command spins up worktrees for as many repos as the task touches, copies what needs copying, runs whatever needs running, opens the editors, drops an agent into the task root with visibility of every repo, builds the CMux workspace. One command tears it all down. Worktrees are just plumbing; what I actually keep adding to is the integration layer (post-create hooks, CMux layout, the agent pane).

## How it works

Each task lives at `<tasks_dir>/<user>-<task-id>/<repo>` as a real `git worktree` linked to the canonical clone — every repo a task touches lives together under one task root. Default `tasks_dir` is `~/.config/context-switcher/tasks/`.

```
~/Developer/
├── frontend/                           # canonical clone (cloned by you, the human, hopefully)
└── backend/                            # canonical clone (cloned by you, the human, hopefully)

~/.config/context-switcher/
├── config.toml
└── tasks/
    ├── alice-PROJ-123/                 # task root
    │   ├── frontend/                   # worktree, branch alice/PROJ-123
    │   └── backend/                    # same task, different repo
    └── alice-PROJ-456/                 # another task
        └── frontend/
```

Task-rooted layout means the agent (and you) can `cd` once and see every repo the task touches, and cross-repo operations (moving a task, inspecting it as a whole) don't need to reach into N separate per-repo directories.

## Install

```sh
git clone <this repo>
cd context-switcher
cargo install --path csw-cli
```

That puts a `csw` binary at `~/.cargo/bin` (which you'll want on `$PATH`). You also need the `git` binary on `$PATH` at runtime, since the tool shells out for everything git-related.

## First-time setup

`csw` keeps its config at `~/.config/context-switcher/config.toml`. Every field is reachable through `csw config`, but you can edit the TOML by hand too.

1. Point `csw` at the directory where your canonical clones live (defaults to `~/Developer`):

   ```sh
   csw config set base_dir ~/repos # Only if you have your repos outside of `~/Developer`
   ```

2. Optionally override where worktrees get materialised (defaults to `<config_dir>/tasks/`):

   ```sh
   csw config set tasks_dir ~/Developer/tasks # Only if you'd like the repos to be near the canonical copies
   ```

3. Optionally pin a username. Otherwise it falls back to the prefix of `git config --global user.email`, then `$USER`:

   ```sh
   csw config set username alice
   ```

4. Register each repo. The `--path` is relative to `base_dir` (or absolute):

   ```sh
   csw config repo add frontend \
       --path frontend \
       --editor "zed {path}"
   ```

   `{path}` gets substituted with the worktree's absolute path when csw spawns the editor. Use `--editor ""` if you'd rather open the editor yourself.

5. Mark the repos you want csw to operate on by default. After this, `csw start PROJ-123` with no `--repos` will use this list:

   ```sh
   csw config repo default frontend
   ```

6. Optionally set the agent command that gets dropped into the task root on `csw start` (see [CMux integration](#cmux-integration)):

   ```sh
   csw config set agent claude
   ```

7. Sanity check what got written:

   ```sh
   csw config show
   ```

`csw` doesn't create the canonical clones for you. Clone them yourself once (`git clone <upstream> ~/Developer/frontend`) and from then on csw uses each canonical as the source for every worktree.

## Day to day

Start a task:

```sh
csw start PROJ-123                            # uses default_repos
csw start PROJ-123 --repos backend            # add backend to your defaults
csw start PROJ-123 --only docs                # ignore defaults, use docs only
csw start PROJ-123 --title "Fix navbar bug"   # store a human label in the sidecar
csw start PROJ-123 --no-editor                # skip launching the editor
```

You can also pass a fully-qualified branch if you have a legacy or non-conventional one already pushed:

```sh
csw start someoneelse/legacy-branch-with-slug --repos frontend
```

If the branch already exists on the remote, csw silently checks it out into the new worktree and tracks it.

When a task spans multiple repos and one of them follows a different branch convention, override that repo's branch with `--branch repo=branch`. The flag is repeatable and only affects the listed repos:

```sh
csw start PROJ-123 --repos frontend,backend \
    --branch backend=feature/legacy-thing
```

Repos without an override still default to `<user>/<task-id>`.

Drop into a subshell rooted at one of a task's worktrees:

```sh
csw nav PROJ-123                   # one-repo task: just enter
csw nav PROJ-123 --repo backend    # multi-repo task: pick which one
csw nav                            # no arg: pick a task interactively
csw nav PROJ-123 --print-path      # emit the path instead of spawning a shell
```

The subshell inherits your env, gets `CSW_TASK_ID`, `CSW_BRANCH`, `CSW_REPO`, `CSW_USER`, `CSW_WORKTREE`, and `CSW_CANONICAL` exported (handy for prompt customisation), and propagates its exit code. Type `exit` or hit Ctrl+D to come back to where you were.

`--print-path` is the escape hatch for shell wrappers, e.g. a fish function `function cs; cd (csw nav --print-path $argv); end` to swap the subshell for a real `cd` in your current shell.

See what's in flight:

```sh
csw list
csw list --json
csw list --repo frontend
```

Inspect a task:

```sh
csw status PROJ-123
csw status                  # infers the task from cwd if you're inside a worktree
```

Refresh the canonical clones so subsequent `csw start` operations see fresh remote refs. Runs `git fetch --prune origin` against every configured repo:

```sh
csw fetch                              # fetch every configured repo
csw fetch --repos frontend,backend     # only these
```

Sequential, best-effort: a single repo's failure (missing canonical, network error) is reported but doesn't stop the rest. Exit 0 if everything fetched, 2 if any repo failed.

Finish a task and remove its worktrees. csw refuses if any worktree is dirty, has unpushed commits, or has no upstream set:

```sh
csw done PROJ-123
csw done                    # infers from cwd
csw done PROJ-123 --force   # bypass safety checks (irrevocable; the branch's only copy goes too)
csw done PROJ-123 --keep-branch     # leave the branch behind in the canonical
csw done PROJ-123 --keep-workspace  # don't close the matching CMux workspace
```

If the branch is pushed but not merged into your base branch, csw warns and prompts before removing. Pass `--yes` to skip the prompt.

## Migrating from the old layout

Older versions of csw laid worktrees out per-repo (`<tasks_dir>/<repo>/<user>-<task-id>`) instead of per-task. If csw finds tasks still sitting in that shape, every command that discovers tasks (`start`, `list`, `status`, `done`, `nav`, `retitle`) prints a one-line warning:

```
warning: 2 legacy tasks found under the old layout — run `csw migrate`
```

Run the migration:

```sh
csw migrate --dry-run   # show what would move, without touching anything
csw migrate             # prompts for confirmation, then moves everything
csw migrate --yes       # skip the confirmation prompt
```

It uses `git worktree move` under the hood (not a raw `mv`), so branches, git metadata, and sidecars all survive the move intact, and it cleans up the now-empty old per-repo directories once nothing's left in them. Migration is user-agnostic — it picks up every old-format task it finds on disk, regardless of which user owns it.

## Per-repo configuration

Each repo can override the base branch (defaults to `origin/HEAD`), use its own editor, and open extra idle tabs alongside it:

```sh
csw config repo add api \
    --path api-monorepo \
    --editor "pycharm {path}" \
    --base-branch develop
```

Update one field later:

```sh
csw config repo set api editor "code {path}"
csw config repo set api base_branch main
csw config repo set api additional_panels 2
```

`additional_panels` opens that many extra idle tabs (CMux surfaces) behind the repo's editor pane in the top row — each just `cd`'d into the repo's worktree, nothing autostarting. Switch between them with ⌘[ / ⌘]. Only takes effect when editors are attached (see [CMux integration](#cmux-integration)); it's ignored with `detached_editor = true` since there's no editor pane to attach tabs to.

Remove a repo:

```sh
csw config repo remove api
```

List everything csw knows about:

```sh
csw config repo list
```

## Post-create hooks

When `csw start` creates a fresh worktree, it can run a list of "post-create" actions you've configured per repo. Two real cases:

- A gitignored `.env` that lives in the canonical clone. Every worktree needs a copy of it before the app will run.
- After every fresh checkout you want `pnpm install` (or whatever your install command is) to happen before the editor opens.

Hooks fire only when a worktree is freshly created, never on resume, so you don't accidentally clobber in-flight work by re-running them. They run in declaration order. The first failure stops processing for that repo, leaves the worktree on disk for inspection, and flips that repo to a failure in the run report. Sibling repos in a multi-repo task carry on.

Two action types:

- **`copy`** copies a file or directory from the canonical clone into the new worktree. Paths are relative to each side (no `..`, no absolute paths). Directories are recursive. An `optional = true` flag lets a missing source be skipped instead of failing.
- **`run`** runs a shell command via `sh -c`, with cwd at either the new worktree (default) or the canonical clone. The command sees these env vars: `CSW_WORKTREE`, `CSW_CANONICAL`, `CSW_TASK_ID`, `CSW_BRANCH`, `CSW_USER`, `CSW_REPO`. Output is captured by default; on failure csw prints the last 50 lines.

### Adding hooks

Editing the TOML by hand for nested structure is annoying, so there's a wizard:

```sh
csw config repo hooks add myrepo
```

It walks one action at a time and asks if you want to add another at the end:

```
? action type › copy
? source path (relative to canonical) › .env
? does the target path differ? (y/N) › n
? optional? (skip if missing) (y/N) › n
? add another? (y/N) › y
? action type › run
? command › pnpm install --frozen-lockfile
? working directory › worktree
? display name (optional) › install deps
? add another? (y/N) › n
• added 2 hooks to myrepo
```

That session lands in your config file as:

```toml
[[repos.myrepo.post_create]]
type = "copy"
path = ".env"

[[repos.myrepo.post_create]]
type = "run"
cmd = "pnpm install --frozen-lockfile"
cwd = "worktree"
name = "install deps"
```

### Inspecting and removing hooks

```sh
csw config repo hooks list myrepo       # plain indexed list
csw config repo hooks remove myrepo     # interactive picker
csw config repo hooks clear myrepo      # confirms then deletes everything
```

### Skipping hooks on a single start

For when you want a bare worktree without running setup (network is flaky, you just want to inspect history, etc.):

```sh
csw start PROJ-123 --skip-hooks
```

### Editing the config directly

Reordering hooks and any other manual surgery isn't in the wizard. For that:

```sh
csw config edit
```

That opens the config in `$VISUAL` / `$EDITOR` / `vi`, validates that the result still parses, and refuses to swap it in if you've broken the syntax. Your original file stays untouched on a parse failure.

## CMux integration

If you use [CMux](https://cmux.com) as your terminal multiplexer, csw can spin up a workspace each time you `csw start`: one sidebar entry per task, laid out as a top row of editor panes plus a bottom agent pane, with a deterministic accent color. `csw done` closes that workspace alongside the disk cleanup.

The integration activates only when csw notices it's running inside a CMux surface (the `CMUX_WORKSPACE_ID` env var that CMux auto-exports). Outside CMux it's a complete no-op; nothing about your config or workflow changes for non-CMux callers.

### The fixed layout

The layout isn't per-pane configurable anymore — it's the same shape every time:

- **Top row**: one pane per repo the task was started with, laid out left-to-right, each running that repo's `editor` command, `cd`'d into that repo's worktree.
- **Bottom pane**: a single pane spanning the full width, running the top-level `agent` command, `cd`'d into the *task root* — so the agent has visibility of every repo the task touches, not just one.

```toml
agent = "claude"

[repos.frontend]
path = "frontend"
editor = "nvim"

[repos.backend]
path = "backend"
editor = "nvim"
```

`csw start PROJ-123 --repos frontend,backend` then opens a workspace with `frontend` and `backend` panes side by side on top, and `claude` running in the task root underneath. Leave `editor` empty on a repo to get just an idle shell in its slot; leave `agent` unset to skip the bottom pane entirely (top row only). If neither is set, csw skips workspace creation — no empty sidebar entries.

Use `csw config repo set <name> additional_panels <N>` to open extra idle tabs behind a repo's editor pane (see [Per-repo configuration](#per-repo-configuration)).

### Attached vs. detached editors

By default (`detached_editor = false`) editors run **attached**, as the top-row CMux panes described above. Set `detached_editor = true` to go back to the old behavior instead: each repo's `editor` launches as a detached GUI process (`zed .`, `code .`, ...) in its own window, independent of CMux, and no top row is built — the workspace then contains just the full-width agent pane (or nothing, if no `agent` is set).

```toml
detached_editor = true   # default false (attached / CMux panes)
```

Attached is the right default for terminal editors (`nvim`, `hx`) — they need a pane to run in. Flip to `detached_editor = true` if your editor is a GUI app that spawns its own window (`zed`, `code`, `pycharm`); it doesn't need a CMux pane at all.

### Workspace name and color

The sidebar title is `<task-id>` on its own, or `<task-id> · <title>` when `--title` was given (the title is recovered from the sidecar on resume, so day-2 `csw start PROJ-123` keeps the day-1 title even without re-passing `--title`).

Each task picks a deterministic color from a curated 10-color palette by hashing its id. The same task id always gets the same color, so the sidebar entry looks the same after a rebuild.

### Disabling

Three escape hatches, in order of scope:

```toml
[cmux]
enabled = false                    # global kill-switch; csw never talks to CMux
replace_simple_workspace = false   # never reshape the current workspace; always open a new one
```

```sh
csw start PROJ-123 --no-cmux              # this invocation only
csw start PROJ-123 --force-new-workspace  # skip in-place adoption for this invocation
csw done  PROJ-123 --keep-workspace       # leave the workspace open after done
```

### Resume and idempotency

On every `csw start`, csw scans CMux's workspaces for one whose name equals `<task-id>` or starts with `<task-id> · `. If it finds one, it just focuses it; no rebuild. If not, it creates a fresh workspace.

That means you can manually rename the suffix in CMux's sidebar (e.g. add a 🚧 marker) without breaking the next resume, as long as the task id stays at the start.

`csw done` uses the same matcher to find what to close.

### Reshaping the current workspace in place

When `csw start` would otherwise create a brand-new workspace (no existing match by name), and the workspace csw is currently running inside is "simple" — exactly one pane with one tab, no splits — csw reshapes it in place instead of opening a new sidebar entry. The current workspace gets renamed to `<task-id>` (or `<task-id> · <title>`), recolored to the task's accent color, and populated with the task layout. You see `cmux: reshaped current workspace into <name>` in the output instead of `cmux: opened workspace ...`.

This avoids the common case where you open a fresh CMux workspace just to type `csw start PROJ-123`, only to end up with two near-identical sidebar entries.

```toml
[cmux]
replace_simple_workspace = true   # default; false disables the in-place behavior
```

```sh
csw start PROJ-123                          # adopt current simple workspace if applicable
csw start PROJ-123 --force-new-workspace    # skip adoption, always open a new workspace
```

If the current workspace has multiple panes, multiple tabs, or csw can't query the CMux side for any reason, the build falls back to creating a new workspace. The reuse-by-name path (existing workspace matching the task id) always wins over adoption: if you already have an open `PROJ-123` somewhere, csw focuses that instead of reshaping the current pane.

### Editor relationship

`editor` (per-repo, in `[repos.<name>]`) drives both worlds: attached (default), it becomes a top-row CMux pane's command; detached, it's spawned as a GUI process the old way, via `editor::spawn`. `--no-editor` skips launching/attaching editors for that invocation either way; `--no-cmux` skips the whole workspace (top row and agent pane both); set both for a bare start with nothing but worktrees on disk.

### Caveats

- macOS only. CMux is macOS-only, and the integration code only fires when `CMUX_WORKSPACE_ID` is set in the env.
- CMux failures are always soft. If the socket is unreachable or a method call fails, csw prints a single `warning: cmux: ...` line and continues. The exit code is never affected.
- Changing `agent`, `detached_editor`, a repo's `editor`, or its `additional_panels` has no effect on an already-open workspace. To pick up the new layout, close the workspace in CMux and `csw start` again, or just `csw done` and re-start.
- Each pane's command is delivered via ` cd '<path>' && <cmd>\n` (leading space, single-quoted path). The cd line stays out of bash/zsh history when `HISTCONTROL` includes `ignorespace`; fish's history ignores leading-space lines by default.

## What lives where

After `csw start PROJ-123 --repos frontend,backend`:

```
~/Developer/                                # canonicals (untouched by csw)
├── frontend/
└── backend/

~/.config/context-switcher/
├── config.toml
└── tasks/
    └── alice-PROJ-123/                     # task root
        ├── frontend/                       # worktree on alice/PROJ-123
        └── backend/                        # worktree on alice/PROJ-123
```

The per-task sidecar (task id, branch, optional title, created-at) lives at `<canonical>/.git/worktrees/<wt-name>/csw.json`, inside the per-worktree git dir, which `git` wipes automatically when the worktree is removed. Nothing for csw to clean up on `done`.

## Exit codes

- `0` success
- `1` something blew up (config issue, git error, anything unexpected)
- `2` pre-flight failed for one or more repos, or a per-repo execute failed
- `3` `csw done` refused on safety grounds (dirty, unpushed, or no upstream)

## Verbosity

Default output is one progress line per repo plus the final result. The progress UI is a per-repo spinner when run interactively, suppressed when stderr isn't a TTY (so pipes and CI logs stay clean).

```sh
csw start PROJ-123 --quiet      # only errors
csw start PROJ-123 --verbose    # every git command and its output
```

## Caveats

- cwd-based inference for `csw done` and `csw status` keys off your configured username. If a worktree was made under a different username (legacy branch, someone else's branch), pass the full `<user>/<task-id>` form explicitly.
- I've only run this on macOS. It should work on Linux without surprises. Windows... just don't unless you're planning on using it on WSL.
- `csw done --force` is irrevocable: the worktree shares its `.git` with the canonical, so the branch's commits live in exactly one place. Forcing the worktree away deletes the branch with it.

## Tests

```sh
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The git tests shell out to a real `git` binary and create real repos in tempdirs. They take a few seconds.
