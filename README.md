# jj-status-daemon

A background daemon that pre-caches [Jujutsu](https://github.com/jj-vcs/jj) and Git repository status, so shell prompts can retrieve it in milliseconds instead of waiting for `jj` or `git` to run on every prompt.

> This project is entirely AI generated but it seems to work

## Problem

Jujutsu can be slow in large repositories. Shell prompt integrations (like starship-jj) that call `jj` on every prompt add noticeable latency. This daemon watches for repository changes via filesystem notifications and keeps a formatted status string in memory, ready to serve instantly.

It also supports plain Git repositories, so you can use a single tool for both.

## Architecture

```
Shell prompt calls:   jj-status-daemon         (client mode, the default)
                          |
                          | connects to Unix domain socket
                          v
                      jj-status-daemon daemon   (background server)
                          |
                          +-- detects VCS type (jj wins if both .jj/ and .git/ exist)
                          +-- watches repo via filesystem notifications (notify)
                          +-- on change: shells out to jj or git, caches formatted status
                          +-- serves cached text to clients instantly
```

- **Single binary, two modes**: `daemon` (background server) and default (client/query)
- **Auto-start**: the client spawns the daemon automatically if it's not running
- **Multi-repo**: the daemon tracks multiple repositories, each with its own filesystem watcher
- **Dual VCS**: supports both jj and git repositories, with jj taking priority when both are present
- **Idle shutdown**: the daemon exits automatically after 1 hour (configurable) with no queries

## Installation

Requires Rust and a working `jj` and/or `git` CLI installation.

```sh
cargo install --path .
```

## Usage

### Shell prompt integration

Add to your shell prompt (e.g. in `.zshrc` or `.bashrc`):

```sh
# Exits silently with no output when not in a jj/git repo
export PS1='$(jj-status-daemon) $ '
```

Or with starship, in `starship.toml`:

```toml
[custom.jj]
command = "jj-status-daemon"
when = true
```

### Commands

```sh
# Query status for the current directory (default, auto-starts daemon)
jj-status-daemon

# Query a specific path
jj-status-daemon query --repo /path/to/repo

# Start the daemon explicitly
jj-status-daemon daemon

# Start the daemon with a specific socket path
jj-status-daemon daemon --socket /tmp/my-custom.sock

# Shut down the daemon
jj-status-daemon shutdown
```

The client sends its current directory to the daemon, which walks up the directory tree to find a repo root (`.jj/` or `.git/`). The mapping from directory to repo root is cached. When run outside a recognized repository, the client exits silently with exit code 0, making it safe for unconditional prompt use.

### Socket path

Both client and daemon resolve the Unix socket path using:

1. `JJ_STATUS_DAEMON_SOCKET_PATH` environment variable (if set)
2. Default: `/tmp/jj-status-daemon-$USER.sock`

The daemon also accepts a `--socket` CLI flag, which takes priority over the environment variable. When the client auto-starts the daemon, it always passes its resolved socket path via `--socket` to ensure both sides agree.

## Configuration

Configuration is loaded from `~/.config/jj-status-daemon/config.toml`. All fields are optional and have sensible defaults.

```toml
# How long the daemon stays alive with no queries (seconds, default: 3600)
idle_timeout_secs = 3600

# Debounce delay for filesystem events before refreshing (ms, default: 200)
debounce_ms = 200

# How many ancestor commits to search for bookmarks in jj repos (default: 10)
bookmark_search_depth = 10

# Enable ANSI color output (default: true)
color = true

# Status format template (Tera syntax, see below)
# format = "..."
```

## Format template

The `format` field is a [Tera](https://keats.github.io/tera/docs/) template string. Tera uses `{{ variable }}` for interpolation, `{% if %}` / `{% elif %}` / `{% endif %}` for conditionals, and `{% for x in list %}` / `{% endfor %}` for loops.

### VCS type detection

The daemon detects whether a repository uses jj or git (jj wins if both `.jj/` and `.git/` are present) and exposes `is_jj` and `is_git` booleans. Use these to write templates that work for both:

```tera
{% if is_jj %}{{ change_id }}{% elif is_git %}{{ branch }} {{ commit_id }}{% endif %}
```

### Template variables

#### VCS type

| Variable | Type | Description |
|---|---|---|
| `is_jj` | bool | `true` if the repo is a jj repository. |
| `is_git` | bool | `true` if the repo is a plain git repository (no `.jj/`). |

#### Shared fields (both jj and git)

| Variable | Type | Description |
|---|---|---|
| `commit_id` | string | Short commit ID. For jj repos with `color = true`, includes jj's native ANSI coloring. |
| `description` | string | First line of the commit description (jj) or commit message summary (git). |
| `empty` | bool | `true` if the working commit (jj) or HEAD commit (git) has no changes. |
| `conflict` | bool | `true` if there are conflicts (jj conflict markers or git merge conflicts). |

#### Diff stats

There are three groups of diff stat variables. For jj repos (which have no staging area), `files_changed` and `total_files_changed` are identical, and `staged_*` is always 0. For git repos, all three groups are independently populated.

| Variable | Type | jj | git |
|---|---|---|---|
| `files_changed` | integer | Files changed in `@` vs parent | Unstaged: working tree vs index |
| `lines_added` | integer | Lines added in `@` | Unstaged lines added |
| `lines_removed` | integer | Lines removed in `@` | Unstaged lines removed |
| `staged_files_changed` | integer | Always 0 | Staged: index vs HEAD |
| `staged_lines_added` | integer | Always 0 | Staged lines added |
| `staged_lines_removed` | integer | Always 0 | Staged lines removed |
| `total_files_changed` | integer | Same as `files_changed` | Total: working tree vs HEAD |
| `total_lines_added` | integer | Same as `lines_added` | Total lines added |
| `total_lines_removed` | integer | Same as `lines_removed` | Total lines removed |

The default template uses `total_*` since it gives the complete picture for both VCS types.

#### jj-only fields

These are populated only for jj repositories. In git repositories they are empty/false/zero.

| Variable | Type | Description |
|---|---|---|
| `change_id` | string | Short change ID (8 chars). With `color = true`, includes jj's native ANSI coloring. |
| `bookmarks` | list | List of bookmark objects (see below). |
| `has_bookmarks` | bool | `true` if any bookmarks were found in the ancestor search range. |
| `divergent` | bool | `true` if the working commit is divergent. |
| `hidden` | bool | `true` if the working commit is hidden. |
| `immutable` | bool | `true` if the working commit is immutable. |


#### git-only fields

These are populated only for git repositories. In jj repositories they are empty/false.

| Variable | Type | Description |
|---|---|---|
| `branch` | string | Current branch name, or short commit hash if HEAD is detached. |
| `has_branch` | bool | `true` if `branch` is non-empty. |


#### Bookmark objects (jj only)

Each item in the `bookmarks` list has:

| Field | Type | Description |
|---|---|---|
| `name` | string | Bookmark name, e.g. `"main"`. |
| `distance` | integer | Number of commits between `@` and the bookmarked commit. `0` means the bookmark is on `@`. |
| `display` | string | Pre-formatted display string: `"main"` when distance is 0, `"main+2"` otherwise. |


#### Color codes

These resolve to ANSI escape sequences when `color = true` and to empty strings when `color = false`, so templates work correctly in both modes.

| Variable | ANSI code | Appearance |
|---|---|---|
| `RST` | `\e[0m` | Reset all formatting |
| `BOLD` | `\e[1m` | Bold |
| `DIM` | `\e[2m` | Dim |
| `BLACK` | `\e[30m` | Black |
| `RED` | `\e[31m` | Red |
| `GREEN` | `\e[32m` | Green |
| `YELLOW` | `\e[33m` | Yellow |
| `BLUE` | `\e[34m` | Blue (dark) |
| `MAGENTA` | `\e[35m` | Magenta |
| `CYAN` | `\e[36m` | Cyan |
| `WHITE` | `\e[37m` | White |
| `BRIGHT_BLACK` | `\e[90m` | Bright black (gray) |
| `BRIGHT_RED` | `\e[91m` | Bright red |
| `BRIGHT_GREEN` | `\e[92m` | Bright green |
| `BRIGHT_YELLOW` | `\e[93m` | Bright yellow |
| `BRIGHT_BLUE` | `\e[94m` | Bright blue |
| `BRIGHT_MAGENTA` | `\e[95m` | Bright magenta |
| `BRIGHT_CYAN` | `\e[96m` | Bright cyan |
| `BRIGHT_WHITE` | `\e[97m` | Bright white |


### Default template

The built-in default template handles both jj and git repos:

- **jj**: `xlvlt main [3 +10-5]` or `xlvlt (EMPTY)`
- **git**: `main abc1234 [3 +10-5]` or `main abc1234 (EMPTY)`

```tera
{% if is_jj %}{{ change_id }}
{%- for b in bookmarks %} {{ BLUE }}{{ b.display }}{{ RST }}{% endfor %}
{%- elif is_git %}{{ BLUE }}{{ branch }}{{ RST }} {{ commit_id }}
{%- endif %}
{%- if total_files_changed > 0 %} {{ BLUE }}[{{ RST }}{{ BRIGHT_BLUE }}{{ total_files_changed }}{{ RST }} {{ BRIGHT_GREEN }}+{{ total_lines_added }}{{ RST }}{{ BRIGHT_RED }}-{{ total_lines_removed }}{{ RST }}{{ BLUE }}]{{ RST }}{% endif %}
{%- if conflict %} {{ BRIGHT_RED }}CONFLICT{{ RST }}{% endif %}
{%- if divergent %} {{ BRIGHT_RED }}DIVERGENT{{ RST }}{% endif %}
{%- if hidden %} {{ BRIGHT_YELLOW }}HIDDEN{{ RST }}{% endif %}
{%- if immutable %} {{ YELLOW }}IMMUTABLE{{ RST }}{% endif %}
{%- if empty %} {{ BLUE }}({{ RST }}EMPTY{{ BLUE }}){{ RST }}{% endif %}
```

In the TOML config file, use multi-line literal strings (`'''`) for readability. Use Tera's `{%-` whitespace trimming to prevent newlines from appearing in the output:

```toml
format = '''
{% if is_jj %}{{ change_id }}
{%- for b in bookmarks %} {{ BLUE }}{{ b.display }}{{ RST }}{% endfor %}
{%- elif is_git %}{{ BLUE }}{{ branch }}{{ RST }} {{ commit_id }}
{%- endif %}
{%- if total_files_changed > 0 %} {{ BLUE }}[{{ RST }}{{ BRIGHT_BLUE }}{{ total_files_changed }}{{ RST }} {{ BRIGHT_GREEN }}+{{ total_lines_added }}{{ RST }}{{ BRIGHT_RED }}-{{ total_lines_removed }}{{ RST }}{{ BLUE }}]{{ RST }}{% endif %}
{%- if conflict %} {{ BRIGHT_RED }}CONFLICT{{ RST }}{% endif %}
{%- if divergent %} {{ BRIGHT_RED }}DIVERGENT{{ RST }}{% endif %}
{%- if hidden %} {{ BRIGHT_YELLOW }}HIDDEN{{ RST }}{% endif %}
{%- if immutable %} {{ YELLOW }}IMMUTABLE{{ RST }}{% endif %}
{%- if empty %} {{ BLUE }}({{ RST }}EMPTY{{ BLUE }}){{ RST }}{% endif %}'''
```

### Custom template examples

**jj-only, minimal** -- just change ID and bookmarks, no color:

```toml
color = false
format = '''
{{ change_id }}
{%- for b in bookmarks %} {{ b.display }}{% endfor %}'''
```

**git-only, minimal** -- just branch and commit:

```toml
color = false
format = "{{ branch }} {{ commit_id }}"
```

**Verbose, both VCS** -- with description/state details:

```toml
format = '''
{% if is_jj %}{{ change_id }} {{ BRIGHT_BLACK }}{{ commit_id }}{{ RST }}
{%- for b in bookmarks %} {{ BLUE }}{{ b.display }}{{ RST }}{% endfor %}
{%- if description %} {{ DIM }}{{ description }}{{ RST }}{% endif %}
{%- elif is_git %}{{ BLUE }}{{ branch }}{{ RST }} {{ BRIGHT_BLACK }}{{ commit_id }}{{ RST }}
{%- endif %}
{%- if total_files_changed > 0 %} {{ BRIGHT_BLUE }}{{ total_files_changed }}f{{ RST }} {{ BRIGHT_GREEN }}+{{ total_lines_added }}{{ RST }} {{ BRIGHT_RED }}-{{ total_lines_removed }}{{ RST }}{% endif %}
{%- if empty %} {{ YELLOW }}empty{{ RST }}{% endif %}
{%- if conflict %} {{ BRIGHT_RED }}conflict!{{ RST }}{% endif %}'''
```

**Custom bookmark formatting** -- show distance differently (jj only):

```toml
format = '''
{{ change_id }}
{%- for b in bookmarks %} {{ CYAN }}{{ b.name }}{% if b.distance > 0 %}~{{ b.distance }}{% endif %}{{ RST }}{% endfor %}
{%- if empty %} {{ DIM }}empty{{ RST }}{% endif %}'''
```

## How it works

1. **Client** connects to the daemon's Unix domain socket. If the daemon isn't running, the client spawns it as a detached background process and retries.

2. **Daemon** receives a query with a directory path and walks up to find the repo root. It detects the VCS type (`.jj/` takes priority over `.git/`). On first query for a repo, it:
   - Sets up a filesystem watcher appropriate for the VCS type:
     - **jj**: watches `.jj/repo/op_heads/heads/` (operations) and the working directory
     - **git**: watches `.git/` and `.git/refs/` (ref changes) and the working directory
   - Shells out to `jj` or `git` to gather status info (commit, branch/bookmarks, diff stats)
   - Renders the format template and caches the result

3. **On filesystem changes**, the daemon debounces events (200ms default), then re-runs the appropriate VCS command and updates the cache. For jj repos, it uses `--ignore-working-copy` when only `.jj/` internal files changed (faster, avoids unnecessary snapshots), and omits it when working copy files changed (so `jj` snapshots first, giving accurate diff stats).

4. **Subsequent queries** return the cached string instantly.

## Development

```sh
# Run tests (requires jj and git to be installed)
cargo nextest run

# Or with plain cargo
cargo test

# Build
cargo build --release
```
