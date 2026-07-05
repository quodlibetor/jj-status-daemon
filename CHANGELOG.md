# Unreleased

jj incremental diff engine overhaul: exact `jj diff --stat` parity and
self-healing after watcher event loss.

**Correctness (jj diff stats)**
- **self-healing on every jj operation**: `ValidateAndRefresh` now compares the
  working-copy commit's tree (and the operation id) in addition to the parent
  tree. Same-parent checkouts (`jj abandon`, `jj edit` to a sibling,
  `jj restore`, `jj undo`) rebuild base stats from the store instead of
  trusting watcher events, so dropped/coalesced filesystem events can no
  longer cause permanently drifting +/- counts.
- **conflicted files**: diffs now materialize conflicted tree values with
  conflict markers exactly as jj does, instead of treating them as
  absent. Touching a file that is conflicted in the parent tree no longer
  counts the entire file as added (previously produced arbitrarily large
  bogus counts, especially in megamerge workflows).
- **zero-line changes count as changed files**: added/deleted empty files,
  binary file changes, and exec-bit-only changes now count toward "N files
  changed", matching `jj diff --stat`.
- **trailing-newline line counting**: a final line without a trailing newline
  now counts as a line, matching `diff --stat`.
- **FSEvents overflow handling**: rescan-flagged notify events (OS event queue
  overflow) previously were silently dropped; they now trigger a full resync
  that rebuilds state from the store and re-diffs all known-dirty files.
- **deleted directories**: repo-relative path mapping now works for files
  whose parent directories were deleted along with them.
- **`ui.conflict-marker-style` respected**: conflict materialization reads the
  user's configured marker style (user config files and repo-level config,
  including the modern `config-id`-keyed repo config location) instead of
  assuming jj's default `diff` style, so conflicted files diff as unchanged
  regardless of configuration.
- **rename/copy detection**: diff stats now match jj's rename handling. Full
  refreshes use the backend's copy records (`{a => b} | 2 +-` counts as one
  changed file with real line deltas, including fuzzy move+edit renames);
  the incremental overlay pairs equal-content Added/Deleted entries as
  renames and diffs edits to a renamed file against the rename source's
  parent content. Previously a `mv` counted as a full delete plus a full
  add.
- **ignore-aware incremental diffs**: overlay entries for files that are
  untracked *and* ignored are recorded as "no change", so a file that
  becomes covered by `.gitignore` after being seen once can no longer leave
  a permanent phantom Added entry.
- **watcher panic fix**: ignore matching now maps event paths to
  repo-relative form before consulting the `ignore` crate, which panics on
  paths outside its root (possible with non-canonical event path prefixes,
  e.g. macOS `/var` vs `/private/var`).

**Testing**
- **property-testing engine** (`src/diff_props.rs`): random sequences of file
  mutations and jj commands replayed against a real repo, with worker state
  compared to `jj diff --stat` after every action. A lossy mode drops event
  batches to simulate FSEvents overflow and asserts the engine converges at
  the next jj operation. Set `DIFF_PROP_CASES=n` and `DIFF_PROP_SEQ_LEN=n`
  to scale the campaign. The action set covers file writes/appends/edits/
  deletes/renames, directory moves, `jj new/commit/describe/abandon/squash/
  undo/edit/restore/split/rebase`, merge creation, conflicts, marker-style
  config changes, `.gitignore` mutations, and git-remote interaction
  (commits to a side repo, `jj git fetch`, `jj new main@origin`).

# v0.0.13

* Remove colocated git checkout detection. It is not ready for prime time.

# v0.0.12

Incremental diff improvements, colocated jj+git fixes, template debugging, and
bookmark limiting.

**Bookmark limiting**
- **`limit_bookmarks` template filter**: new Tera filter that truncates bookmark
  lists with a "(+N more)" indicator. Accepts `count` (max visible) and optional
  `prioritize` (glob pattern to sort matching bookmarks first, e.g. `bwm/*`).
- **`[template.vars]` config table**: user-defined variables injected into the
  Tera rendering context. Use `max_bookmarks` and `prioritize_branches` to
  control bookmark limiting in built-in templates.
- **auto-applied in built-in templates**: all templates that iterate bookmarks
  (detail, gitstatus, ohmyzsh, starship) conditionally apply `limit_bookmarks`
  when the `max_bookmarks` template var is set.

**Config restructure**
- **`[template]` table**: `template_name`, `format`, and `not_ready_format` are
  now nested under `[template]` as `name`, `format`, and `not_ready_format`.
  This is a **breaking change** — old flat keys will produce a config error.
- **`config set`/`config get` dotted keys**: `config set template.name nerdfont`,
  `config set template.vars.max_bookmarks 3`, etc.

