use anyhow::Result;
use std::cmp::max;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, Notify, mpsc, watch};
use tokio::time::{Duration, Instant};

use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::git::{GitWorkerRequest, query_git_status, spawn_git_worker};
use crate::jj::{JjWorkerRequest, query_jj_status, spawn_jj_worker};
use crate::protocol::{DaemonStats, Request, Response, VcsKind};
use crate::template::RepoStatus;
use crate::template::format_not_ready;
use crate::template::format_status;
use crate::watcher::{RepoWatcher, WatchEvent, watch_repo};

struct DaemonState {
    cache: HashMap<PathBuf, (RepoStatus, String)>,
    watchers: HashMap<PathBuf, RepoWatcher>,
    /// Maps arbitrary directories to their repo root and VCS kind. Negatives are not cached.
    dir_to_repo: HashMap<PathBuf, (PathBuf, VcsKind)>,
    last_query: Instant,
    started_at: Instant,
    config: Config,
    cache_dir: PathBuf,
    stats: DaemonStats,
    /// Repos currently being refreshed by watcher-triggered refresh_repo.
    refreshing: HashSet<PathBuf>,
    /// Per-repo watch channels notified when cache is updated.
    cache_watch: HashMap<PathBuf, watch::Sender<u64>>,
}

impl DaemonState {
    /// Insert a cache entry and notify any waiting clients.
    fn update_cache(&mut self, repo_path: &Path, status: RepoStatus, formatted: String) {
        self.cache
            .insert(repo_path.to_path_buf(), (status, formatted));
        if let Some(tx) = self.cache_watch.get(repo_path) {
            let val = *tx.borrow() + 1;
            let _ = tx.send(val);
        }
    }

    /// Get a watch receiver for a repo's cache updates.
    fn subscribe_cache(&mut self, repo_path: &Path) -> watch::Receiver<u64> {
        self.cache_watch
            .entry(repo_path.to_path_buf())
            .or_insert_with(|| watch::channel(0).0)
            .subscribe()
    }
}

use crate::config::find_repo_root;

/// Maximum log file size before rotation (5 MB).
const MAX_LOG_SIZE: u64 = 5 * 1024 * 1024;

/// Number of recent query durations to keep for percentile stats.
const TIMING_RING_SIZE: usize = 100;

pub fn init_logging(runtime_dir: &Path) {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

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

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_appender)
        .with_ansi(false)
        .with_filter(filter);

    let registry = tracing_subscriber::registry().with(fmt_layer);

    #[cfg(feature = "tokio-console")]
    {
        let console_layer = console_subscriber::spawn();
        registry.with(console_layer).init();
    }

    #[cfg(not(feature = "tokio-console"))]
    {
        registry.init();
    }
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

    // Write version file so clients can detect version mismatches without a socket round-trip
    let (version, git_hash, _) = crate::protocol::version_info();
    std::fs::write(runtime_dir.join("version"), format!("{version} {git_hash}")).ok();

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
        stats: DaemonStats::default(),
        refreshing: HashSet::new(),
        cache_watch: HashMap::new(),
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
                let _ = std::fs::remove_file(runtime_dir.join("version"));
                let _ = std::fs::remove_file(runtime_dir.join("version_warned"));
                return Ok(());
            }
        }
    }
}

