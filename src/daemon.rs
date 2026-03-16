use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, Notify, mpsc};
use tokio::time::{Duration, Instant};

use crate::config::Config;
use crate::git::query_git_status;
use crate::jj::{format_status, query_jj_status};
use crate::protocol::{Request, Response, VcsKind};
use crate::watcher::{RepoWatcher, WatchEvent, watch_repo};

struct DaemonState {
    cache: HashMap<PathBuf, String>,
    watchers: HashMap<PathBuf, RepoWatcher>,
    /// Maps arbitrary directories to their repo root and VCS kind. Negatives are not cached.
    dir_to_repo: HashMap<PathBuf, (PathBuf, VcsKind)>,
    last_query: Instant,
    config: Config,
}

/// Find the repo root and VCS kind. jj wins if both `.jj/` and `.git/` are present.
fn find_repo_root(start: &Path) -> Option<(PathBuf, VcsKind)> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".jj").is_dir() {
            return Some((dir, VcsKind::Jj));
        }
        if dir.join(".git").exists() {
            return Some((dir, VcsKind::Git));
        }
        if !dir.pop() {
            return None;
        }
    }
}

pub async fn run_daemon(config: Config, socket_path: PathBuf) -> Result<()> {
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
        dir_to_repo: HashMap::new(),
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
            let query_path = PathBuf::from(&repo_path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(&repo_path));

            // Resolve the repo root and VCS kind from the given path
            let (repo_path, vcs_kind, cached, config) = {
                let mut st = state.lock().await;
                st.last_query = Instant::now();

                let resolved = if let Some(entry) = st.dir_to_repo.get(&query_path) {
                    Some(entry.clone())
                } else if let Some(found) = find_repo_root(&query_path) {
                    st.dir_to_repo.insert(query_path, found.clone());
                    Some(found)
                } else {
                    None
                };

                let Some((repo_path, vcs_kind)) = resolved else {
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
                (repo_path, vcs_kind, cached, config)
            };

            let formatted = if let Some(cached) = cached {
                cached
            } else {
                let result = match vcs_kind {
                    VcsKind::Jj => query_jj_status(&repo_path, &config, false).await,
                    VcsKind::Git => query_git_status(&repo_path, &config).await,
                };
                match result {
                    Ok(status) => {
                        let formatted = format_status(&status, &config.format, config.color);
                        state
                            .lock()
                            .await
                            .cache
                            .insert(repo_path, formatted.clone());
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

/// Tracks per-repo: (vcs_kind, working_copy_changed)
fn collect_change(
    pending: &mut HashMap<PathBuf, (VcsKind, bool)>,
    event: WatchEvent,
) {
    match event {
        WatchEvent::Change {
            repo_path,
            vcs_kind,
            working_copy_changed,
        } => {
            if working_copy_changed {
                pending.insert(repo_path, (vcs_kind, true));
            } else {
                pending
                    .entry(repo_path)
                    .or_insert((vcs_kind, false));
            }
        }
        WatchEvent::Flush(_) => {} // handled by caller
    }
}

async fn process_pending(
    state: &Arc<Mutex<DaemonState>>,
    pending: &mut HashMap<PathBuf, (VcsKind, bool)>,
) {
    let repos: Vec<(PathBuf, VcsKind, bool)> = pending
        .drain()
        .map(|(p, (v, wc))| (p, v, wc))
        .collect();
    for (repo_path, vcs_kind, needs_snapshot) in repos {
        let config = state.lock().await.config.clone();
        let result = match vcs_kind {
            VcsKind::Jj => {
                let ignore_wc = !needs_snapshot;
                query_jj_status(&repo_path, &config, ignore_wc).await
            }
            VcsKind::Git => query_git_status(&repo_path, &config).await,
        };
        match result {
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

    fn temp_socket_path(suffix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("jj-test-{}-{suffix}.sock", std::process::id()))
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
        let socket_path = temp_socket_path("serves");
        let _ = std::fs::remove_file(&socket_path);
        let config = Config {
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, socket_path.clone()));
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

        let socket_path = temp_socket_path("subdir");
        let _ = std::fs::remove_file(&socket_path);
        let config = Config {
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, socket_path.clone()));
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

        let socket_path = temp_socket_path("norepo");
        let _ = std::fs::remove_file(&socket_path);
        let config = Config {
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, socket_path.clone()));
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
        let socket_path = temp_socket_path("shutdown");
        let _ = std::fs::remove_file(&socket_path);
        let config = Config {
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, socket_path.clone()));
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
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, socket_path.clone()));
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
            debounce_ms: 100,
            color: false,
            format: "{{ change_id }} {{ description }}{% if empty %} EMPTY{% endif %}".to_string(),
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, socket_path.clone()));
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
        run(vec!["config".into(), "user.email".into(), "test@test.com".into()]).await;
        run(vec!["config".into(), "user.name".into(), "Test".into()]).await;
        std::fs::write(dir.path().join("README"), "init\n").unwrap();
        run(vec!["add".into(), ".".into()]).await;
        run(vec!["commit".into(), "-m".into(), "initial".into()]).await;
        dir
    }

    #[tokio::test]
    async fn test_daemon_serves_git_status() {
        let dir = create_git_repo().await;
        let socket_path = temp_socket_path("git-serves");
        let _ = std::fs::remove_file(&socket_path);
        let config = Config {
            color: false,
            format: "{% if is_git %}GIT {{ branch }} {{ commit_id }}{% endif %}".to_string(),
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, socket_path.clone()));
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

        let socket_path = temp_socket_path("multi");
        let _ = std::fs::remove_file(&socket_path);
        let config = Config {
            color: false,
            debounce_ms: 100,
            format: "{{ change_id }} {{ description }}{% for b in bookmarks %} {{ b.name }}{% endfor %}{% if empty %} EMPTY{% endif %}".to_string(),
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, socket_path.clone()));
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
