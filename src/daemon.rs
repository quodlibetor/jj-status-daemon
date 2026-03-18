use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::time::{Duration, Instant};

use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::git::query_git_status;
use crate::jj::query_jj_status;
use crate::protocol::{Request, Response, VcsKind};
use crate::template::format_status;
use crate::watcher::{RepoWatcher, WatchEvent, watch_repo};

struct DaemonState {
    cache: HashMap<PathBuf, String>,
    watchers: HashMap<PathBuf, RepoWatcher>,
    /// Maps arbitrary directories to their repo root and VCS kind. Negatives are not cached.
    dir_to_repo: HashMap<PathBuf, (PathBuf, VcsKind)>,
    last_query: Instant,
    started_at: Instant,
    config: Config,
    cache_dir: PathBuf,
}

use crate::config::find_repo_root;

/// Maximum log file size before rotation (5 MB).
const MAX_LOG_SIZE: u64 = 5 * 1024 * 1024;

pub fn init_logging(runtime_dir: &Path) {
    std::fs::create_dir_all(runtime_dir).ok();

    // Rotate on startup: if the log exceeds the limit, move it to .old (keeping one backup).
    let log_path = runtime_dir.join("daemon.log");
    if let Ok(meta) = log_path.metadata()
        && meta.len() > MAX_LOG_SIZE
    {
        let old_path = runtime_dir.join("daemon.log.old");
        let _ = std::fs::rename(&log_path, &old_path);
    }

    let file_appender = tracing_appender::rolling::never(runtime_dir, "daemon.log");

    let filter =
        EnvFilter::try_from_env("VCS_STATUS_DAEMON_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(file_appender)
        .with_ansi(false)
        .init();
}

pub async fn run_daemon(config: Config, runtime_dir: PathBuf) -> Result<()> {
    let socket_path = runtime_dir.join("sock");
    let cache_dir = runtime_dir.join("cache");
    tracing::info!(
        template_name = %config.template_name,
        has_format_override = config.format.is_some(),
        "starting daemon"
    );

    // Validate the template early so the user gets immediate feedback
    let resolved = config.resolved_format();
    if let Err(e) = crate::template::validate_template(&resolved) {
        let source = if config.format.is_some() {
            "format".to_string()
        } else {
            format!("template \"{}\"", config.template_name)
        };
        eprintln!("warning: invalid {source}: {e}");
        tracing::error!(source = %source, "invalid template: {e}");
    }

    // Clean up stale socket
    if socket_path.exists() {
        if tokio::net::UnixStream::connect(&socket_path).await.is_err() {
            tracing::debug!(path = %socket_path.display(), "removing stale socket");
            std::fs::remove_file(&socket_path)?;
        } else {
            anyhow::bail!("daemon already running (socket is active)");
        }
    }

    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let listener = UnixListener::bind(&socket_path)?;
    tracing::info!(path = %socket_path.display(), "daemon listening");

    // Write pidfile so the client can force-kill us if graceful shutdown fails
    let pid_path = runtime_dir.join("pid");
    std::fs::write(&pid_path, std::process::id().to_string()).ok();

    let (watch_tx, watch_rx) = mpsc::unbounded_channel();
    let shutdown = Arc::new(Notify::new());

    let state = Arc::new(Mutex::new(DaemonState {
        cache: HashMap::new(),
        watchers: HashMap::new(),
        dir_to_repo: HashMap::new(),
        last_query: Instant::now(),
        started_at: Instant::now(),
        config: config.clone(),
        cache_dir: cache_dir.clone(),
    }));

    // Spawn refresh task
    tokio::spawn(refresh_task(state.clone(), watch_rx));

    // Spawn idle timeout task (also handles log rotation)
    let state_idle = state.clone();
    let shutdown_idle = shutdown.clone();
    let idle_timeout_secs = config.idle_timeout_secs;
    let log_dir = runtime_dir.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let last = state_idle.lock().await.last_query;
            if last.elapsed() > Duration::from_secs(idle_timeout_secs) {
                tracing::info!("idle timeout, shutting down");
                shutdown_idle.notify_one();
                return;
            }
            // Rotate log if it exceeds the size limit
            let log_path = log_dir.join("daemon.log");
            if let Ok(meta) = log_path.metadata()
                && meta.len() > MAX_LOG_SIZE
            {
                let old_path = log_dir.join("daemon.log.old");
                let _ = std::fs::rename(&log_path, &old_path);
            }
        }
    });

    // Periodic watcher health check: remove watchers for repos that no longer exist
    let state_health = state.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(300)).await;
            let mut st = state_health.lock().await;
            let stale: Vec<PathBuf> = st
                .watchers
                .keys()
                .filter(|p| !p.exists())
                .cloned()
                .collect();
            for path in &stale {
                tracing::info!(repo = %path.display(), "repo no longer exists, removing watcher");
                st.watchers.remove(path);
                st.cache.remove(path);
                st.dir_to_repo.retain(|_, (root, _)| root != path);
            }
        }
    });

    // Ctrl-C: clear cache files but keep running
    let state_int = state.clone();
    let cache_dir_int = cache_dir.clone();
    tokio::spawn(async move {
        loop {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("received ctrl-c, clearing cache");
            let _ = std::fs::remove_dir_all(&cache_dir_int);
            state_int.lock().await.cache.clear();
        }
    });

    // SIGTERM: clean up everything and shut down
    let shutdown_term = shutdown.clone();
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        sigterm.recv().await;
        shutdown_term.notify_one();
    });

    loop {
        tokio::select! {
            result = listener.accept() => {
                let (stream, _) = result?;
                let state = state.clone();
                let watch_tx = watch_tx.clone();
                let shutdown_conn = shutdown.clone();

                tokio::spawn(async move {
                    match tokio::time::timeout(
                        Duration::from_secs(60),
                        handle_connection(stream, state, watch_tx, shutdown_conn),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => tracing::warn!(error = %e, "connection error"),
                        Err(_) => tracing::warn!("connection handler timed out"),
                    }
                });
            }
            _ = shutdown.notified() => {
                tracing::info!("daemon shutting down");
                if let Err(e) = std::fs::remove_file(&socket_path) {
                    tracing::warn!(path = %socket_path.display(), error = %e, "failed to remove socket");
                }
                if let Err(e) = std::fs::remove_dir_all(&cache_dir) {
                    tracing::warn!(path = %cache_dir.display(), error = %e, "failed to remove cache directory");
                }
                let _ = std::fs::remove_file(&pid_path);
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
            let query_path = PathBuf::from(&repo_path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(&repo_path));

            // Resolve the repo root and VCS kind from the given path
            let (repo_path, vcs_kind, cached, config, cd) = {
                let mut st = state.lock().await;
                st.last_query = Instant::now();

                let resolved = if let Some(entry) = st.dir_to_repo.get(&query_path) {
                    Some(entry.clone())
                } else if let Some(found) = find_repo_root(&query_path) {
                    st.dir_to_repo.insert(query_path.clone(), found.clone());
                    Some(found)
                } else {
                    None
                };

                let Some((repo_path, vcs_kind)) = resolved else {
                    tracing::debug!("no repo found");
                    drop(st);
                    return send_response(
                        &mut writer,
                        Response::Status {
                            formatted: String::new(),
                        },
                    )
                    .await;
                };

                if !st.watchers.contains_key(&repo_path)
                    && let Ok(watcher) = watch_repo(&repo_path, vcs_kind, watch_tx.clone())
                {
                    st.watchers.insert(repo_path.clone(), watcher);
                }

                let cached = st.cache.get(&repo_path).cloned();
                let config = st.config.clone();
                let cache_dir = st.cache_dir.clone();
                (repo_path, vcs_kind, cached, config, cache_dir)
            };

            let formatted = if let Some(cached) = cached {
                tracing::debug!(repo = %repo_path.display(), "cache hit");
                cached
            } else {
                tracing::debug!(repo = %repo_path.display(), vcs = ?vcs_kind, "cache miss, querying");
                let result = match vcs_kind {
                    VcsKind::Jj => query_jj_status(&repo_path, &config, false).await,
                    VcsKind::Git => query_git_status(&repo_path, &config).await,
                };
                match result {
                    Ok(status) => {
                        let formatted =
                            format_status(&status, &config.resolved_format(), config.color);
                        write_cache_file(&cd, &repo_path, &formatted);
                        state
                            .lock()
                            .await
                            .cache
                            .insert(repo_path.clone(), formatted.clone());
                        formatted
                    }
                    Err(e) => {
                        return send_response(
                            &mut writer,
                            Response::Error {
                                message: e.to_string(),
                            },
                        )
                        .await;
                    }
                }
            };

            // Ensure the queried directory has a hardlink to the repo root's cache file
            // so the client can read it directly next time without directory walking.
            if query_path != repo_path {
                link_cache_file(&cd, &repo_path, &query_path);
            }

            send_response(&mut writer, Response::Status { formatted }).await
        }
        Request::Flush => {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = watch_tx.send(WatchEvent::Flush(tx));
            let _ = rx.await;
            send_response(&mut writer, Response::Ok).await
        }
        Request::Shutdown => {
            send_response(&mut writer, Response::Ok).await?;
            shutdown.notify_one();
            Ok(())
        }
        Request::DaemonStatus => {
            let st = state.lock().await;
            let watched_repos = st
                .watchers
                .keys()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            let uptime_secs = st.started_at.elapsed().as_secs();
            drop(st);
            send_response(
                &mut writer,
                Response::DaemonStatus {
                    pid: std::process::id(),
                    uptime_secs,
                    watched_repos,
                },
            )
            .await
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

/// Compute the cache file path for a given directory within a specific cache dir.
fn cache_file_in(cache_dir: &Path, dir: &Path) -> PathBuf {
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let name = canonical.to_string_lossy().replace('/', "%");
    cache_dir.join(name)
}

/// Write the formatted status to the on-disk cache file for fast client reads.
///
/// Because subdirectory entries are hardlinked to the repo root file,
/// we write in-place (not rename) so all hardlinks see the update via the shared inode.
fn write_cache_file(cache_dir: &Path, repo_path: &Path, formatted: &str) {
    let file_path = cache_file_in(cache_dir, repo_path);
    if let Some(parent) = file_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&file_path, formatted) {
        tracing::warn!(path = %file_path.display(), error = %e, "failed to write cache file");
    }
}

/// Create a hardlink from a queried subdirectory's cache entry to the repo root's cache file.
/// Since they share the same inode, future writes to the repo root file update both.
fn link_cache_file(cache_dir: &Path, repo_root: &Path, query_dir: &Path) {
    let root_file = cache_file_in(cache_dir, repo_root);
    let dir_file = cache_file_in(cache_dir, query_dir);
    if dir_file.exists() {
        return; // already linked
    }
    if let Some(parent) = dir_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::hard_link(&root_file, &dir_file) {
        tracing::debug!(
            src = %root_file.display(),
            dst = %dir_file.display(),
            error = %e,
            "failed to hardlink cache file"
        );
    }
}

/// Tracks per-repo: (vcs_kind, working_copy_changed)
fn collect_change(pending: &mut HashMap<PathBuf, (VcsKind, bool)>, event: WatchEvent) {
    match event {
        WatchEvent::Change {
            repo_path,
            vcs_kind,
            working_copy_changed,
        } => {
            if working_copy_changed {
                pending.insert(repo_path, (vcs_kind, true));
            } else {
                pending.entry(repo_path).or_insert((vcs_kind, false));
            }
        }
        WatchEvent::Flush(_) => {} // handled by caller
    }
}

async fn process_pending(
    state: &Arc<Mutex<DaemonState>>,
    pending: &mut HashMap<PathBuf, (VcsKind, bool)>,
) {
    let repos: Vec<(PathBuf, VcsKind, bool)> =
        pending.drain().map(|(p, (v, wc))| (p, v, wc)).collect();
    for (repo_path, vcs_kind, needs_snapshot) in repos {
        let (config, cd) = {
            let st = state.lock().await;
            (st.config.clone(), st.cache_dir.clone())
        };
        let result = match vcs_kind {
            VcsKind::Jj => {
                let ignore_wc = !needs_snapshot;
                query_jj_status(&repo_path, &config, ignore_wc).await
            }
            VcsKind::Git => query_git_status(&repo_path, &config).await,
        };
        match result {
            Ok(status) => {
                let formatted = format_status(&status, &config.resolved_format(), config.color);
                write_cache_file(&cd, &repo_path, &formatted);
                state.lock().await.cache.insert(repo_path, formatted);
            }
            Err(e) => {
                tracing::error!(repo = %repo_path.display(), error = %e, "refresh failed");
            }
        }
    }
}

async fn refresh_task(
    state: Arc<Mutex<DaemonState>>,
    mut watch_rx: mpsc::UnboundedReceiver<WatchEvent>,
) {
    let mut wc_changed: HashMap<PathBuf, (VcsKind, bool)> = HashMap::new();

    loop {
        let Some(event) = watch_rx.recv().await else {
            return;
        };

        // Handle flush immediately if nothing is pending
        if let WatchEvent::Flush(tx) = event {
            process_pending(&state, &mut wc_changed).await;
            let _ = tx.send(());
            continue;
        }

        collect_change(&mut wc_changed, event);

        let debounce_ms = state.lock().await.config.debounce_ms;
        tokio::time::sleep(Duration::from_millis(debounce_ms)).await;

        // Drain remaining events, stopping at a flush
        let mut flush_tx = None;
        while let Ok(event) = watch_rx.try_recv() {
            if let WatchEvent::Flush(tx) = event {
                flush_tx = Some(tx);
                break;
            }
            collect_change(&mut wc_changed, event);
        }

        process_pending(&state, &mut wc_changed).await;

        if let Some(tx) = flush_tx {
            let _ = tx.send(());
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

    #[test]
    fn test_find_repo_root() {
        let dir = TempDir::new().unwrap();
        let jj_dir = dir.path().join(".jj");
        std::fs::create_dir(&jj_dir).unwrap();

        let sub = dir.path().join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();

        assert_eq!(
            find_repo_root(&sub),
            Some((dir.path().to_path_buf(), VcsKind::Jj))
        );
    }

    #[test]
    fn test_find_repo_root_at_root() {
        let dir = TempDir::new().unwrap();
        let jj_dir = dir.path().join(".jj");
        std::fs::create_dir(&jj_dir).unwrap();

        assert_eq!(
            find_repo_root(dir.path()),
            Some((dir.path().to_path_buf(), VcsKind::Jj))
        );
    }

    #[test]
    fn test_find_repo_root_git() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();

        assert_eq!(
            find_repo_root(dir.path()),
            Some((dir.path().to_path_buf(), VcsKind::Git))
        );
    }

    #[test]
    fn test_find_repo_root_jj_wins() {
        let dir = TempDir::new().unwrap();
        // Both .jj and .git present — jj should win
        std::fs::create_dir(dir.path().join(".jj")).unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();

        assert_eq!(
            find_repo_root(dir.path()),
            Some((dir.path().to_path_buf(), VcsKind::Jj))
        );
    }

    #[test]
    fn test_find_repo_root_not_found() {
        let dir = TempDir::new().unwrap();
        assert_eq!(find_repo_root(dir.path()), None);
    }

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

    fn temp_runtime_dir(suffix: &str) -> TempDir {
        TempDir::with_prefix(format!("vcs-test-{suffix}-")).unwrap()
    }

    /// Wait for filesystem events to arrive, then flush the daemon's refresh task.
    async fn flush_daemon(socket_path: &std::path::Path) {
        // Brief sleep to let filesystem events propagate to the watcher
        tokio::time::sleep(Duration::from_millis(100)).await;
        let resp = send_request(socket_path, &Request::Flush).await;
        assert_eq!(resp, Response::Ok);
    }

    #[tokio::test]
    async fn test_daemon_serves_status() {
        let dir = create_jj_repo().await;
        let rt = temp_runtime_dir("serves");
        let socket_path = rt.path().join("sock");
        let config = Config {
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));
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
    async fn test_daemon_resolves_subdirectory() {
        let dir = create_jj_repo().await;
        let sub = dir.path().join("src").join("nested");
        std::fs::create_dir_all(&sub).unwrap();

        let rt = temp_runtime_dir("subdir");
        let socket_path = rt.path().join("sock");
        let config = Config {
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Query from a subdirectory — daemon should resolve the repo root
        let resp = send_request(
            &socket_path,
            &Request::Query {
                repo_path: sub.to_string_lossy().to_string(),
            },
        )
        .await;

        match resp {
            Response::Status { formatted } => {
                assert!(
                    !formatted.is_empty(),
                    "expected non-empty status from subdirectory query"
                );
            }
            other => panic!("expected Status, got {other:?}"),
        }

        // Query from the repo root should return the same result (cached via dir_to_repo)
        let resp2 = send_request(
            &socket_path,
            &Request::Query {
                repo_path: dir.path().to_string_lossy().to_string(),
            },
        )
        .await;

        match resp2 {
            Response::Status { formatted } => {
                assert!(
                    !formatted.is_empty(),
                    "expected non-empty status from root query"
                );
            }
            other => panic!("expected Status, got {other:?}"),
        }

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_daemon_not_a_repo() {
        let dir = TempDir::new().unwrap(); // no jj init

        let rt = temp_runtime_dir("norepo");
        let socket_path = rt.path().join("sock");
        let config = Config {
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));
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
                assert!(
                    formatted.is_empty(),
                    "expected empty status for non-repo, got: {formatted:?}"
                );
            }
            other => panic!("expected empty Status, got {other:?}"),
        }

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_daemon_shutdown() {
        let rt = temp_runtime_dir("shutdown");
        let socket_path = rt.path().join("sock");
        let config = Config {
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));
        tokio::time::sleep(Duration::from_millis(200)).await;

        let resp = send_request(&socket_path, &Request::Shutdown).await;
        assert_eq!(resp, Response::Ok);

        // Daemon should exit cleanly
        daemon.await.unwrap().unwrap();
        assert!(!socket_path.exists());
    }

    #[tokio::test]
    async fn test_daemon_shutdown_cleans_cache() {
        let dir = create_jj_repo().await;
        let rt = temp_runtime_dir("shutdown-cache");
        let socket_path = rt.path().join("sock");
        let cache_dir = rt.path().join("cache");
        let config = Config {
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Query to populate the cache file
        let resp = send_request(
            &socket_path,
            &Request::Query {
                repo_path: dir.path().to_string_lossy().to_string(),
            },
        )
        .await;
        assert!(matches!(resp, Response::Status { .. }));

        // Verify cache file was created
        assert!(
            cache_dir.exists(),
            "cache directory should exist after query"
        );

        // Shutdown
        let resp = send_request(&socket_path, &Request::Shutdown).await;
        assert_eq!(resp, Response::Ok);
        daemon.await.unwrap().unwrap();

        // Verify cleanup
        assert!(!socket_path.exists(), "socket should be removed");
        assert!(
            !cache_dir.exists(),
            "cache directory should be removed on shutdown"
        );
    }

    /// Helper: start the daemon as a subprocess and wait for it to be ready.
    async fn spawn_daemon_process(runtime_dir: &std::path::Path) -> tokio::process::Child {
        let exe = escargot::CargoBuild::new()
            .bin("vcs-status-daemon")
            .current_target()
            .run()
            .expect("failed to build vcs-status-daemon")
            .path()
            .to_path_buf();

        let socket_path = runtime_dir.join("sock");
        let child = Command::new(&exe)
            .args(["daemon", "--dir"])
            .arg(runtime_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();

        // Wait for daemon to start listening
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if socket_path.exists() {
                return child;
            }
        }
        panic!("daemon did not create socket at {}", socket_path.display());
    }

    #[tokio::test]
    async fn test_daemon_sigterm_cleans_cache() {
        let dir = create_jj_repo().await;
        let rt = temp_runtime_dir("sigterm-cache");
        let socket_path = rt.path().join("sock");
        let cache_dir = rt.path().join("cache");

        let mut child = spawn_daemon_process(rt.path()).await;

        // Query to populate the cache
        let resp = send_request(
            &socket_path,
            &Request::Query {
                repo_path: dir.path().to_string_lossy().to_string(),
            },
        )
        .await;
        assert!(matches!(resp, Response::Status { .. }));

        assert!(
            cache_dir.exists(),
            "cache directory should exist after query"
        );

        // Send SIGTERM and wait for clean exit
        let pid = child.id().expect("child should have pid");
        std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap();

        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("daemon should exit within 5s")
            .unwrap();
        assert!(
            status.success() || status.code().is_none(),
            "daemon should exit cleanly on SIGTERM, got: {status}"
        );

        // Verify cleanup
        assert!(
            !socket_path.exists(),
            "socket should be removed after SIGTERM"
        );
        assert!(
            !cache_dir.exists(),
            "cache directory should be removed after SIGTERM"
        );
    }

    #[tokio::test]
    async fn test_daemon_stale_socket() {
        let rt = temp_runtime_dir("stale");
        let socket_path = rt.path().join("sock");
        std::fs::write(&socket_path, "").unwrap();

        let config = Config {
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));
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
        let rt = temp_runtime_dir("cache");
        let socket_path = rt.path().join("sock");
        let config = Config {
            debounce_ms: 100,
            color: false,
            format: Some(
                "{{ change_id }} {{ description }}{% if empty %} EMPTY{% endif %}".to_string(),
            ),
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));
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
        assert!(
            !first.contains("changed"),
            "first should not contain 'changed': {first:?}"
        );

        // Make a change
        Command::new("jj")
            .args(["describe", "-m", "changed"])
            .current_dir(dir.path())
            .output()
            .await
            .unwrap();

        flush_daemon(&socket_path).await;

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

        assert!(
            second.contains("changed"),
            "expected cache to update with description, got: {second:?}"
        );

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }

    async fn create_git_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let run = |args: Vec<String>| {
            let dir_path = dir.path().to_path_buf();
            async move {
                let output = Command::new("git")
                    .args(&args)
                    .current_dir(&dir_path)
                    .output()
                    .await
                    .unwrap();
                assert!(output.status.success(), "git {:?} failed", args);
            }
        };
        run(vec!["init".into()]).await;
        run(vec![
            "config".into(),
            "user.email".into(),
            "test@test.com".into(),
        ])
        .await;
        run(vec!["config".into(), "user.name".into(), "Test".into()]).await;
        std::fs::write(dir.path().join("README"), "init\n").unwrap();
        run(vec!["add".into(), ".".into()]).await;
        run(vec!["commit".into(), "-m".into(), "initial".into()]).await;
        dir
    }

    #[tokio::test]
    async fn test_daemon_serves_git_status() {
        let dir = create_git_repo().await;
        let rt = temp_runtime_dir("git-serves");
        let socket_path = rt.path().join("sock");
        let config = Config {
            color: false,
            format: Some("{% if is_git %}GIT {{ branch }} {{ commit_id }}{% endif %}".to_string()),
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));
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
                assert!(
                    formatted.starts_with("GIT "),
                    "expected git status, got: {formatted:?}"
                );
                assert!(
                    formatted.contains("main") || formatted.contains("master"),
                    "expected branch name, got: {formatted:?}"
                );
            }
            other => panic!("expected Status, got {other:?}"),
        }

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_daemon_multi_repo_concurrent() {
        // Create two repos with different bookmarks
        let dir_a = create_jj_repo().await;
        let dir_b = create_jj_repo().await;

        Command::new("jj")
            .args(["bookmark", "create", "main", "-r", "@"])
            .current_dir(dir_a.path())
            .output()
            .await
            .unwrap();

        Command::new("jj")
            .args(["bookmark", "create", "develop", "-r", "@"])
            .current_dir(dir_b.path())
            .output()
            .await
            .unwrap();

        let rt = temp_runtime_dir("multi");
        let socket_path = rt.path().join("sock");
        let config = Config {
            color: false,
            debounce_ms: 100,
            format: Some("{{ change_id }} {{ description }}{% for b in bookmarks %} {{ b.name }}{% endfor %}{% if empty %} EMPTY{% endif %}".to_string()),
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));
        tokio::time::sleep(Duration::from_millis(200)).await;

        let req_a = Request::Query {
            repo_path: dir_a.path().to_string_lossy().to_string(),
        };
        let req_b = Request::Query {
            repo_path: dir_b.path().to_string_lossy().to_string(),
        };

        // Round 1: initial concurrent queries
        let (resp_a, resp_b) = tokio::join!(
            send_request(&socket_path, &req_a),
            send_request(&socket_path, &req_b),
        );
        let status_a = match resp_a {
            Response::Status { formatted } => formatted,
            other => panic!("expected Status for repo A, got {other:?}"),
        };
        let status_b = match resp_b {
            Response::Status { formatted } => formatted,
            other => panic!("expected Status for repo B, got {other:?}"),
        };
        assert!(
            status_a.contains("main"),
            "repo A round 1: expected 'main', got: {status_a:?}"
        );
        assert!(
            !status_a.contains("develop"),
            "repo A round 1: should not have 'develop', got: {status_a:?}"
        );
        assert!(
            status_b.contains("develop"),
            "repo B round 1: expected 'develop', got: {status_b:?}"
        );
        assert!(
            !status_b.contains("main"),
            "repo B round 1: should not have 'main', got: {status_b:?}"
        );
        assert!(
            status_a.contains("EMPTY"),
            "repo A round 1: expected EMPTY, got: {status_a:?}"
        );
        assert!(
            status_b.contains("EMPTY"),
            "repo B round 1: expected EMPTY, got: {status_b:?}"
        );

        // Mutate both repos: describe repo A, write a file in repo B
        Command::new("jj")
            .args(["describe", "-m", "alpha-change"])
            .current_dir(dir_a.path())
            .output()
            .await
            .unwrap();

        tokio::fs::write(dir_b.path().join("hello.txt"), "world\n")
            .await
            .unwrap();

        flush_daemon(&socket_path).await;

        // Round 2: concurrent queries after mutations
        let (resp_a, resp_b) = tokio::join!(
            send_request(&socket_path, &req_a),
            send_request(&socket_path, &req_b),
        );
        let status_a = match resp_a {
            Response::Status { formatted } => formatted,
            other => panic!("expected Status for repo A round 2, got {other:?}"),
        };
        let status_b = match resp_b {
            Response::Status { formatted } => formatted,
            other => panic!("expected Status for repo B round 2, got {other:?}"),
        };
        assert!(
            status_a.contains("alpha-change"),
            "repo A round 2: expected 'alpha-change', got: {status_a:?}"
        );
        assert!(
            status_a.contains("main"),
            "repo A round 2: expected 'main', got: {status_a:?}"
        );
        assert!(
            !status_a.contains("develop"),
            "repo A round 2: should not have 'develop', got: {status_a:?}"
        );
        assert!(
            status_b.contains("develop"),
            "repo B round 2: expected 'develop', got: {status_b:?}"
        );
        assert!(
            !status_b.contains("main"),
            "repo B round 2: should not have 'main', got: {status_b:?}"
        );
        assert!(
            !status_b.contains("EMPTY"),
            "repo B round 2: should not be EMPTY after file write, got: {status_b:?}"
        );

        // Mutate again: describe repo B, add a bookmark to repo A
        Command::new("jj")
            .args(["describe", "-m", "beta-change"])
            .current_dir(dir_b.path())
            .output()
            .await
            .unwrap();

        Command::new("jj")
            .args(["bookmark", "create", "feature", "-r", "@"])
            .current_dir(dir_a.path())
            .output()
            .await
            .unwrap();

        flush_daemon(&socket_path).await;

        // Round 3: verify both caches updated independently
        let (resp_a, resp_b) = tokio::join!(
            send_request(&socket_path, &req_a),
            send_request(&socket_path, &req_b),
        );
        let status_a = match resp_a {
            Response::Status { formatted } => formatted,
            other => panic!("expected Status for repo A round 3, got {other:?}"),
        };
        let status_b = match resp_b {
            Response::Status { formatted } => formatted,
            other => panic!("expected Status for repo B round 3, got {other:?}"),
        };
        assert!(
            status_a.contains("main"),
            "repo A round 3: expected 'main', got: {status_a:?}"
        );
        assert!(
            status_a.contains("feature"),
            "repo A round 3: expected 'feature', got: {status_a:?}"
        );
        assert!(
            status_a.contains("alpha-change"),
            "repo A round 3: expected 'alpha-change', got: {status_a:?}"
        );
        assert!(
            !status_a.contains("develop"),
            "repo A round 3: should not have 'develop', got: {status_a:?}"
        );
        assert!(
            status_b.contains("beta-change"),
            "repo B round 3: expected 'beta-change', got: {status_b:?}"
        );
        assert!(
            status_b.contains("develop"),
            "repo B round 3: expected 'develop', got: {status_b:?}"
        );
        assert!(
            !status_b.contains("main"),
            "repo B round 3: should not have 'main', got: {status_b:?}"
        );
        assert!(
            !status_b.contains("feature"),
            "repo B round 3: should not have 'feature', got: {status_b:?}"
        );

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }
}
