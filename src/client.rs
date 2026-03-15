use anyhow::{Context, Result};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::{Duration, timeout};

use crate::config;
use crate::protocol::{Request, Response};

async fn send_request(socket_path: &Path, request: &Request) -> Result<Response> {
    let stream = UnixStream::connect(socket_path)
        .await
        .context("failed to connect to daemon")?;
    let (reader, mut writer) = stream.into_split();

    let mut json = serde_json::to_string(request)?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;

    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let response: Response = serde_json::from_str(line.trim())?;
    Ok(response)
}

fn start_daemon(socket_path: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("failed to get current exe")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.args(["daemon", "--socket"]);
    cmd.arg(socket_path);

    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    cmd.spawn().context("failed to start daemon")?;
    Ok(())
}

pub async fn query(repo_path: &Path) -> Result<String> {
    let socket_path = config::socket_path();
    let request = Request::Query {
        repo_path: repo_path.to_string_lossy().to_string(),
    };

    // Try connecting directly first
    if let Ok(response) = send_request(&socket_path, &request).await {
        return match response {
            Response::Status { formatted } => Ok(formatted),
            Response::Error { message } => anyhow::bail!("{message}"),
            Response::Ok => Ok(String::new()),
        };
    }

    // Daemon not running, start it
    start_daemon(&socket_path)?;

    // Retry with backoff
    for i in 0..10 {
        tokio::time::sleep(Duration::from_millis(100 * (i + 1))).await;
        if let Ok(response) = send_request(&socket_path, &request).await {
            return match response {
                Response::Status { formatted } => Ok(formatted),
                Response::Error { message } => anyhow::bail!("{message}"),
                Response::Ok => Ok(String::new()),
            };
        }
    }

    anyhow::bail!("failed to connect to daemon after starting it")
}

pub async fn shutdown() -> Result<()> {
    let socket_path = config::socket_path();
    let response = timeout(
        Duration::from_secs(5),
        send_request(&socket_path, &Request::Shutdown),
    )
    .await
    .context("timeout connecting to daemon")?
    .context("failed to send shutdown")?;

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
        let sock = std::env::temp_dir().join(format!(
            "jj-client-test-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&sock);

        // Point both daemon and client at the same test socket
        unsafe { std::env::set_var("JJ_STATUS_DAEMON_SOCKET_PATH", &sock) };

        let config = Config {
            color: false,
            ..Default::default()
        };

        let _daemon = tokio::spawn(run_daemon(config, sock.clone()));
        tokio::time::sleep(Duration::from_millis(200)).await;

        let result = query(dir.path()).await.unwrap();
        assert!(!result.is_empty());

        shutdown().await.ok();
        unsafe { std::env::remove_var("JJ_STATUS_DAEMON_SOCKET_PATH") };
    }
}
