# jj-status-daemon

A background daemon that pre-caches [Jujutsu](https://github.com/jj-vcs/jj) repository status, so shell prompts can retrieve it in milliseconds instead of waiting for `jj` to run on every prompt.

## Problem

Jujutsu can be slow in large repositories. Shell prompt integrations (like starship-jj) that call `jj` on every prompt add noticeable latency. This daemon watches for repository changes via filesystem notifications and keeps a formatted status string in memory, ready to serve instantly.

## Architecture

```
Shell prompt calls:   jj-status-daemon         (client mode, the default)
                          |
                          | connects to Unix domain socket
                          v
                      jj-status-daemon daemon   (background server)
                          |
                          +-- watches .jj/repo/op_heads/ and working directory via notify
                          +-- on change: shells out to jj, caches formatted status text
                          +-- serves cached text to clients instantly
```

- **Single binary, two modes**: `daemon` (background server) and default (client/query)
- **Auto-start**: the client spawns the daemon automatically if it's not running
- **Multi-repo**: the daemon tracks multiple repositories, each with its own filesystem watcher
- **Idle shutdown**: the daemon exits automatically after 1 hour (configurable) with no queries

## Installation

Requires Rust and a working `jj` CLI installation.

```sh
cargo install --path .
```

## Usage

### Shell prompt integration

Add to your shell prompt (e.g. in `.zshrc` or `.bashrc`):

```sh
# Exits silently with no output when not in a jj repo
export PS1='$(jj-status-daemon) $ '
```

Or with starship, in `starship.toml`:

```toml
[custom.jj]
command = "jj-status-daemon"
when = "test -d .jj"
```

### Commands

```sh
# Query status for the current repo (default, auto-starts daemon)
jj-status-daemon

# Query a specific repo
jj-status-daemon query --repo /path/to/repo

# Start the daemon explicitly
jj-status-daemon daemon

# Shut down the daemon
jj-status-daemon shutdown
```

When run outside a jj repository (and no `--repo` flag is passed), the client exits silently with exit code 0, making it safe for unconditional prompt use.

## Configuration

Configuration is loaded from `~/.config/jj-status-daemon/config.toml`. All fields are optional and have sensible defaults.

```toml
# How long the daemon stays alive with no queries (seconds, default: 3600)
idle_timeout_secs = 3600

# Debounce delay for filesystem events before refreshing (ms, default: 200)
debounce_ms = 200

# How many ancestor commits to search for bookmarks (default: 10)
bookmark_search_depth = 10

# Enable ANSI color output (default: true)
color = true

# Custom Unix socket path (default: /tmp/jj-status-daemon-$USER.sock)
# socket_path = "/tmp/my-custom.sock"

# Status format template (Tera syntax, see below)
# format = "..."
```

## Format template

The `format` field is a [Tera](https://keats.github.io/tera/docs/) template string. Tera uses `{{ variable }}` for interpolation and `{% if %}` / `{% endif %}` for conditionals.

### Template variables

#### Status data


| Variable | Type | Description |
|---|---|---|
| `change_id` | string | Short change ID (8 chars). When `color = true`, includes jj's native ANSI coloring (bold unique prefix, gray rest). |
| `commit_id` | string | Short commit ID (8 chars). Same coloring behavior as `change_id`. |
| `description` | string | First line of the commit description. |
| `bookmarks` | list | List of bookmark objects (see below). Iterate with `{% for b in bookmarks %}`. |
| `has_bookmarks` | bool | `true` if any bookmarks were found in the ancestor search range. |
| `files_changed` | integer | Number of files changed in the working commit. |
| `lines_added` | integer | Number of lines added in the working commit. |
| `lines_removed` | integer | Number of lines removed in the working commit. |
| `empty` | bool | `true` if the working commit has no file changes. |
| `conflict` | bool | `true` if the working commit has conflicts. |
| `divergent` | bool | `true` if the working commit is divergent. |
| `hidden` | bool | `true` if the working commit is hidden. |
| `immutable` | bool | `true` if the working commit is immutable. |

#### Bookmark objects

Each item in the `bookmarks` list has:

| Field | Type | Description |
|---|---|---|
| `name` | string | Bookmark name, e.g. `"main"`. |
| `distance` | integer | Number of commits between `@` and the bookmarked commit. `0` means the bookmark is on `@`. |
| `display` | string | Pre-formatted display string: `"main"` when distance is 0, `"main+2"` otherwise. |

Example usage in a template:

```tera
{% for b in bookmarks %} {{ BLUE }}{{ b.display }}{{ RST }}{% endfor %}
```

You can also use the individual fields for custom formatting:

```tera
{% for b in bookmarks %} {{ b.name }}{% if b.distance > 0 %}({{ b.distance }} away){% endif %}{% endfor %}
```


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

The built-in default template produces output like `xlvlt main [3 +10-5]` or `xlvlt (EMPTY)`:

```tera
{{ change_id }}
{%- for b in bookmarks %} {{ BLUE }}{{ b.display }}{{ RST }}{% endfor %}
{%- if files_changed > 0 %} {{ BLUE }}[{{ RST }}{{ BRIGHT_BLUE }}{{ files_changed }}{{ RST }} {{ BRIGHT_GREEN }}+{{ lines_added }}{{ RST }}{{ BRIGHT_RED }}-{{ lines_removed }}{{ RST }}{{ BLUE }}]{{ RST }}{% endif %}
{%- if conflict %} {{ BRIGHT_RED }}CONFLICT{{ RST }}{% endif %}
{%- if divergent %} {{ BRIGHT_RED }}DIVERGENT{{ RST }}{% endif %}
{%- if hidden %} {{ BRIGHT_YELLOW }}HIDDEN{{ RST }}{% endif %}
{%- if immutable %} {{ YELLOW }}IMMUTABLE{{ RST }}{% endif %}
{%- if empty %} {{ BLUE }}({{ RST }}EMPTY{{ BLUE }}){{ RST }}{% endif %}
```

In the TOML config file, use multi-line literal strings (`'''`) for readability. Use Tera's `{%-` whitespace trimming to prevent newlines from appearing in the output:

```toml
format = '''
{{ change_id }}
{%- for b in bookmarks %} {{ BLUE }}{{ b.display }}{{ RST }}{% endfor %}
{%- if files_changed > 0 %} {{ BLUE }}[{{ RST }}{{ BRIGHT_BLUE }}{{ files_changed }}{{ RST }} {{ BRIGHT_GREEN }}+{{ lines_added }}{{ RST }}{{ BRIGHT_RED }}-{{ lines_removed }}{{ RST }}{{ BLUE }}]{{ RST }}{% endif %}
{%- if conflict %} {{ BRIGHT_RED }}CONFLICT{{ RST }}{% endif %}
{%- if divergent %} {{ BRIGHT_RED }}DIVERGENT{{ RST }}{% endif %}
{%- if hidden %} {{ BRIGHT_YELLOW }}HIDDEN{{ RST }}{% endif %}
{%- if immutable %} {{ YELLOW }}IMMUTABLE{{ RST }}{% endif %}
{%- if empty %} {{ BLUE }}({{ RST }}EMPTY{{ BLUE }}){{ RST }}{% endif %}'''
```

### Custom template examples

**Minimal** -- just change ID and bookmarks, no color:

```toml
color = false
format = '''
{{ change_id }}
{%- for b in bookmarks %} {{ b.display }}{% endfor %}'''
```

**Verbose** -- with commit ID, description, and bookmark distance annotation:

```toml
format = '''
{{ change_id }} {{ BRIGHT_BLACK }}{{ commit_id }}{{ RST }}
{%- for b in bookmarks %} {{ BLUE }}{{ b.display }}{{ RST }}{% endfor %}
{%- if description %} {{ DIM }}{{ description }}{{ RST }}{% endif %}
{%- if files_changed > 0 %} {{ BRIGHT_BLUE }}{{ files_changed }}f{{ RST }} {{ BRIGHT_GREEN }}+{{ lines_added }}{{ RST }} {{ BRIGHT_RED }}-{{ lines_removed }}{{ RST }}{% endif %}
{%- if empty %} {{ YELLOW }}empty{{ RST }}{% endif %}
{%- if conflict %} {{ BRIGHT_RED }}conflict!{{ RST }}{% endif %}'''
```

**Custom bookmark formatting** -- show distance differently:

```toml
format = '''
{{ change_id }}
{%- for b in bookmarks %} {{ CYAN }}{{ b.name }}{% if b.distance > 0 %}~{{ b.distance }}{% endif %}{{ RST }}{% endfor %}
{%- if empty %} {{ DIM }}empty{{ RST }}{% endif %}'''
```

## How it works

1. **Client** connects to the daemon's Unix domain socket. If the daemon isn't running, the client spawns it as a detached background process and retries.

2. **Daemon** receives a query with a repo path. On first query for a repo, it:
   - Sets up a filesystem watcher on `.jj/repo/op_heads/heads/` (jj operations) and the repo working directory (file edits)
   - Runs three `jj` commands concurrently to gather commit info, bookmarks, and diff stats
   - Renders the format template and caches the result

3. **On filesystem changes**, the daemon debounces events (200ms default), then re-runs `jj` and updates the cache. It uses `--ignore-working-copy` when only `.jj/` internal files changed (faster, avoids unnecessary snapshots), and omits it when working copy files changed (so `jj` snapshots first, giving accurate diff stats).

4. **Subsequent queries** return the cached string instantly.

## Development

```sh
# Run tests (requires jj to be installed)
cargo test

# Build
cargo build --release
```
