# Staleness handling

## What staleness means

When the daemon refreshes a repo's VCS status (via `jj-lib` or `git2`), the operation can fail — corrupted repo state, locked files, permission errors, etc. Before staleness handling, these errors were logged and silently swallowed: the cache kept showing the last successful status with no indication that it was outdated. The user's prompt would look normal even though the underlying data might be minutes or hours old.

Staleness handling fixes this by re-rendering the cached status with a visible indicator when a refresh fails, so the user knows the displayed status may not reflect reality.

## How it works

### Data model

`RepoStatus` has two fields for staleness:

```rust
pub is_stale: bool,       // true when the last refresh attempt failed
pub refresh_error: String, // the error message from the failed refresh
```

Both default to `false`/empty. They are set only in the daemon's `refresh_repo` error path — never by VCS query code, client code, or connection handling.

### Daemon flow

1. A file-system event passes through the ignore filter and triggers `refresh_repo` for a watched repo.
2. **Immediately** (before querying VCS): If there is existing cached data, the daemon clones it, sets `is_stale = true` with an empty `refresh_error`, re-renders, and writes to both in-memory and on-disk caches. The prompt shows the stale indicator right away, signaling that a refresh is in progress.
3. The daemon queries VCS status via `jj-lib` or `git2` (on dedicated blocking threads).
4. **On success**: The cache entry is replaced with a fresh `RepoStatus` where `is_stale` is false. The stale indicator disappears.
5. **On error, with existing cache**: The daemon clones the (already-stale) `RepoStatus`, keeps `is_stale = true`, and sets `refresh_error` to the error string. The stale indicator persists, now with an error attached.
6. **On error, with no existing cache** (first query for a repo failed in the background spawn): There is no previous status to mark stale, so the error is logged and the client gets a "not ready" response on its next query. No stale indicator is shown because there is nothing to show.

This means staleness has two phases: a transient "refreshing" phase (stale, no error) that resolves in milliseconds during normal operation, and a persistent "error" phase (stale, with error) that requires user intervention.

### Cache structure

The in-memory cache stores `(RepoStatus, String)` tuples rather than just the formatted string. This is necessary so the error path can access the previous `RepoStatus` to clone and modify it. The on-disk cache files still contain only the rendered string (what the client's fast-path reads directly).

### What does NOT set staleness

- **Client connection errors** (`handle_connection`): A client disconnecting, sending malformed JSON, or timing out is a connection problem, not a data problem. These errors propagate via `?` and are logged as connection warnings. The cache is not touched.
- **VCS query code** (`jj.rs`, `git.rs`): These return `Result<RepoStatus>`. Errors propagate up to `refresh_repo`, which decides what to do. The query functions have no knowledge of staleness.
- **The client** (`client.rs`): Fully synchronous, reads formatted strings. Unaware of `RepoStatus` or staleness fields.

### Self-healing

Staleness is not permanent. The next successful refresh overwrites the cache entry with a fresh `RepoStatus` where `is_stale` is false, so the stale indicator disappears automatically. Common triggers for a successful refresh:

- The user fixes the underlying problem (e.g., repairs a corrupted repo).
- A transient error resolves itself (e.g., a locked file is released).
- The user saves a file, triggering a new file-system event and refresh cycle.

## Template indicators

Each built-in template (except `simple`) shows a stale indicator:

| Template  | Indicator | Meaning |
|-----------|-----------|---------|
| ascii     | `STALE`   | Plain text, yellow |
| nerdfont  | `󰇘`       | Nerd Font ellipsis icon, yellow |
| unicode   | `⟳`       | Clockwise arrow (refresh symbol), yellow |
| simple    | (none)    | Too minimal for extra indicators |

Custom templates can use `{% if is_stale %}` and `{{ refresh_error }}` to build their own indicators. The `refresh_error` variable contains the raw error message, which can be useful for debugging but is typically too long for a prompt.

## User guide: resolving staleness

If your prompt shows a stale indicator, it means the daemon tried to refresh the repo's status and failed. The displayed status is from the last successful refresh and may be outdated.

### Common causes and fixes

**Library-level failure** — The most common cause. The `jj-lib` or `git2` library call failed internally (e.g., corrupt index, invalid object, lock contention). Run `jj status` or `git status` manually in the repo to see a comparable error. Fix whatever it reports, and the next automatic refresh will clear the stale indicator.

**Locked repository** — Another process (IDE, another `jj` or `git` invocation) holds a lock on the repo. This is typically transient — the lock releases when the other process finishes, and the next refresh succeeds automatically.

**Permission errors** — The daemon runs as your user. If repo file permissions changed (e.g., after a `sudo` operation), fix them and the next refresh will succeed.

**Corrupted repo** — Run `jj debug local-working-copy --repair` (for jj) or `git fsck` (for git) to diagnose and fix.

### Quick fixes

- **Force a refresh**: Save any file in the repo (triggers a file-system event) or run `vcs-status-daemon restart`.
- **Check the daemon log**: `cat /tmp/vcs-status-daemon-$USER/daemon.log | grep "refresh failed"` to see the actual error.
- **Restart the daemon**: `vcs-status-daemon restart` clears all cached state and starts fresh.
