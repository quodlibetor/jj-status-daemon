use anyhow::{Context, Result};
use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::config;
use crate::protocol::{Request, Response};

fn send_request(socket_path: &Path, request: &Request) -> Result<Response> {
    let stream = UnixStream::connect(socket_path).context("failed to connect to daemon")?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
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

fn start_daemon(socket_path: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("failed to get current exe")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "--dir"]);
    cmd.arg(socket_path.parent().unwrap_or(socket_path));

    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    cmd.spawn().context("failed to start daemon")?;
    Ok(())
}

fn extract_status(response: Response) -> Result<String> {
    match response {
        Response::Status { formatted } => Ok(formatted),
        Response::Error { message } => anyhow::bail!("{message}"),
        Response::Ok => Ok(String::new()),
        _ => Ok(String::new()),
    }
}

/// Try to read cached status directly from a file (fastest path — no socket, no directory walk).
/// The daemon hardlinks queried directories to the repo root's cache file.
fn try_cache_file(repo_path: &Path) -> Option<String> {
    let cache_path = config::cache_file_path(repo_path);
    std::fs::read_to_string(cache_path).ok()
}

pub fn query(repo_path: &Path, use_cache: bool) -> Result<String> {
    // Fast path: read directly from cache file (no IPC)
    if use_cache && let Some(cached) = try_cache_file(repo_path) {
        return Ok(cached);
    }

    // Slow path: socket query (also populates the cache file for next time)
    let socket_path = config::socket_path();
    let request = Request::Query {
        repo_path: repo_path.to_string_lossy().to_string(),
    };

    // Try connecting directly first
    if let Ok(response) = send_request(&socket_path, &request) {
        return extract_status(response);
    }

    // Daemon not running, start it
    start_daemon(&socket_path)?;

    // Retry with backoff
    for i in 0..10 {
        std::thread::sleep(Duration::from_millis(100 * (i + 1)));
        if let Ok(response) = send_request(&socket_path, &request) {
            return extract_status(response);
        }
    }

    anyhow::bail!("failed to connect to daemon after starting it")
}

pub fn shutdown() -> Result<()> {
    let socket_path = config::socket_path();
    let response =
        send_request(&socket_path, &Request::Shutdown).context("failed to send shutdown")?;

    match response {
        Response::Ok => Ok(()),
        Response::Error { message } => anyhow::bail!("{message}"),
        _ => Ok(()),
    }
}

pub fn restart() -> Result<()> {
    let socket_path = config::socket_path();
    let pid_path = config::pid_path();

    // Try graceful shutdown first
    let _ = send_request(&socket_path, &Request::Shutdown);

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
    start_daemon(&socket_path)?;
    Ok(())
}

pub fn status() -> Result<()> {
    let socket_path = config::socket_path();
    let pid_path = config::pid_path();

    match send_request(&socket_path, &Request::DaemonStatus) {
        Ok(Response::DaemonStatus {
            pid,
            uptime_secs,
            watched_repos,
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
        let result = tokio::task::spawn_blocking(move || query(&dir_path, true).unwrap())
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
            } => {
                assert!(pid > 0);
                assert!(uptime_secs < 10); // just started
                assert!(watched_repos.is_empty()); // no queries yet
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
}
