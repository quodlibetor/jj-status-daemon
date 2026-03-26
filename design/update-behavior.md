# Binary update behavior

## Problem

When the daemon binary is updated (e.g. via `self-update`, package manager, or `cargo install`), the running daemon is still the old binary. Users have to manually run `vcs-status-daemon restart` or wait for the idle timeout to cycle it. This is easy to forget and leads to version mismatch warnings.

## Design

The daemon watches its own binary for replacement and automatically exec's the new version in-place.

### Binary watcher

On startup, the daemon resolves its binary path via `std::env::current_exe()` (canonicalized to follow symlinks) and records the file's inode. It then watches the binary's parent directory with `notify` for modify/create/remove events affecting that path. It also periodically checks (every 30s) that the binary still exists on disk.

When a modify/create event fires:
1. Wait 500ms for the write/rename to finish.
2. Stat the file at the original path and compare inodes.
3. If the inode changed, the binary was replaced — trigger exec-restart.

When the binary is deleted (remove event or periodic existence check):
1. The daemon shuts down cleanly (no exec-restart — there's nothing to re-exec).
2. The next client query auto-starts the new daemon from whatever binary is now in PATH.

This handles two update patterns:
- **In-place replacement** (e.g. `cargo install`, `self-update`): new file at the same path, different inode → exec-restart.
- **Path relocation** (e.g. `mise`, `nix`, `asdf`): old binary deleted, new binary at a different path → clean shutdown, client auto-starts new version.

The periodic existence check (every 30s) is a fallback for cases where the filesystem watcher itself fails, e.g. when the parent directory is deleted before the file remove event is delivered.

### Restart via exec

When the binary watcher fires, the daemon does **not** go through normal shutdown cleanup (removing socket, cache, pid files). Instead it:

1. Calls `maybe_clean_runtime_dir` (see below).
2. Calls `exec()` with the same arguments (`std::env::args()`).

`exec()` replaces the current process in-place — same PID, no fork. The new daemon starts fresh: it detects the stale socket (can't connect to it since the old listener is gone), removes it, and binds a new one. The pid file is already correct since the PID didn't change.

Normal shutdown cleanup only runs as a fallback if `exec()` fails.

### Directory version

A monotonically increasing constant `DIRECTORY_VERSION` (in `daemon.rs`) tracks the runtime directory layout. When the layout changes in a breaking way (new cache format, renamed files, etc.), this constant is bumped.

The hidden subcommand `vcs-status-daemon directory-version` prints this number.

During restart, before exec'ing the new binary, the daemon runs `<new-binary> directory-version` and compares the result to its own `DIRECTORY_VERSION`. If the new version is greater, it removes everything in the runtime directory except `daemon.log*` files. This gives the new daemon a clean slate without losing diagnostic logs.

### What happens to in-flight requests

The `exec()` call closes all file descriptors (tokio sets `CLOEXEC`). Any in-flight client connections get a broken pipe. This is acceptable because:
- Clients already handle connection failures gracefully (the shell prompt shows nothing rather than erroring).
- The new daemon is available within milliseconds.
- This is an infrequent event (only on binary updates).

## Relationship to existing update mechanisms

- **`self-update` subcommand**: Runs `axoupdater`, then shuts down the daemon. The next client query auto-starts the new binary. With the binary watcher, the restart happens automatically without waiting for a client query.
- **`restart` subcommand**: Manual graceful shutdown + start. Still works as before, useful for forcing a restart regardless of binary changes.
- **Version mismatch auto-shutdown**: The client checks the `version` file and, if it doesn't match, sends a `Shutdown` request to the stale daemon. The next client query (i.e. next shell prompt) auto-starts the new version. This acts as a safety net for update patterns that the binary watcher doesn't catch.
