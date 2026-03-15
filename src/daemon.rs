use anyhow::Result;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::time::{Duration, Instant};

use crate::config::Config;
use crate::jj::{format_status, query_jj_status};
use crate::protocol::{Request, Response};
use crate::watcher::{watch_repo, RepoWatcher, WatchEvent};

struct DaemonState {
    cache: HashMap<PathBuf, String>,
    watchers: HashMap<PathBuf, RepoWatcher>,
    last_query: Instant,
    config: Config,
}

pub async fn run_daemon(config: Config) -> Result<()> {
    let socket_path = config.socket_path();

    // Clean up stale socket
    if socket_path.exists() {
        if tokio::net::UnixStream::connect(&socket_path).await.is_err() {
            std::fs::remove_file(&socket_path)?;
        } else {
            anyhow::bail!("daemon already running (socket is active)");
        }
    }

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let listener = UnixListener::bind(&socket_path)?;
    eprintln!("daemon listening on {}", socket_path.display());

    let (watch_tx, watch_rx) = mpsc::unbounded_channel();
    let shutdown = Arc::new(Notify::new());

    let state = Arc::new(Mutex::new(DaemonState {
        cache: HashMap::new(),
        watchers: HashMap::new(),
        last_query: Instant::now(),
        config: config.clone(),
    }));

    // Spawn refresh task
    tokio::spawn(refresh_task(state.clone(), watch_rx));

    // Spawn idle timeout task
    let state_idle = state.clone();
    let shutdown_idle = shutdown.clone();
    let idle_timeout_secs = config.idle_timeout_secs;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let last = state_idle.lock().await.last_query;
            if last.elapsed() > Duration::from_secs(idle_timeout_secs) {
                eprintln!("idle timeout, shutting down");
                shutdown_idle.notify_one();
                return;
            }
        }
    });

    // Signal handling for cleanup
    let shutdown_sig = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown_sig.notify_one();
    });

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                let state = state.clone();
                let watch_tx = watch_tx.clone();
                let shutdown_conn = shutdown.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, state, watch_tx, shutdown_conn).await {
                        eprintln!("connection error: {e}");
                    }
                });
            }
            _ = shutdown.notified() => {
                eprintln!("daemon shutting down");
                let _ = std::fs::remove_file(&socket_path);
                return Ok(());
            }
        }
    }
}

async fn handle_connection(
    stream: tokio::net::UnixStream,
    state: Arc<Mutex<DaemonState>>,
    watch_tx: mpsc::UnboundedSender<WatchEvent>,
    shutdown: Arc<Notify>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;

    let request: Request = serde_json::from_str(line.trim())?;

    match request {
        Request::Query { repo_path } => {
            let repo_path = PathBuf::from(&repo_path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(&repo_path));

            let (cached, config) = {
                let mut st = state.lock().await;
                st.last_query = Instant::now();

                if !st.watchers.contains_key(&repo_path) {
                    if let Ok(watcher) = watch_repo(&repo_path, watch_tx.clone()) {
                        st.watchers.insert(repo_path.clone(), watcher);
                    }
                }

                let cached = st.cache.get(&repo_path).cloned();
                let config = st.config.clone();
                (cached, config)
            };

            let formatted = if let Some(cached) = cached {
                cached
            } else {
                match query_jj_status(&repo_path, &config, false).await {
                    Ok(status) => {
                        let formatted = format_status(&status, &config.format, config.color);
                        state.lock().await.cache.insert(repo_path, formatted.clone());
                        formatted
                    }
                    Err(e) => {
                        return send_response(&mut writer, Response::Error { message: e.to_string() }).await;
                    }
                }
            };

            send_response(&mut writer, Response::Status { formatted }).await
        }
        Request::Shutdown => {
            send_response(&mut writer, Response::Ok).await?;
            shutdown.notify_one();
            Ok(())
        }
    }
}

async fn send_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: Response,
) -> Result<()> {
    let mut json = serde_json::to_string(&response)?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;
    Ok(())
}

