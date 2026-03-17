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
}
