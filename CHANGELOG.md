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