**Performance**
- **fine-grained incremental diff invalidation**: file watcher events now
  update only the affected files in the diff overlay instead of recomputing
  the entire diff, significantly reducing refresh cost for large repos.

**Bug fixes**
- **colocated jj+git repos**: `.git/` internal paths are no longer treated
  as working copy changes, fixing spurious diff recalculations.
- **colocated git checkout detection**: when an external `git checkout`
  changes HEAD in a colocated jj repo, the status now shows a diverged
  indicator prompting the user to run a jj command to reconcile.

**Template debugging**
- **`template debug`**: new subcommand that shows the current template with
  each variable's value annotated inline (e.g. `{{ change_id_prefix=su | magenta }}`),
  plus a list of available variables not referenced by the template.

**Observability**
- **`status --verbose` template variables**: verbose status output now
  includes per-repo template variable values, showing the full rendering
  context for each watched repository.

# v0.0.11

Shell completions, broader shell support, and improved template CLI.

**Shell completions**
- **tab completion for all subcommands**: `init completions <shell>` generates
  completions via `clap_complete` for bash, zsh, fish, elvish, and powershell.

**Broader shell support**
- **fish and nushell init support**: `init fish` and `init nushell` generate
  eval-able shell integration scripts, joining the existing bash and zsh support.

**Template CLI improvements**
- **`template list` renamed to `template show`**: accepts optional template
  names to filter output (e.g. `template show ascii nerdfont`).
- **`template set`**: new subcommand to set the active template by `--name`
  or inline `--format`, equivalent to the corresponding `config set` calls.

**Observability**
- **verbose per-directory diff stats**: `status -v` now shows per-directory
  breakdown of incremental diff overlay statistics (base files, overlay
  entries, files/lines changed).

**Upgrade handling**
- **detect binary deletion for package manager updates**: when tools like
  mise, nix, or asdf update the binary to a new path and delete the old one,
  the daemon now detects the deletion and shuts down cleanly. The next client
  query auto-starts the new version.
- **auto-shutdown on version mismatch**: when a client detects it is a newer
  version than the running daemon, it now sends a shutdown request instead of
  just warning. The next prompt evaluation auto-starts the correct version.

**Watcher improvements**
- **lazy nested ignore file discovery**: the file watcher now discovers
  `.gitignore`/`.jjignore` files in subdirectories as events arrive, rather
  than only loading root-level ignore files. Uses a checked-directories cache
  to avoid repeated filesystem probes.

# v0.0.10

Nested gitignore support, new template commands, and incremental diff observability.

**Nested gitignore discovery**
- **lazy nested `.gitignore`/`.jjignore` loading**: the file watcher now
  discovers ignore files in subdirectories as events arrive, rather than
  only loading root-level ignore files. New `IgnoreFilter` struct handles
  thread-safe lazy discovery and rebuilds the matcher when ignore files
  are created, modified, or deleted.

**New template commands**
- **`template print <name>`**: prints the raw Tera source of a template
  with includes inlined.
- **`template list -n`**: prints just template names (builtins +
  user-defined) without descriptions.

**Observability**
- **incremental diff stats in `status`**: the `status` subcommand now
  shows per-repo incremental diff overlay statistics (base files, overlay
  entries, file/line counts), queried from the jj and git worker threads.

**Bug fixes**
- **fix: export `VCS_STATUS_DAEMON` env var for starship** so the
  starship template preset works correctly.

# v0.0.9

Richer status information, seamless upgrades, and better observability.

**Richer status output**
- **ahead/behind and stash counts**: git repos now show ahead/behind
  counts (via `graph_ahead_behind`) and stash count. Jj repos show
  per-bookmark tracking status (tracked/ahead/behind/sideways) using
  index-based ancestor checks. All 9 templates updated with indicators.
- **built-in templates cloning popular prompts**: new `gitstatus`
  (Powerlevel10k lean), `starship`, `ohmyzsh`, and `pure` template
  presets.
- **renamed diff fields for clarity**: `files_changed` → `file_mad_count`,
  unstaged fields gain `_working_tree` suffix, staged/total prefixes
  moved to suffixes (e.g. `lines_added_staged`). `TrackingStatus::Diverged`
  renamed to `Sideways`.

**Seamless upgrades**
- **self-update subcommand**: `self-update` downloads the latest release
  via `curl | sh`. `self-update --check` queries GitHub for the latest
  version without installing.
- **auto-restart on binary replacement**: the daemon watches its own
  executable and automatically exec's the new version when replaced.
- **SIGHUP restart**: sending SIGHUP to the daemon triggers a restart,
  useful for post-install scripts.
- **version logged at startup**: the daemon logs its version and git hash
  on startup for upgrade traceability.

**Observability and operations**
- **set-log-filter command**: dynamically change the daemon's log level
  at runtime.
