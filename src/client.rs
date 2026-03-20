use anyhow::{Context, Result};
use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::config;
use crate::protocol::{Request, Response};

fn send_request(socket_path: &Path, request: &Request) -> Result<Response> {
    send_request_with_timeout(socket_path, request, Duration::from_millis(100))
}

fn send_request_slow(socket_path: &Path, request: &Request) -> Result<Response> {
    send_request_with_timeout(socket_path, request, Duration::from_secs(5))
}

fn send_request_with_timeout(
    socket_path: &Path,
    request: &Request,
    timeout: Duration,
) -> Result<Response> {
    let stream = UnixStream::connect(socket_path).context("failed to connect to daemon")?;
    stream.set_read_timeout(Some(timeout)).ok();
    let mut writer = std::io::BufWriter::new(&stream);
    let mut json = serde_json::to_string(request)?;
    json.push('\n');
    writer.write_all(json.as_bytes())?;
    writer.flush()?;

    let mut reader = std::io::BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let response: Response = serde_json::from_str(line.trim())?;
    Ok(response)
}

fn start_daemon(socket_path: &Path, config_file: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe().context("failed to get current exe")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "--dir"]);
    cmd.arg(socket_path.parent().unwrap_or(socket_path));

    // Forward config file to the daemon so it uses the same config
    if let Some(cf) = config_file {
        cmd.args(["--config-file"]);
        cmd.arg(cf);
    }

    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().context("failed to start daemon")?;

    // Wait briefly to detect immediate crashes (e.g. missing build flags, bad config).
    // If the daemon is still alive after this, detach and let it run.
    std::thread::sleep(Duration::from_millis(200));
    if let Some(status) = child.try_wait().context("failed to check daemon process")? {
        let stderr = child
            .stderr
            .take()
            .and_then(|mut s| {
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut s, &mut buf).ok()?;
                Some(buf)
            })
            .unwrap_or_default();
        let stderr = stderr.trim();
        if stderr.is_empty() {
            anyhow::bail!("daemon exited immediately with {status}");
        } else {
            anyhow::bail!("daemon exited immediately with {status}:\n{stderr}");
        }
    }

    Ok(())
}

fn extract_status(response: Response) -> Result<String> {
    match response {
        Response::Status { formatted } | Response::NotReady { formatted } => Ok(formatted),
        Response::Error { message } => anyhow::bail!("{message}"),
        Response::Ok => Ok(String::new()),
        _ => Ok(String::new()),
    }
}

/// Hardcoded fallback when the daemon isn't reachable within the timeout.
const NOT_READY_FALLBACK: &str = "…";

pub fn query(repo_path: &Path, config_file: Option<&Path>) -> Result<String> {
    let socket_path = config::socket_path();
    let request = Request::Query {
        repo_path: repo_path.to_string_lossy().to_string(),
    };

    // Resolve config file: explicit arg > VSD_CONFIG_FILE env var > default path
    // Always resolve so the daemon gets an explicit path regardless of its environment.
    let resolved_config_file = config_file
        .map(|p| p.to_path_buf())
        .or_else(config::config_path);

    // Try connecting with a short timeout (100ms)
    match send_request(&socket_path, &request) {
        Ok(response) => extract_status(response),
        Err(_) => {
            // Daemon not reachable — try to start it, return fallback
            if let Err(e) = start_daemon(&socket_path, resolved_config_file.as_deref()) {
                eprintln!("vcs-status-daemon: {e}");
            }
            Ok(NOT_READY_FALLBACK.to_string())
        }
    }
}

/// Query the running daemon for its version info.
/// Returns (version, git_hash, features) or an error if the daemon isn't reachable.
pub fn daemon_version() -> Result<(String, String, Vec<String>)> {
    let socket_path = config::socket_path();
    let response = send_request(&socket_path, &Request::Version)?;
    match response {
        Response::Version {
            version,
            git_hash,
            features,
        } => Ok((version, git_hash, features)),
        Response::Error { message } => anyhow::bail!("{message}"),
        _ => anyhow::bail!("unexpected response from daemon"),
    }
}

pub fn shutdown() -> Result<()> {
    let socket_path = config::socket_path();
    let response =
        send_request_slow(&socket_path, &Request::Shutdown).context("failed to send shutdown")?;

    match response {
        Response::Ok => Ok(()),
        Response::Error { message } => anyhow::bail!("{message}"),
        _ => Ok(()),
    }
}

