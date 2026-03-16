# vcs-status-daemon -- VCS status line daemon

This is to be a tool that makes it possible to get jujutsu status information in milliseconds as part of a shell prompt.

Jujutsu can be extremely slow in large repositories, so this will need to work
by spawning a daemon which listens for file changes in a repo, and a client
that can will auto-start the daemon and notify it of the current repo if one
isn't running.

The daemon's configuration should make it possible to configure similar to the
starship prompt (e.g. as I have it configured in
/Users/bwm/.local/share/chezmoi/dot_config/starship.toml.tmpl).

The daemon should communicate with the client in json, but the primary field
that the server should send should be pre-formatted text, and that text should
be sitting in memory due to file system notifications causing the daemon to
self-update by the time the client asks for it when possible.

you can take inspiration from starship-jj (which I have cloned in
~/repos/starship-jj), but this first iteration should be as simple as possible
-- it's reasonable for the daemon to shell out to jujutsu instead of compiling
it in if that will save a couple hundred lines of code.
