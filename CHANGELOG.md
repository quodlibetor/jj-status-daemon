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