async fn refresh_task(
    state: Arc<Mutex<DaemonState>>,
    mut watch_rx: mpsc::UnboundedReceiver<WatchEvent>,
) {
    let mut wc_changed: HashMap<PathBuf, bool> = HashMap::new();

    loop {
        let Some(event) = watch_rx.recv().await else {
            return;
        };

        if event.working_copy_changed {
            wc_changed.insert(event.repo_path.clone(), true);
        } else {
            wc_changed.entry(event.repo_path.clone()).or_insert(false);
        }

        let debounce_ms = state.lock().await.config.debounce_ms;
        tokio::time::sleep(Duration::from_millis(debounce_ms)).await;

        while let Ok(event) = watch_rx.try_recv() {
            if event.working_copy_changed {
                wc_changed.insert(event.repo_path.clone(), true);
            } else {
                wc_changed.entry(event.repo_path.clone()).or_insert(false);
            }
        }

        let repos: Vec<(PathBuf, bool)> = wc_changed.drain().collect();
        for (repo_path, needs_snapshot) in repos {
            let config = state.lock().await.config.clone();
            let ignore_wc = !needs_snapshot;
            match query_jj_status(&repo_path, &config, ignore_wc).await {
                Ok(status) => {
                    let formatted = format_status(&status, &config.format, config.color);
                    state.lock().await.cache.insert(repo_path, formatted);
                }
                Err(e) => {
                    eprintln!("refresh error for {}: {e}", repo_path.display());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::process::Command;
    use tokio::time::Duration;

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

    async fn send_request(socket_path: &std::path::Path, request: &Request) -> Response {
        let stream = UnixStream::connect(socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut json = serde_json::to_string(request).unwrap();
        json.push('\n');
        writer.write_all(json.as_bytes()).await.unwrap();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    fn temp_socket_path(suffix: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "jj-test-{}-{suffix}.sock",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn test_daemon_serves_status() {
        let dir = create_jj_repo().await;
        let socket_path = temp_socket_path("serves");
        let _ = std::fs::remove_file(&socket_path);
        let config = Config {
            socket_path: Some(socket_path.to_string_lossy().to_string()),
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config));
        tokio::time::sleep(Duration::from_millis(200)).await;

        let resp = send_request(
            &socket_path,
            &Request::Query {
                repo_path: dir.path().to_string_lossy().to_string(),
            },
        )
        .await;

        match resp {
            Response::Status { formatted } => {
                assert!(!formatted.is_empty(), "expected non-empty status");
            }
            other => panic!("expected Status, got {other:?}"),
        }

        // Shutdown
        let resp = send_request(&socket_path, &Request::Shutdown).await;
        assert_eq!(resp, Response::Ok);
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_daemon_shutdown() {
        let socket_path = temp_socket_path("shutdown");
        let _ = std::fs::remove_file(&socket_path);
        let config = Config {
            socket_path: Some(socket_path.to_string_lossy().to_string()),
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config));
        tokio::time::sleep(Duration::from_millis(200)).await;

        let resp = send_request(&socket_path, &Request::Shutdown).await;
        assert_eq!(resp, Response::Ok);

        // Daemon should exit cleanly
        daemon.await.unwrap().unwrap();
        assert!(!socket_path.exists());
    }

    #[tokio::test]
    async fn test_daemon_stale_socket() {
        let socket_path = temp_socket_path("stale");
        let _ = std::fs::remove_file(&socket_path);
        std::fs::write(&socket_path, "").unwrap();

        let config = Config {
            socket_path: Some(socket_path.to_string_lossy().to_string()),
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config));
        tokio::time::sleep(Duration::from_millis(200)).await;

        let dir = create_jj_repo().await;
        let resp = send_request(
            &socket_path,
            &Request::Query {
                repo_path: dir.path().to_string_lossy().to_string(),
            },
        )
        .await;
        assert!(matches!(resp, Response::Status { .. }));

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_daemon_cache_update() {
        let dir = create_jj_repo().await;
        let socket_path = temp_socket_path("cache");
        let _ = std::fs::remove_file(&socket_path);
        let config = Config {
            socket_path: Some(socket_path.to_string_lossy().to_string()),
            debounce_ms: 100,
            format: "{{ change_id }} {{ description }}{% if empty %} EMPTY{% endif %}".to_string(),
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config));
        tokio::time::sleep(Duration::from_millis(200)).await;

        // First query
        let resp = send_request(
            &socket_path,
            &Request::Query {
                repo_path: dir.path().to_string_lossy().to_string(),
            },
        )
        .await;
        let first = match resp {
            Response::Status { formatted } => formatted,
            other => panic!("expected Status, got {other:?}"),
        };
        assert!(!first.contains("changed"), "first should not contain 'changed': {first:?}");

        // Make a change
        Command::new("jj")
            .args(["describe", "-m", "changed"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        // Wait for debounce + refresh
        tokio::time::sleep(Duration::from_millis(2000)).await;

        // Second query - should reflect the change
        let resp = send_request(
            &socket_path,
            &Request::Query {
                repo_path: dir.path().to_string_lossy().to_string(),
            },
        )
        .await;
        let second = match resp {
            Response::Status { formatted } => formatted,
            other => panic!("expected Status, got {other:?}"),
        };

        assert!(second.contains("changed"),
            "expected cache to update with description, got: {second:?}");

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }
}
