# Binary update behavior

## Problem

When the daemon binary is updated (e.g. via `self-update`, package manager, or `cargo install`), the running daemon is still the old binary. Users have to manually run `vcs-status-daemon restart` or wait for the idle timeout to cycle it. This is easy to forget and leads to version mismatch warnings.

## Design

The daemon watches its own binary for replacement and automatically exec's the new version in-place.

### Binary watcher

On startup, the daemon resolves its binary path via `std::env::current_exe()` (canonicalized to follow symlinks) and records the file's inode. It then watches the binary's parent directory with `notify` for modify/create events affecting that path.

When an event fires:
1. Wait 500ms for the write/rename to finish.
2. Stat the file at the original path and compare inodes.
3. If the inode changed, the binary was replaced — trigger restart.

Inode comparison is the key mechanism. A simple file touch or in-place write (same inode) is ignored. Only a genuine replacement (new file, different inode) triggers a restart. This matches how installers, package managers, and `cargo install` work: they write a new file and atomically rename it over the old path.

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
- **Version mismatch warning**: The client checks the `version` file and warns once if it doesn't match. With auto-restart, this warning should rarely appear.
