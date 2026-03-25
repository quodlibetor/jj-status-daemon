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