#[tracing::instrument(skip_all)]
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
        Request::Query {
            repo_path,
            timeout_override_ms,
        } => {
            let query_path = PathBuf::from(&repo_path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(&repo_path));

            // Resolve the repo root and VCS kind from the given path
            let query_start = Instant::now();
            let (repo_path, vcs_kind, cached, config, cd) = {
                let mut st = state.lock().await;
                st.last_query = Instant::now();
                st.stats.queries += 1;

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

                let cached = st.cache.get(&repo_path).map(|(_, f)| f.clone());
                let config = st.config.clone();
                let cache_dir = st.cache_dir.clone();
                (repo_path, vcs_kind, cached, config, cache_dir)
            };

            if let Some(cached) = cached {
                tracing::debug!(repo = %repo_path.display(), "cache hit");

                // If a refresh is in progress and wait is configured, wait for fresh data
                if config.query_timeout_ms > 0 || timeout_override_ms > 0 {
                    let rx = {
                        let mut st = state.lock().await;
                        if st.refreshing.contains(&repo_path) {
                            Some(st.subscribe_cache(&repo_path))
                        } else {
                            None
                        }
                    };

                    if let Some(mut rx) = rx {
                        tracing::debug!(repo = %repo_path.display(), timeout_ms = config.query_timeout_ms, "waiting for in-flight refresh");
                        if let Ok(Ok(())) = tokio::time::timeout(
                            Duration::from_millis(max(
                                config.query_timeout_ms,
                                timeout_override_ms,
                            )),
                            rx.changed(),
                        )
                        .await
                        {
                            // Fresh data available
                            let mut st = state.lock().await;
                            if let Some((_, fresh)) = st.cache.get(&repo_path).cloned() {
                                st.stats.cache_hits += 1;
                                record_timing(&mut st.stats, query_start.elapsed());
                                drop(st);
                                if query_path != repo_path {
                                    link_cache_file(&cd, &repo_path, &query_path);
                                }
                                return send_response(
                                    &mut writer,
                                    Response::Status { formatted: fresh },
                                )
                                .await;
                            }
                        }
                        // Timeout — fall through to return stale cached value
                    }
                }

                {
                    let mut st = state.lock().await;
                    st.stats.cache_hits += 1;
                    record_timing(&mut st.stats, query_start.elapsed());
                }
                // Ensure the queried directory has a hardlink to the repo root's cache file
                if query_path != repo_path {
                    link_cache_file(&cd, &repo_path, &query_path);
                }
                send_response(&mut writer, Response::Status { formatted: cached }).await
            } else {
                // Cache miss — populate in the background
                tracing::debug!(repo = %repo_path.display(), vcs = ?vcs_kind, "cache miss");
                let query_timeout_ms = config.query_timeout_ms;
                let rx = {
                    let mut st = state.lock().await;
                    st.stats.cache_misses += 1;
                    if query_timeout_ms > 0 {
                        Some(st.subscribe_cache(&repo_path))
                    } else {
                        None
                    }
                };

                let not_ready = format_not_ready(&config.resolved_not_ready_format(), config.color);
                let state_bg = state.clone();
                let query_path_bg = query_path.clone();
                let repo_path_bg = repo_path.clone();
                let cd_bg = cd.clone();
                tokio::spawn(async move {
                    let result = match vcs_kind {
                        VcsKind::Jj => query_jj_status(&repo_path_bg, &config).await,
                        VcsKind::Git => query_git_status(&repo_path_bg, &config).await,
                    };
                    match result {
                        Ok(status) => {
                            let formatted =
                                format_status(&status, &config.resolved_format(), config.color);
                            write_cache_file(&cd_bg, &repo_path_bg, &formatted);
                            if query_path_bg != repo_path_bg {
                                link_cache_file(&cd_bg, &repo_path_bg, &query_path_bg);
                            }
                            state_bg
                                .lock()
                                .await
                                .update_cache(&repo_path_bg, status, formatted);
                        }
                        Err(e) => {
                            tracing::error!(repo = %repo_path_bg.display(), error = %e, "background status query failed");
                        }
                    }
                });

                // If timeout configured, wait for background task to complete
                if let Some(mut rx) = rx {
                    tracing::debug!(repo = %repo_path.display(), timeout_ms = query_timeout_ms, "waiting for initial scan");
                    if let Ok(Ok(())) =
                        tokio::time::timeout(Duration::from_millis(query_timeout_ms), rx.changed())
                            .await
                    {
                        let mut st = state.lock().await;
                        if let Some((_, formatted)) = st.cache.get(&repo_path).cloned() {
                            record_timing(&mut st.stats, query_start.elapsed());
                            drop(st);
                            if query_path != repo_path {
                                link_cache_file(&cd, &repo_path, &query_path);
                            }
                            return send_response(&mut writer, Response::Status { formatted })
                                .await;
                        }
                    }
                }

                send_response(
                    &mut writer,
                    Response::NotReady {
                        formatted: not_ready,
                    },
                )
                .await
            }
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
            let mut stats = st.stats.clone();
            stats.fs_events_ignored = st
                .watchers
                .values()
                .map(|w| w.ignored_events.load(std::sync::atomic::Ordering::Relaxed))
                .sum();
            drop(st);
            send_response(
                &mut writer,
                Response::DaemonStatus {
                    pid: std::process::id(),
                    uptime_secs,
                    watched_repos,
                    stats,
                },
            )
            .await
        }
        Request::Version => {
            let (version, git_hash, features) = crate::protocol::version_info();
            send_response(
                &mut writer,
                Response::Version {
                    version,
                    git_hash,
                    features,
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

fn record_timing(stats: &mut DaemonStats, elapsed: Duration) {
    let ms = elapsed.as_secs_f64() * 1000.0;
    if stats.recent_query_ms.len() >= TIMING_RING_SIZE {
        stats.recent_query_ms.remove(0);
    }
    stats.recent_query_ms.push(ms);
}

/// Per-repo refresh state: tracks whether a re-refresh is needed while one is in flight.
enum RepoRefreshState {
    /// A refresh task is running, no new events queued.
    InFlight,
    /// A refresh task is running AND new events arrived — re-refresh needed after completion.
    /// Fields: vcs_kind, working_copy_changed, accumulated changed_paths.
    Pending(VcsKind, bool, Vec<PathBuf>),
}

/// Channel message sent when a per-repo refresh task completes.
struct RefreshDone {
    repo_path: PathBuf,
}

#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip_all, fields(repo = %repo_path.display(), vcs = ?vcs_kind))]
async fn refresh_repo(
    repo_path: PathBuf,
    vcs_kind: VcsKind,
    working_copy_changed: bool,
    changed_paths: Vec<PathBuf>,
    state: Arc<Mutex<DaemonState>>,
    jj_worker: mpsc::UnboundedSender<JjWorkerRequest>,
    git_worker: mpsc::UnboundedSender<GitWorkerRequest>,
    done_tx: mpsc::UnboundedSender<RefreshDone>,
) {
    let refresh_start = Instant::now();
    let (config, cd) = {
        let mut st = state.lock().await;
        st.refreshing.insert(repo_path.clone());
        let config = st.config.clone();
        let cd = st.cache_dir.clone();

        // Mark existing cache as stale immediately so the prompt reflects
        // that a refresh is in progress, before the VCS query completes.
        if let Some((prev_status, _)) = st.cache.get(&repo_path) {
            let mut stale_status = prev_status.clone();
            stale_status.is_stale = true;
            stale_status.refresh_error.clear();
            let formatted = format_status(&stale_status, &config.resolved_format(), config.color);
            write_cache_file(&cd, &repo_path, &formatted);
            st.update_cache(&repo_path, stale_status, formatted);
        }

        (config, cd)
    };

    let result = match vcs_kind {
        VcsKind::Jj if working_copy_changed && !changed_paths.is_empty() => {
            // Try incremental update via jj worker
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let _ = jj_worker.send(JjWorkerRequest::IncrementalUpdate {
                repo_path: repo_path.clone(),
                changed_paths,
                reply: reply_tx,
            });
            match reply_rx.await {
                Ok(Ok(status)) => Ok(status),
                Ok(Err(_)) => {
                    // No incremental state — fall back to full refresh
                    jj_full_refresh(&repo_path, &config, &jj_worker).await
                }
                Err(_) => Err(anyhow::anyhow!("jj worker channel closed")),
            }
        }
        VcsKind::Jj => {
            // VCS-internal event or no paths — full refresh
            jj_full_refresh(&repo_path, &config, &jj_worker).await
        }
        VcsKind::Git if working_copy_changed && !changed_paths.is_empty() => {
            // Try incremental update via git worker
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let _ = git_worker.send(GitWorkerRequest::IncrementalUpdate {
                repo_path: repo_path.clone(),
                changed_paths,
                reply: reply_tx,
            });
            match reply_rx.await {
                Ok(Ok(status)) => Ok(status),
                Ok(Err(_)) => {
                    // No incremental state — fall back to full refresh
                    git_full_refresh(&repo_path, &git_worker).await
                }
                Err(_) => Err(anyhow::anyhow!("git worker channel closed")),
            }
        }
        VcsKind::Git => {
            // VCS-internal event or no paths — full refresh
            git_full_refresh(&repo_path, &git_worker).await
        }
    };

    match result {
        Ok(status) => {
            let formatted = format_status(&status, &config.resolved_format(), config.color);
            write_cache_file(&cd, &repo_path, &formatted);
            let mut st = state.lock().await;
            st.refreshing.remove(&repo_path);
            st.update_cache(&repo_path, status, formatted);
            st.stats.refreshes += 1;
            record_timing(&mut st.stats, refresh_start.elapsed());
        }
        Err(e) => {
            tracing::error!(repo = %repo_path.display(), error = %e, "refresh failed");
            let mut st = state.lock().await;
            st.refreshing.remove(&repo_path);
            if let Some((prev_status, _)) = st.cache.get(&repo_path) {
                let mut stale_status = prev_status.clone();
                stale_status.is_stale = true;
                stale_status.refresh_error = e.to_string();
                let formatted =
                    format_status(&stale_status, &config.resolved_format(), config.color);
                write_cache_file(&cd, &repo_path, &formatted);
                st.update_cache(&repo_path, stale_status, formatted);
            }
        }
    }
    let _ = done_tx.send(RefreshDone { repo_path });
}

/// Full jj refresh via the worker thread (replaces incremental state).
async fn jj_full_refresh(
    repo_path: &Path,
    config: &Config,
    jj_worker: &mpsc::UnboundedSender<JjWorkerRequest>,
) -> Result<crate::template::RepoStatus> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let _ = jj_worker.send(JjWorkerRequest::FullRefresh {
        repo_path: repo_path.to_path_buf(),
        depth: config.bookmark_search_depth,
        reply: reply_tx,
    });
    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("jj worker channel closed"))?
}

/// Full git refresh via the worker thread (replaces incremental state).
async fn git_full_refresh(
    repo_path: &Path,
    git_worker: &mpsc::UnboundedSender<GitWorkerRequest>,
) -> Result<crate::template::RepoStatus> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let _ = git_worker.send(GitWorkerRequest::FullRefresh {
        repo_path: repo_path.to_path_buf(),
        reply: reply_tx,
    });
    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("git worker channel closed"))?
}