pub fn restart(config_file: Option<&Path>) -> Result<()> {
    let socket_path = config::socket_path();
    let pid_path = config::pid_path();

    // Try graceful shutdown first
    let _ = send_request_slow(&socket_path, &Request::Shutdown);

    // Wait for socket to disappear (up to 5 seconds)
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while socket_path.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }

    // If socket still exists, force-kill via pidfile
    if socket_path.exists() {
        if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
            let pid = pid_str.trim();
            let _ = std::process::Command::new("kill")
                .args(["-9", pid])
                .status();
            // Wait briefly for the process to die
            std::thread::sleep(Duration::from_millis(200));
        }
        // Clean up stale socket
        let _ = std::fs::remove_file(&socket_path);
    }

    // Clean up pidfile
    let _ = std::fs::remove_file(&pid_path);

    // Start a fresh daemon
    start_daemon(&socket_path, config_file)?;
    Ok(())
}

pub fn status() -> Result<()> {
    let socket_path = config::socket_path();
    let pid_path = config::pid_path();

    match send_request_slow(&socket_path, &Request::DaemonStatus) {
        Ok(Response::DaemonStatus {
            pid,
            uptime_secs,
            watched_repos,
            stats,
        }) => {
            let hours = uptime_secs / 3600;
            let mins = (uptime_secs % 3600) / 60;
            let secs = uptime_secs % 60;
            eprintln!("daemon running");
            eprintln!("  pid:           {pid}");
            eprintln!("  uptime:        {hours}h {mins}m {secs}s");
            eprintln!("  watched repos: {}", watched_repos.len());
            for repo in &watched_repos {
                eprintln!("    {repo}");
            }
            eprintln!("  socket:        {}", socket_path.display());

            // Performance stats
            eprintln!();
            eprintln!(
                "  queries:       {} ({} hits, {} misses)",
                stats.queries, stats.cache_hits, stats.cache_misses
            );
            if stats.queries > 0 {
                let hit_rate = stats.cache_hits as f64 / stats.queries as f64 * 100.0;
                eprintln!("  hit rate:      {hit_rate:.1}%");
            }
            eprintln!("  refreshes:     {}", stats.refreshes);
            eprintln!(
                "  fs events:     {} ({} ignored)",
                stats.fs_events, stats.fs_events_ignored
            );

            if !stats.recent_query_ms.is_empty() {
                let mut sorted = stats.recent_query_ms.clone();
                sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                let len = sorted.len();
                let p50 = sorted[len / 2];
                let p95 = sorted[(len as f64 * 0.95) as usize];
                let p99 = sorted[((len as f64 * 0.99) as usize).min(len - 1)];
                let max = sorted[len - 1];
                eprintln!(
                    "  timing (last {len}): p50={p50:.1}ms p95={p95:.1}ms p99={p99:.1}ms max={max:.1}ms"
                );
            }

            Ok(())
        }
        Ok(Response::Error { message }) => anyhow::bail!("{message}"),
        Ok(_) => anyhow::bail!("unexpected response from daemon"),
        Err(_) => {
            // Daemon not running — check for stale pidfile
            let stale_pid = std::fs::read_to_string(&pid_path).ok();
            eprintln!("daemon not running");
            if let Some(pid) = stale_pid {
                eprintln!(
                    "  stale pidfile: {} (pid {})",
                    pid_path.display(),
                    pid.trim()
                );
            }
            eprintln!("  socket: {}", socket_path.display());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::daemon::run_daemon;
    use tempfile::TempDir;
    use tokio::process::Command;

    async fn create_jj_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let output = Command::new("jj")
            .args(["git", "init"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();
        assert!(output.status.success());
        dir
    }

    #[tokio::test]
    async fn test_client_connects_to_running_daemon() {
        let dir = create_jj_repo().await;
        let rt = TempDir::with_prefix("vcs-test-client-").unwrap();

        // Point both daemon and client at the same runtime directory
        unsafe { std::env::set_var("VCS_STATUS_DAEMON_DIR", rt.path()) };

        let config = Config {
            color: false,
            ..Default::default()
        };

        let _daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Client calls are synchronous — run on a blocking thread so the
        // tokio executor can still drive the daemon task.
        let dir_path = dir.path().to_path_buf();
        let result = tokio::task::spawn_blocking(move || query(&dir_path, None).unwrap())
            .await
            .unwrap();
        assert!(!result.is_empty());

        tokio::task::spawn_blocking(|| shutdown().ok())
            .await
            .unwrap();
        unsafe { std::env::remove_var("VCS_STATUS_DAEMON_DIR") };
    }

    #[tokio::test]
    async fn test_status_daemon_running() {
        let rt = TempDir::with_prefix("vcs-test-status-running-").unwrap();
        let socket_path = rt.path().join("sock");

        let config = Config {
            color: false,
            ..Default::default()
        };

        let _daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Send DaemonStatus request directly via the socket
        let sp = socket_path.clone();
        let result = tokio::task::spawn_blocking(move || send_request(&sp, &Request::DaemonStatus))
            .await
            .unwrap()
            .unwrap();

        match result {
            Response::DaemonStatus {
                pid,
                uptime_secs,
                watched_repos,
                stats,
            } => {
                assert!(pid > 0);
                assert!(uptime_secs < 10); // just started
                assert!(watched_repos.is_empty()); // no queries yet
                assert_eq!(stats.queries, 0);
            }
            other => panic!("expected DaemonStatus, got {other:?}"),
        }

        // Verify pidfile was created
        assert!(rt.path().join("pid").exists());

        let sp = socket_path.clone();
        tokio::task::spawn_blocking(move || send_request(&sp, &Request::Shutdown).ok())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_status_daemon_not_running() {
        let rt = TempDir::with_prefix("vcs-test-status-notrunning-").unwrap();
        let socket_path = rt.path().join("sock");

        // No daemon started — send_request should fail
        let sp = socket_path.clone();
        let result = tokio::task::spawn_blocking(move || send_request(&sp, &Request::DaemonStatus))
            .await
            .unwrap();

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_status_stale_pidfile() {
        let rt = TempDir::with_prefix("vcs-test-status-stalepid-").unwrap();
        let socket_path = rt.path().join("sock");
        let pid_path = rt.path().join("pid");

        // Write a stale pidfile (PID that doesn't correspond to our daemon)
        std::fs::write(&pid_path, "999999").unwrap();

        // No daemon running — send_request should fail
        let sp = socket_path.clone();
        let result = tokio::task::spawn_blocking(move || send_request(&sp, &Request::DaemonStatus))
            .await
            .unwrap();

        assert!(result.is_err());
        // But the pidfile still exists (stale)
        assert!(pid_path.exists());
    }

    #[tokio::test]
    async fn test_query_returns_fallback_when_daemon_not_running() {
        let rt = TempDir::with_prefix("vcs-test-fallback-").unwrap();
        // Point client at a directory with no daemon
        unsafe { std::env::set_var("VCS_STATUS_DAEMON_DIR", rt.path()) };

        let dir = create_jj_repo().await;
        let dir_path = dir.path().to_path_buf();
        let result = tokio::task::spawn_blocking(move || query(&dir_path, None))
            .await
            .unwrap()
            .unwrap();

        assert_eq!(result, NOT_READY_FALLBACK, "should return fallback text");

        unsafe { std::env::remove_var("VCS_STATUS_DAEMON_DIR") };
    }

    #[tokio::test]
    async fn test_auto_start_daemon_then_query() {
        let exe = escargot::CargoBuild::new()
            .bin("vcs-status-daemon")
            .current_target()
            .run()
            .expect("failed to build vcs-status-daemon")
            .path()
            .to_path_buf();

        let dir = create_jj_repo().await;
        let rt = TempDir::with_prefix("vcs-test-autostart-").unwrap();
        let socket_path = rt.path().join("sock");

        // Verify: runtime dir exists but no socket, no cache
        assert!(rt.path().exists());
        assert!(!socket_path.exists());

        // First query: should return fallback and auto-start daemon
        let output = Command::new(&exe)
            .args(["--repo", dir.path().to_str().unwrap()])
            .env("VCS_STATUS_DAEMON_DIR", rt.path())
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "first query failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let first = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(
            first, NOT_READY_FALLBACK,
            "first query should return fallback"
        );

        // Wait for daemon to start listening
        for _ in 0..30 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            if socket_path.exists() {
                break;
            }
        }
        assert!(socket_path.exists(), "daemon should have created socket");

        // Second query: daemon is running, should eventually return real status
        // May need a few retries as daemon populates cache
        let mut got_status = false;
        for _ in 0..20 {
            let output = Command::new(&exe)
                .args(["--repo", dir.path().to_str().unwrap()])
                .env("VCS_STATUS_DAEMON_DIR", rt.path())
                .output()
                .await
                .unwrap();
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if text != NOT_READY_FALLBACK && !text.is_empty() {
                got_status = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
        assert!(
            got_status,
            "should eventually get real status from auto-started daemon"
        );

        // Clean up: shut down the daemon
        let sp = socket_path.clone();
        let _ =
            tokio::task::spawn_blocking(move || send_request_slow(&sp, &Request::Shutdown)).await;
    }
}