- **notification timing metrics**: the `status` subcommand now shows
  full vs incremental refresh counts.
- **remove idle shutdown**: the daemon no longer exits after a period of
  inactivity. The `idle_timeout_secs` config key is removed.

**Bug fixes**
- **fix coalesced VCS-internal events**: when a VCS-internal event
  arrived while an incremental refresh was pending, it was absorbed
  without forcing a full refresh — now fixed.

# v0.0.8

- **expanded templating**: many new template variables available —
  `files_modified`, `files_added`, `files_deleted` (and staged
  equivalents), `commit_id_prefix`/`commit_id_rest`,
  `change_id_prefix`/`change_id_rest`, `is_stale`, `refresh_error`.
  New `italic` and `underline` ANSI helpers in templates.
- **colorized ID prefixes for jj**: change and commit IDs now highlight
  their shortest unique prefix (bold magenta for change IDs, bold blue
  for commit IDs), matching jj's default styling.
- **new "simple" template**: a middle ground between "minimal" (formerly
  "simple") and the full "ascii" template. The old "simple" template has
  been renamed to "minimal".
- **shared detail.tera**: ascii, nerdfont, and unicode templates now
  share a common detail template, making them easier to customize.
- **hot-reload config**: the daemon watches its config file and
  hot-reloads on valid changes. `config set` also triggers a reload.
- **staleness indicator**: when a refresh fails, cached output is marked
  stale via `is_stale` and `refresh_error` template variables.
- **daemon self-shutdown on socket removal**: the daemon exits cleanly
  if its Unix socket is deleted.
- **refuse to run as root**: the daemon refuses to start as root unless
  `--allow-root` is passed or `VCS_STATUS_DAEMON_DIR` is set.
- **version mismatch warning**: the client warns if the running daemon
  is a different version when an error occurs.
- **show version in status output**: `status` subcommand now includes
  the daemon version.
- **fix immutable heads detection** for jj repos.

# v0.0.7

- switch to using libgit2 and jj-lib instead of subprocess calls
- run diffs on individual files we're notified for instead of using
  built-in vcs diff tools to reduce total checks
- attempt to wait a configurable timeout (default 150ms) if there is
  a status update in-flight instead of immediately returning the
  cached value
- never snapshot with jj
- add status and --version commands and flags
- [debugging] add a way to build with tokio-console

# v0.0.6

- **tera-based templates**: templates moved to `.tera` files with color
  filters, replacing the old inline format strings.
- **more built-in templates**: added `unicode` and `simple` presets,
  plus a `template list` command to see available templates.
- **`config set` command**: change config values from the CLI
  (e.g. `vcs-status-daemon config set template_name nerdfont`).
- **rebasing status**: detect and display when a repo is mid-rebase.
- **worktree/workspace support**: handle jj workspaces and git worktrees.
- **client caching removed**: cache reads moved to shell integration,
  client no longer caches independently.
- **shorter internal timeouts** for snappier responses.

# v0.0.5

- **shell init commands**: `vcs-status-daemon init bash|zsh|fish` for
  easy shell prompt integration (outputs eval-able script).
- **runtime directory**: switched from a bare socket path to a runtime
  directory (`/tmp/vcs-status-daemon-$USER/`) containing socket, cache,
  and log files. Configurable via `$VCS_STATUS_DAEMON_DIR`.
- **gitignore-aware file watching**: watcher loads `.gitignore`/`.jjignore`
  rules to skip ignored paths, reducing steady-state CPU usage.
- **log rotation**: daemon log is capped at 5 MB.
- **template validation**: templates are validated on startup with
  warnings for errors.
- **watcher self-cleanup**: periodic sweep removes watchers for deleted
  repo directories.
- **`restart` subcommand**: stop and re-launch the daemon in one command.
- **`--use-cache` flag**: force the client to interact with the daemon
  rather than reading cache files directly.
- **`status` subcommand**: inspect the running daemon's state.

# v0.0.4

- **file-based cache**: daemon writes status to cache files that the
  client reads directly, avoiding a subprocess round-trip for the common
  case.
- **synchronous client**: client no longer uses tokio — pure synchronous
  I/O for minimal startup overhead.
- **shell environment variable support**: `eval`-friendly output for
  shell integration.

# v0.0.3

- **git support**: handle git repos in addition to jj.
- **named preset templates**: choose between built-in format presets
  (ascii, nerdfont).
- **renamed to vcs-status-daemon** (from jj-status-daemon).
- deduplicate jj bookmarks by name.

# v0.0.2

- fix: homebrew release packaging.

# v0.0.1

- initial release: background daemon that watches jj repos and caches
  formatted status for shell prompts.
- colorized output with ANSI escape codes.
- directory traversal to find repo root from subdirectories.
- Unix socket protocol for client-daemon communication.