#[tracing::instrument(skip_all)]
async fn refresh_task(
    state: Arc<Mutex<DaemonState>>,
    mut watch_rx: mpsc::UnboundedReceiver<WatchEvent>,
) {
    // Per-repo concurrency control: at most one refresh per repo at a time.
    let mut in_flight: HashMap<PathBuf, RepoRefreshState> = HashMap::new();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<RefreshDone>();
    let jj_worker = spawn_jj_worker();
    let git_worker = spawn_git_worker();

    loop {
        tokio::select! {
            event = watch_rx.recv() => {
                let Some(event) = event else { return };

                match event {
                    WatchEvent::Flush(tx) => {
                        // Wait for all in-flight refreshes to complete
                        while !in_flight.is_empty() {
                            if let Some(done) = done_rx.recv().await {
                                handle_refresh_done(
                                    &done, &mut in_flight, &jj_worker, &git_worker, &state, &done_tx,
                                );
                            }
                        }
                        let _ = tx.send(());
                    }
                    WatchEvent::Change { repo_path, vcs_kind, working_copy_changed, changed_paths } => {
                        state.lock().await.stats.fs_events += 1;

                        match in_flight.get_mut(&repo_path) {
                            None => {
                                // No refresh running — start one immediately
                                in_flight.insert(repo_path.clone(), RepoRefreshState::InFlight);
                                tokio::spawn(refresh_repo(
                                    repo_path, vcs_kind, working_copy_changed, changed_paths,
                                    state.clone(), jj_worker.clone(), git_worker.clone(), done_tx.clone(),
                                ));
                            }
                            Some(entry) => {
                                // Refresh already running — queue a re-refresh.
                                // working_copy_changed=true wins; accumulate paths.
                                match entry {
                                    RepoRefreshState::InFlight => {
                                        *entry = RepoRefreshState::Pending(vcs_kind, working_copy_changed, changed_paths);
                                    }
                                    RepoRefreshState::Pending(_, wc, paths) => {
                                        *wc = *wc || working_copy_changed;
                                        paths.extend(changed_paths);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            done = done_rx.recv() => {
                if let Some(done) = done {
                    handle_refresh_done(&done, &mut in_flight, &jj_worker, &git_worker, &state, &done_tx);
                }
            }
        }
    }
}

fn handle_refresh_done(
    done: &RefreshDone,
    in_flight: &mut HashMap<PathBuf, RepoRefreshState>,
    jj_worker: &mpsc::UnboundedSender<JjWorkerRequest>,
    git_worker: &mpsc::UnboundedSender<GitWorkerRequest>,
    state: &Arc<Mutex<DaemonState>>,
    done_tx: &mpsc::UnboundedSender<RefreshDone>,
) {
    match in_flight.remove(&done.repo_path) {
        Some(RepoRefreshState::Pending(vcs_kind, wc, paths)) if wc => {
            // Working copy changed while refreshing — re-refresh immediately
            in_flight.insert(done.repo_path.clone(), RepoRefreshState::InFlight);
            tokio::spawn(refresh_repo(
                done.repo_path.clone(),
                vcs_kind,
                wc,
                paths,
                state.clone(),
                jj_worker.clone(),
                git_worker.clone(),
                done_tx.clone(),
            ));
        }
        Some(RepoRefreshState::Pending(..)) => {
            // Only VCS-internal changes arrived during refresh — skip to
            // avoid infinite loop (our own queries create VCS events).
            tracing::debug!(repo = %done.repo_path.display(), "skipping re-refresh for VCS-internal-only events");
        }
        _ => {
            // Done, no pending work for this repo
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
        let mut stream = None;
        for _ in 0..50 {
            if let Ok(res) = UnixStream::connect(socket_path).await {
                stream = Some(res);
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let stream = stream.expect("Stream should have been created");
        let (reader, mut writer) = stream.into_split();
        let mut json = serde_json::to_string(request).unwrap();
        json.push('\n');
        writer.write_all(json.as_bytes()).await.unwrap();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    /// Wait for the daemon socket to appear.
    async fn wait_for_socket(socket_path: &std::path::Path) {
        for _ in 0..2000 {
            if socket_path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("socket never appeared at {}", socket_path.display());
    }

    /// Query the daemon and wait until it returns a Status (retrying through NotReady).
    async fn query_until_ready(socket_path: &std::path::Path, repo_path: &str) -> String {
        wait_for_socket(socket_path).await;
        let request = Request::test_query(repo_path.to_string());
        for _ in 0..2000 {
            let resp = send_request(socket_path, &request).await;
            match resp {
                Response::Status { formatted } => return formatted,
                Response::NotReady { .. } => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    continue;
                }
                other => panic!("expected Status or NotReady, got {other:?}"),
            }
        }
        panic!("timed out waiting for Status response");
    }

    /// Flush the daemon, query, and retry until `pred` matches the formatted status.
    async fn query_until_match(
        socket_path: &std::path::Path,
        repo_path: &str,
        pred: impl Fn(&str) -> bool,
    ) -> String {
        let request = Request::test_query(repo_path.to_string());
        for _ in 0..2000 {
            let _ = send_request(socket_path, &Request::Flush).await;
            let resp = send_request(socket_path, &request).await;
            if let Response::Status { ref formatted } = resp
                && pred(formatted)
            {
                return formatted.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        // One final attempt to get a diagnostic message
        let resp = send_request(socket_path, &request).await;
        panic!("timed out waiting for condition on {repo_path}, last response: {resp:?}");
    }

    fn temp_runtime_dir(suffix: &str) -> TempDir {
        TempDir::with_prefix(format!("vcs-test-{suffix}-")).unwrap()
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

        let formatted = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(!formatted.is_empty(), "expected non-empty status");

        // Shutdown
        let resp = send_request(&socket_path, &Request::Shutdown).await;
        assert_eq!(resp, Response::Ok);
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_daemon_returns_not_ready_then_status() {
        let dir = create_jj_repo().await;
        let rt = temp_runtime_dir("notready");
        let socket_path = rt.path().join("sock");
        let config = Config {
            color: false,
            query_timeout_ms: 0, // disable waiting to test NotReady behavior
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));

        let query = Request::Query {
            repo_path: dir.path().to_string_lossy().to_string(),
            timeout_override_ms: 0,
        };

        // First query should return NotReady (cache is cold)
        let resp = send_request(&socket_path, &query).await;
        assert!(
            matches!(resp, Response::NotReady { .. }),
            "first query should be NotReady, got {resp:?}"
        );

        // Retry until background population completes and we get Status
        let mut got_status = false;
        for _ in 0..2000 {
            let resp = send_request(&socket_path, &query).await;
            if let Response::Status { formatted } = resp {
                assert!(!formatted.is_empty(), "expected non-empty cached status");
                got_status = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(got_status, "timed out waiting for Status after NotReady");

        let _ = send_request(&socket_path, &Request::Shutdown).await;
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

        // Query from a subdirectory — daemon should resolve the repo root
        let formatted = query_until_ready(&socket_path, &sub.to_string_lossy()).await;
        assert!(
            !formatted.is_empty(),
            "expected non-empty status from subdirectory query"
        );

        // Query from the repo root should return the same result (cached via dir_to_repo)
        let formatted2 = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(
            !formatted2.is_empty(),
            "expected non-empty status from root query"
        );

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_daemon_writes_cache_for_subdirectory() {
        let dir = create_jj_repo().await;
        let sub = dir.path().join("src").join("nested");
        std::fs::create_dir_all(&sub).unwrap();

        let rt = temp_runtime_dir("cache-subdir");
        let socket_path = rt.path().join("sock");
        let cache_dir = rt.path().join("cache");
        let config = Config {
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));

        // Query from the subdirectory and wait for cache to populate
        let formatted = query_until_ready(&socket_path, &sub.to_string_lossy()).await;
        assert!(!formatted.is_empty(), "expected non-empty status");

        // Both the repo root and subdirectory should have cache files
        let root_cache = cache_file_in(&cache_dir, dir.path());
        let sub_cache = cache_file_in(&cache_dir, &sub);
        assert!(
            root_cache.exists(),
            "cache file missing for repo root: {}",
            root_cache.display()
        );
        assert!(
            sub_cache.exists(),
            "cache file missing for subdirectory: {}",
            sub_cache.display()
        );

        // They should be hardlinked (same inode)
        use std::os::unix::fs::MetadataExt;
        let root_ino = std::fs::metadata(&root_cache).unwrap().ino();
        let sub_ino = std::fs::metadata(&sub_cache).unwrap().ino();
        assert_eq!(
            root_ino, sub_ino,
            "cache files should be hardlinked (same inode)"
        );

        // Content should match and be non-empty
        let content = std::fs::read_to_string(&root_cache).unwrap();
        assert!(!content.is_empty(), "cache file should not be empty");

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
        wait_for_socket(&socket_path).await;

        let resp = send_request(
            &socket_path,
            &Request::test_query(dir.path().to_string_lossy().to_string()),
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
        wait_for_socket(&socket_path).await;

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

        // Query to populate the cache
        let formatted = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(!formatted.is_empty());

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
        for _ in 0..2000 {
            if socket_path.exists() {
                return child;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
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
        let formatted = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(!formatted.is_empty());

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

        let dir = create_jj_repo().await;
        let formatted = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(!formatted.is_empty());

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_daemon_cache_update() {
        let dir = create_jj_repo().await;
        let rt = temp_runtime_dir("cache");
        let socket_path = rt.path().join("sock");
        let config = Config {
            color: false,
            format: Some(
                "{{ change_id }} {{ description }}{% if empty %} EMPTY{% endif %}".to_string(),
            ),
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));

        // First query — wait for cache to populate
        let first = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
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

        // Retry flush+query until the change is reflected
        let second = query_until_match(&socket_path, &dir.path().to_string_lossy(), |s| {
            s.contains("changed")
        })
        .await;

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

        let formatted = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(
            formatted.starts_with("GIT "),
            "expected git status, got: {formatted:?}"
        );
        assert!(
            formatted.contains("main") || formatted.contains("master"),
            "expected branch name, got: {formatted:?}"
        );

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
            format: Some("{{ change_id }} {{ description }}{% for b in bookmarks %} {{ b.name }}{% endfor %}{% if empty %} EMPTY{% endif %}".to_string()),
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));

        // Round 1: initial queries — wait for cache to populate
        let path_a = dir_a.path().to_string_lossy().to_string();
        let path_b = dir_b.path().to_string_lossy().to_string();
        let (status_a, status_b) = tokio::join!(
            query_until_ready(&socket_path, &path_a),
            query_until_ready(&socket_path, &path_b),
        );
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
        // Snapshot so jj-lib sees the working copy change
        Command::new("jj")
            .args(["status"])
            .current_dir(dir_b.path())
            .output()
            .await
            .unwrap();

        // Round 2: retry until mutations are reflected
        let (status_a, status_b) = tokio::join!(
            query_until_match(&socket_path, &path_a, |s| s.contains("alpha-change")),
            query_until_match(&socket_path, &path_b, |s| !s.contains("EMPTY")),
        );
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

        // Round 3: retry until both caches updated independently
        let (status_a, status_b) = tokio::join!(
            query_until_match(&socket_path, &path_a, |s| s.contains("feature")),
            query_until_match(&socket_path, &path_b, |s| s.contains("beta-change")),
        );
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

    #[tokio::test]
    async fn test_daemon_query_timeout_waits_for_result() {
        let dir = create_jj_repo().await;
        let rt = temp_runtime_dir("qtimeout");
        let socket_path = rt.path().join("sock");
        let config = Config {
            color: false,
            query_timeout_ms: 5000, // generous timeout so the scan completes
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));
        wait_for_socket(&socket_path).await;

        let query = Request::test_query(dir.path().to_string_lossy().to_string());

        // With query_timeout_ms, the first query should wait and return Status (not NotReady)
        let resp = send_request(&socket_path, &query).await;
        match resp {
            Response::Status { formatted } => {
                assert!(!formatted.is_empty(), "expected non-empty status");
            }
            Response::NotReady { .. } => {
                panic!(
                    "with query_timeout_ms=5000, first query should wait and return Status, not NotReady"
                );
            }
            other => panic!("unexpected response: {other:?}"),
        }

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_daemon_stale_on_refresh_error() {
        let dir = create_jj_repo().await;
        let rt = temp_runtime_dir("stale-refresh");
        let socket_path = rt.path().join("sock");
        let config = Config {
            color: false,
            format: Some("{{ change_id }}{% if is_stale %} STALE{% endif %}".to_string()),
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));

        // Populate the cache with a successful status
        let first = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(
            !first.contains("STALE"),
            "initial status should not be stale: {first:?}"
        );

        // Corrupt the repo so jj-lib will fail on next refresh
        let repo_dir = dir.path().join(".jj").join("repo");
        std::fs::remove_dir_all(&repo_dir).unwrap();

        // Write a file to trigger a file-system event and refresh
        std::fs::write(dir.path().join("trigger.txt"), "trigger\n").unwrap();

        // Wait for the stale indicator to appear
        let stale = query_until_match(&socket_path, &dir.path().to_string_lossy(), |s| {
            s.contains("STALE")
        })
        .await;
        assert!(
            stale.contains("STALE"),
            "expected STALE after corruption: {stale:?}"
        );
        // Original change_id should still be present
        assert!(!stale.is_empty(), "stale status should still contain data");

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_daemon_stale_clears_on_recovery() {
        let dir = create_jj_repo().await;
        let rt = temp_runtime_dir("stale-recover");
        let socket_path = rt.path().join("sock");
        let config = Config {
            color: false,
            format: Some("{{ change_id }}{% if is_stale %} STALE{% endif %}".to_string()),
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));

        // Populate cache
        let first = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(!first.contains("STALE"));

        // Corrupt
        let repo_dir = dir.path().join(".jj").join("repo");
        let backup = TempDir::new().unwrap();
        // Copy instead of delete so we can restore
        Command::new("cp")
            .args([
                "-a",
                &repo_dir.to_string_lossy(),
                &backup.path().join("repo").to_string_lossy(),
            ])
            .output()
            .await
            .unwrap();
        std::fs::remove_dir_all(&repo_dir).unwrap();

        // Trigger refresh — should become stale
        std::fs::write(dir.path().join("trigger.txt"), "trigger\n").unwrap();
        let stale = query_until_match(&socket_path, &dir.path().to_string_lossy(), |s| {
            s.contains("STALE")
        })
        .await;
        assert!(stale.contains("STALE"));

        // Restore the repo
        Command::new("cp")
            .args([
                "-a",
                &backup.path().join("repo").to_string_lossy(),
                &repo_dir.to_string_lossy(),
            ])
            .output()
            .await
            .unwrap();

        // Trigger another refresh — should clear staleness
        std::fs::write(dir.path().join("trigger2.txt"), "recover\n").unwrap();
        let recovered = query_until_match(&socket_path, &dir.path().to_string_lossy(), |s| {
            !s.contains("STALE")
        })
        .await;
        assert!(
            !recovered.contains("STALE"),
            "staleness should clear after recovery: {recovered:?}"
        );

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_daemon_stale_git_on_refresh_error() {
        let dir = create_git_repo().await;
        let rt = temp_runtime_dir("stale-git");
        let socket_path = rt.path().join("sock");
        let config = Config {
            color: false,
            format: Some("{{ branch }}{% if is_stale %} STALE{% endif %}".to_string()),
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));

        // Populate cache
        let first = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(
            !first.contains("STALE"),
            "initial should not be stale: {first:?}"
        );

        // Corrupt git repo by removing .git/HEAD
        std::fs::remove_file(dir.path().join(".git").join("HEAD")).unwrap();

        // Trigger refresh
        std::fs::write(dir.path().join("trigger.txt"), "trigger\n").unwrap();

        // Wait for stale
        let stale = query_until_match(&socket_path, &dir.path().to_string_lossy(), |s| {
            s.contains("STALE")
        })
        .await;
        assert!(
            stale.contains("STALE"),
            "expected STALE after git corruption: {stale:?}"
        );

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_daemon_query_timeout_zero_returns_not_ready() {
        let dir = create_jj_repo().await;
        let rt = temp_runtime_dir("qtimeout0");
        let socket_path = rt.path().join("sock");
        let config = Config {
            color: false,
            query_timeout_ms: 0, // default — no waiting
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon(config, rt.path().to_path_buf()));
        wait_for_socket(&socket_path).await;

        let query = Request::test_query(dir.path().to_string_lossy().to_string());

        // With query_timeout_ms=0, the first query should return NotReady (current behavior)
        let resp = send_request(&socket_path, &query).await;
        assert!(
            matches!(resp, Response::NotReady { .. }),
            "with query_timeout_ms=0, first query should be NotReady, got {resp:?}"
        );

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }
}
