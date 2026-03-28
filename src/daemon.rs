use anyhow::Result;
use std::cmp::max;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, Notify, mpsc, watch};
use tokio::time::{Duration, Instant};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::reload;

pub type LogFilterHandle = reload::Handle<EnvFilter, tracing_subscriber::Registry>;

use crate::config::Config;
use crate::git::{GitWorkerRequest, query_git_status, spawn_git_worker};
use crate::jj::{JjWorkerRequest, query_jj_status, spawn_jj_worker};
use crate::protocol::{DaemonStats, Request, Response, VcsKind};
use crate::template::RepoStatus;
use crate::template::format_not_ready;
use crate::template::format_status;
use crate::watcher::{RepoWatcher, VcsChangeHint, WatchEvent, watch_repo};

struct DaemonState {
    cache: HashMap<PathBuf, (RepoStatus, String)>,
    watchers: HashMap<PathBuf, RepoWatcher>,
    /// Maps arbitrary directories to their repo root and VCS kind. Negatives are not cached.
    dir_to_repo: HashMap<PathBuf, (PathBuf, VcsKind)>,
    started_at: Instant,
    config: Config,
    cache_dir: PathBuf,
    stats: DaemonStats,
    /// Repos currently being refreshed by watcher-triggered refresh_repo.
    refreshing: HashSet<PathBuf>,
    /// Per-repo watch channels notified when cache is updated.
    cache_watch: HashMap<PathBuf, watch::Sender<u64>>,
    /// Sticky config/template error appended to every formatted status.
    /// Cleared on successful config reload.
    config_error: Option<String>,
    /// Path to the config file, for explicit reload requests.
    config_file: Option<PathBuf>,
    /// Handle for dynamically changing the log filter at runtime.
    log_filter_handle: LogFilterHandle,
    /// Worker senders for querying overlay stats.
    jj_worker: Option<mpsc::UnboundedSender<JjWorkerRequest>>,
    git_worker: Option<mpsc::UnboundedSender<GitWorkerRequest>>,
}

impl DaemonState {
    /// Format status, appending any sticky config error.
    fn format(&self, status: &RepoStatus) -> String {
        let mut formatted =
            format_status(status, &self.config.resolved_format(), self.config.color);
        if let Some(ref err) = self.config_error {
            if !formatted.is_empty() {
                formatted.push(' ');
            }
            formatted.push_str(err);
        }
        formatted
    }

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

/// Monotonically increasing version for the runtime directory layout.
/// Bump this when the layout of files in the runtime directory changes in a
/// way that requires a clean slate (e.g. new cache format, renamed files).
/// On binary upgrade, if the new binary's directory version is greater than
/// the running one's, the runtime directory is cleaned (except logs) before
/// the new daemon starts.
pub const DIRECTORY_VERSION: u32 = 1;

/// Number of recent query durations to keep for percentile stats.
const TIMING_RING_SIZE: usize = 100;

pub fn init_logging(runtime_dir: &Path) -> LogFilterHandle {
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
        EnvFilter::try_from_env("VCS_STATUS_DAEMON_LOG").unwrap_or_else(|_| EnvFilter::new("info"));

    let (filter, reload_handle) = reload::Layer::new(filter);

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_appender)
        .with_ansi(false);

    let registry = tracing_subscriber::registry().with(filter).with(fmt_layer);

    #[cfg(feature = "tokio-console")]
    {
        let console_layer = console_subscriber::spawn();
        registry.with(console_layer).init();
    }

    #[cfg(not(feature = "tokio-console"))]
    {
        registry.init();
    }

    reload_handle
}

pub async fn run_daemon(
    config: Config,
    runtime_dir: PathBuf,
    // Path to the config file to watch for hot-reload. None disables watching
    // (used in tests where config is constructed in-memory).
    config_file: Option<PathBuf>,
    // Initial config/template error to display (e.g. from startup parse failure).
    initial_config_error: Option<String>,
    log_filter_handle: LogFilterHandle,
) -> Result<()> {
    let socket_path = runtime_dir.join("sock");
    let cache_dir = runtime_dir.join("cache");
    let (version, git_hash, _) = crate::protocol::version_info();
    tracing::info!(
        version = %version,
        git_hash = %git_hash,
        pid = std::process::id(),
        runtime_dir = %runtime_dir.display(),
        template_name = %config.template_name,
        has_format_override = config.format.is_some(),
        "starting daemon"
    );

    // Use the initial config error if provided, otherwise validate the template
    let startup_config_error = if initial_config_error.is_some() {
        initial_config_error
    } else {
        let resolved = config.resolved_format();
        if let Err(e) = crate::template::validate_template(&resolved) {
            let source = if config.format.is_some() {
                "format".to_string()
            } else {
                format!("template \"{}\"", config.template_name)
            };
            eprintln!("warning: invalid {source}: {e}");
            tracing::error!(source = %source, "invalid template: {e}");
            Some(e)
        } else {
            None
        }
    };

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
    std::fs::write(runtime_dir.join("version"), format!("{version} {git_hash}")).ok();

    let (watch_tx, watch_rx) = mpsc::unbounded_channel();
    let shutdown = Arc::new(Notify::new());

    let jj_worker = spawn_jj_worker();
    let git_worker = spawn_git_worker();

    let state = Arc::new(Mutex::new(DaemonState {
        cache: HashMap::new(),
        watchers: HashMap::new(),
        dir_to_repo: HashMap::new(),
        started_at: Instant::now(),
        config: config.clone(),
        cache_dir: cache_dir.clone(),
        stats: DaemonStats::default(),
        refreshing: HashSet::new(),
        cache_watch: HashMap::new(),
        config_error: startup_config_error,
        config_file: config_file.clone(),
        log_filter_handle,
        jj_worker: Some(jj_worker.clone()),
        git_worker: Some(git_worker.clone()),
    }));

    // Recover watchers from cache files left by a previous daemon instance.
    // The shell hook reads cache files directly and only contacts the daemon on
    // cache miss, so after an exec restart we must proactively re-watch all repos
    // that have cached entries — otherwise working copy changes go undetected.
    recover_watchers_from_cache(&cache_dir, &state, &watch_tx).await;

    // Spawn refresh task
    tokio::spawn(refresh_task(state.clone(), watch_rx, jj_worker, git_worker));

    // Spawn log rotation task
    let log_dir = runtime_dir.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
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

    // Watch for socket deletion: shut down if the socket file is removed
    let shutdown_socket = shutdown.clone();
    let socket_path_watch = socket_path.clone();
    tokio::spawn(async move {
        use notify::{Event, EventKind, RecursiveMode, Watcher, event::RemoveKind};

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let _watcher = notify::RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = tx.send(event);
                }
            },
            notify::Config::default(),
        );
        let mut watcher = match _watcher {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "failed to create socket watcher");
                return;
            }
        };
        if let Err(e) = watcher.watch(
            socket_path_watch.parent().unwrap(),
            RecursiveMode::NonRecursive,
        ) {
            tracing::warn!(error = %e, "failed to watch runtime directory for socket deletion");
            return;
        }
        while let Some(event) = rx.recv().await {
            if matches!(
                event.kind,
                EventKind::Remove(RemoveKind::File | RemoveKind::Any)
            ) && event.paths.iter().any(|p| p == &socket_path_watch)
            {
                tracing::info!("socket file was deleted, shutting down");
                shutdown_socket.notify_one();
                return;
            }
            // Also detect rename-over (some editors/tools delete via rename)
            if matches!(event.kind, EventKind::Remove(_)) && !socket_path_watch.exists() {
                tracing::info!("socket file is gone, shutting down");
                shutdown_socket.notify_one();
                return;
            }
        }
    });

    // Shared flag: when true, shutdown triggers exec-restart instead of exit
    let restart_requested = Arc::new(AtomicBool::new(false));

    // SIGTERM: clean up everything and shut down
    let shutdown_term = shutdown.clone();
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        sigterm.recv().await;
        shutdown_term.notify_one();
    });

    // SIGHUP: restart the daemon (e.g. after package manager update)
    let shutdown_hup = shutdown.clone();
    let restart_hup = restart_requested.clone();
    tokio::spawn(async move {
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("failed to register SIGHUP handler");
        sighup.recv().await;
        tracing::info!("received SIGHUP, restarting daemon");
        restart_hup.store(true, Ordering::Relaxed);
        shutdown_hup.notify_one();
    });

    // Watch config file for changes and hot-reload on valid updates
    if let Some(cf) = config_file {
        let state_cfg = state.clone();
        tokio::spawn(async move {
            watch_config_file(cf, state_cfg).await;
        });
    }

    // Watch for binary replacement and auto-restart
    let shutdown_exe = shutdown.clone();
    let restart_exe = restart_requested.clone();
    tokio::spawn(async move {
        if let Err(e) = watch_binary(shutdown_exe, restart_exe).await {
            tracing::warn!(error = %e, "binary watcher failed");
        }
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
                if restart_requested.load(Ordering::Relaxed) {
                    // Binary was replaced — exec the new binary in-place.
                    // Skip normal shutdown cleanup: exec replaces this process,
                    // and the new daemon will handle stale socket/pid on startup.
                    if let Ok(exe) = std::env::current_exe() {
                        maybe_clean_runtime_dir(&exe, &runtime_dir);

                        let args: Vec<String> = std::env::args().collect();
                        tracing::info!(exe = %exe.display(), "re-exec with new binary");
                        let err = exec_binary(&exe, &args);
                        // exec only returns on failure
                        tracing::error!(error = %err, "failed to exec new binary");
                    }
                }

                // Normal shutdown (or exec failed fallback)
                tracing::info!("daemon shutting down");
                if let Err(e) = std::fs::remove_file(&socket_path) {
                    tracing::warn!(path = %socket_path.display(), error = %e, "failed to remove socket");
                }
                if let Err(e) = std::fs::remove_dir_all(&cache_dir) {
                    tracing::warn!(path = %cache_dir.display(), error = %e, "failed to remove cache directory");
                }
                let _ = std::fs::remove_file(&pid_path);
                let _ = std::fs::remove_file(runtime_dir.join("version"));
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
                    tracing::info!(repo = %repo_path.display(), vcs = ?vcs_kind, "watching repo");
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
                                record_query_timing(&mut st.stats, query_start.elapsed());
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
                    record_query_timing(&mut st.stats, query_start.elapsed());
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
                            let mut st = state_bg.lock().await;
                            let formatted = st.format(&status);
                            write_cache_file(&cd_bg, &repo_path_bg, &formatted);
                            if query_path_bg != repo_path_bg {
                                link_cache_file(&cd_bg, &repo_path_bg, &query_path_bg);
                            }
                            st.update_cache(&repo_path_bg, status, formatted);
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
                            record_query_timing(&mut st.stats, query_start.elapsed());
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
        Request::ReloadConfig => {
            let config_file = state.lock().await.config_file.clone();
            if let Some(cf) = config_file {
                reload_config(&cf, &state).await;
            }
            send_response(&mut writer, Response::Ok).await
        }
        Request::Shutdown => {
            send_response(&mut writer, Response::Ok).await?;
            shutdown.notify_one();
            Ok(())
        }
        Request::DaemonStatus { verbose } => {
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
            let jj_w = st.jj_worker.clone();
            let git_w = st.git_worker.clone();
            drop(st);

            // Query workers for current overlay stats
            let mut incremental_diff_stats = Vec::new();
            if let Some(ref jj) = jj_w {
                let (tx, rx) = tokio::sync::oneshot::channel();
                if jj
                    .send(JjWorkerRequest::QueryOverlayStats { reply: tx })
                    .is_ok()
                    && let Ok(jj_stats) = rx.await
                {
                    incremental_diff_stats.extend(jj_stats);
                }
            }
            if let Some(ref git) = git_w {
                let (tx, rx) = tokio::sync::oneshot::channel();
                if git
                    .send(GitWorkerRequest::QueryOverlayStats { reply: tx })
                    .is_ok()
                    && let Ok(git_stats) = rx.await
                {
                    incremental_diff_stats.extend(git_stats);
                }
            }

            // Query per-directory breakdown if verbose
            let mut dir_diff_stats = Vec::new();
            if verbose {
                if let Some(jj) = jj_w {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    if jj
                        .send(JjWorkerRequest::QueryOverlayStatsVerbose { reply: tx })
                        .is_ok()
                        && let Ok(jj_stats) = rx.await
                    {
                        dir_diff_stats.extend(jj_stats);
                    }
                }
                if let Some(git) = git_w {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    if git
                        .send(GitWorkerRequest::QueryOverlayStatsVerbose { reply: tx })
                        .is_ok()
                        && let Ok(git_stats) = rx.await
                    {
                        dir_diff_stats.extend(git_stats);
                    }
                }
            }

            send_response(
                &mut writer,
                Response::DaemonStatus {
                    pid: std::process::id(),
                    uptime_secs,
                    watched_repos,
                    stats,
                    incremental_diff_stats,
                    dir_diff_stats,
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
        Request::SetLogFilter { filter } => {
            let new_filter = match EnvFilter::try_new(&filter) {
                Ok(f) => f,
                Err(e) => {
                    return send_response(
                        &mut writer,
                        Response::Error {
                            message: format!("invalid filter \"{filter}\": {e}"),
                        },
                    )
                    .await;
                }
            };
            let st = state.lock().await;
            match st.log_filter_handle.reload(new_filter) {
                Ok(()) => {
                    tracing::info!(filter = %filter, "log filter updated");
                    drop(st);
                    send_response(&mut writer, Response::Ok).await
                }
                Err(e) => {
                    drop(st);
                    send_response(
                        &mut writer,
                        Response::Error {
                            message: format!("failed to reload filter: {e}"),
                        },
                    )
                    .await
                }
            }
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

/// Scan the cache directory for files left by a previous daemon instance and
/// set up watchers for the repos they represent.  Cache file names encode the
/// directory path (`/` → `%`), so we can decode them back.  Hardlinked
/// subdirectory entries share an inode with their repo root; we deduplicate by
/// inode so each repo is watched exactly once.
async fn recover_watchers_from_cache(
    cache_dir: &Path,
    state: &Arc<Mutex<DaemonState>>,
    watch_tx: &mpsc::UnboundedSender<WatchEvent>,
) {
    let entries = match std::fs::read_dir(cache_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    // Deduplicate by inode — hardlinked subdirectory cache entries share the
    // same inode as the repo root entry.
    use std::collections::HashSet;
    use std::os::unix::fs::MetadataExt;
    let mut seen_inodes = HashSet::new();
    let mut recovered = 0u32;

    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !seen_inodes.insert(meta.ino()) {
            continue; // hardlink to a repo root we already processed
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Decode: `%` → `/`
        let dir_path = PathBuf::from(name.replace('%', "/"));

        // Only set up a watcher if this path is actually a repo root
        let Some((repo_path, vcs_kind)) = find_repo_root(&dir_path) else {
            continue;
        };
        if repo_path != dir_path {
            continue; // this cache entry is for a subdirectory, not the root
        }

        let mut st = state.lock().await;
        if st.watchers.contains_key(&repo_path) {
            continue;
        }
        match watch_repo(&repo_path, vcs_kind, watch_tx.clone()) {
            Ok(watcher) => {
                tracing::info!(repo = %repo_path.display(), vcs = ?vcs_kind, "recovered watcher from cache");
                st.dir_to_repo
                    .insert(repo_path.clone(), (repo_path.clone(), vcs_kind));
                st.watchers.insert(repo_path, watcher);
                recovered += 1;
            }
            Err(e) => {
                tracing::warn!(repo = %repo_path.display(), error = %e, "failed to recover watcher");
            }
        }
    }

    if recovered > 0 {
        tracing::info!(count = recovered, "recovered watchers from previous cache");
    }
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

fn record_query_timing(stats: &mut DaemonStats, elapsed: Duration) {
    let ms = elapsed.as_secs_f64() * 1000.0;
    if stats.recent_query_ms.len() >= TIMING_RING_SIZE {
        stats.recent_query_ms.remove(0);
    }
    stats.recent_query_ms.push(ms);
}

fn record_refresh_timing(stats: &mut DaemonStats, elapsed: Duration, incremental: bool) {
    let ms = elapsed.as_secs_f64() * 1000.0;
    let buf = if incremental {
        stats.incremental_refreshes += 1;
        &mut stats.recent_incremental_refresh_ms
    } else {
        stats.full_refreshes += 1;
        &mut stats.recent_full_refresh_ms
    };
    if buf.len() >= TIMING_RING_SIZE {
        buf.remove(0);
    }
    buf.push(ms);
}

/// Per-repo refresh state: tracks whether a re-refresh is needed while one is in flight.
enum RepoRefreshState {
    /// A refresh task is running, no new events queued.
    InFlight,
    /// A refresh task is running AND new events arrived — re-refresh needed after completion.
    /// Fields: vcs_kind, working_copy_changed, accumulated changed_paths, VCS change hint.
    Pending(VcsKind, bool, Vec<PathBuf>, Option<VcsChangeHint>),
}

impl RepoRefreshState {
    /// Coalesce a new event into the current state.
    ///
    /// Hints coalesce by taking the maximum severity. Working-copy paths accumulate.
    fn coalesce(
        &mut self,
        vcs_kind: VcsKind,
        working_copy_changed: bool,
        changed_paths: Vec<PathBuf>,
        hint: Option<VcsChangeHint>,
    ) {
        match self {
            RepoRefreshState::InFlight => {
                *self =
                    RepoRefreshState::Pending(vcs_kind, working_copy_changed, changed_paths, hint);
            }
            RepoRefreshState::Pending(_, wc, paths, existing_hint) => {
                *wc = *wc || working_copy_changed;
                paths.extend(changed_paths);
                *existing_hint = match (*existing_hint, hint) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (Some(a), None) => Some(a),
                    (None, b) => b,
                };
            }
        }
    }
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
    vcs_change_hint: Option<VcsChangeHint>,
    state: Arc<Mutex<DaemonState>>,
    jj_worker: mpsc::UnboundedSender<JjWorkerRequest>,
    git_worker: mpsc::UnboundedSender<GitWorkerRequest>,
    done_tx: mpsc::UnboundedSender<RefreshDone>,
) {
    let refresh_start = Instant::now();
    tracing::debug!(
        repo = %repo_path.display(),
        vcs = ?vcs_kind,
        working_copy_changed,
        changed_paths = changed_paths.len(),
        hint = ?vcs_change_hint,
        "refresh starting"
    );
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
            let formatted = st.format(&stale_status);
            write_cache_file(&cd, &repo_path, &formatted);
            st.update_cache(&repo_path, stale_status, formatted);
        }

        (config, cd)
    };

    let (result, was_incremental) = match (vcs_kind, vcs_change_hint) {
        // VCS-internal event detected — validate baseline before deciding refresh type
        (VcsKind::Jj, Some(hint)) => (
            jj_validate_refresh(&repo_path, &config, changed_paths, hint, &jj_worker).await,
            false,
        ),
        (VcsKind::Git, Some(hint)) => (
            git_validate_refresh(&repo_path, changed_paths, hint, &git_worker).await,
            false,
        ),
        // Pure working-copy change — try incremental update
        (VcsKind::Jj, None) if !changed_paths.is_empty() => {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let _ = jj_worker.send(JjWorkerRequest::IncrementalUpdate {
                repo_path: repo_path.clone(),
                changed_paths,
                reply: reply_tx,
            });
            match reply_rx.await {
                Ok(Ok(status)) => (Ok(status), true),
                Ok(Err(_)) => {
                    // No incremental state — fall back to full refresh
                    (
                        jj_full_refresh(&repo_path, &config, &jj_worker).await,
                        false,
                    )
                }
                Err(_) => (Err(anyhow::anyhow!("jj worker channel closed")), false),
            }
        }
        (VcsKind::Git, None) if !changed_paths.is_empty() => {
            let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
            let _ = git_worker.send(GitWorkerRequest::IncrementalUpdate {
                repo_path: repo_path.clone(),
                changed_paths,
                reply: reply_tx,
            });
            match reply_rx.await {
                Ok(Ok(status)) => (Ok(status), true),
                Ok(Err(_)) => {
                    // No incremental state — fall back to full refresh
                    (git_full_refresh(&repo_path, &git_worker).await, false)
                }
                Err(_) => (Err(anyhow::anyhow!("git worker channel closed")), false),
            }
        }
        // Fallback — full refresh
        (VcsKind::Jj, None) => (
            jj_full_refresh(&repo_path, &config, &jj_worker).await,
            false,
        ),
        (VcsKind::Git, None) => (git_full_refresh(&repo_path, &git_worker).await, false),
    };

    match result {
        Ok(ref status) => {
            tracing::debug!(
                repo = %repo_path.display(),
                elapsed_ms = refresh_start.elapsed().as_millis() as u64,
                incremental = was_incremental,
                change_id = %status.change_id,
                file_mad_count_working_tree = status.file_mad_count_working_tree,
                "refresh complete"
            );
            let mut st = state.lock().await;
            let formatted = st.format(status);
            write_cache_file(&cd, &repo_path, &formatted);
            st.refreshing.remove(&repo_path);
            st.update_cache(&repo_path, status.clone(), formatted);
            record_refresh_timing(&mut st.stats, refresh_start.elapsed(), was_incremental);
        }
        Err(e) => {
            tracing::error!(repo = %repo_path.display(), error = %e, "refresh failed");
            let mut st = state.lock().await;
            st.refreshing.remove(&repo_path);
            if let Some((prev_status, _)) = st.cache.get(&repo_path) {
                let mut stale_status = prev_status.clone();
                stale_status.is_stale = true;
                stale_status.refresh_error = e.to_string();
                let formatted = st.format(&stale_status);
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

/// Validate-and-refresh for jj: check if parent tree changed, then either
/// do a metadata-only update (with optional incremental WC diffs) or full refresh.
async fn jj_validate_refresh(
    repo_path: &Path,
    config: &Config,
    changed_paths: Vec<PathBuf>,
    _hint: VcsChangeHint,
    jj_worker: &mpsc::UnboundedSender<JjWorkerRequest>,
) -> Result<crate::template::RepoStatus> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let _ = jj_worker.send(JjWorkerRequest::ValidateAndRefresh {
        repo_path: repo_path.to_path_buf(),
        changed_paths,
        depth: config.bookmark_search_depth,
        reply: reply_tx,
    });
    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("jj worker channel closed"))?
}

/// Validate-and-refresh for git: check if HEAD tree OID changed, then dispatch
/// to metadata-only, index refresh, or full refresh based on hint.
async fn git_validate_refresh(
    repo_path: &Path,
    changed_paths: Vec<PathBuf>,
    hint: VcsChangeHint,
    git_worker: &mpsc::UnboundedSender<GitWorkerRequest>,
) -> Result<crate::template::RepoStatus> {
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let _ = git_worker.send(GitWorkerRequest::ValidateAndRefresh {
        repo_path: repo_path.to_path_buf(),
        changed_paths,
        hint,
        reply: reply_tx,
    });
    reply_rx
        .await
        .map_err(|_| anyhow::anyhow!("git worker channel closed"))?
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

/// Reload config from disk, validate the template, swap the config in DaemonState,
/// and re-render all cached statuses.
async fn reload_config(config_path: &Path, state: &Arc<Mutex<DaemonState>>) {
    let (new_config, config_err) = match crate::config::load_config_from(Some(config_path)) {
        Ok(c) => {
            let resolved = c.resolved_format();
            let err = crate::template::validate_template(&resolved).err();
            if let Some(ref e) = err {
                tracing::warn!(error = %e, "new config has invalid template");
            }
            (Some(c), err)
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to load updated config, keeping current config");
            (None, Some(format!("config error: {e}")))
        }
    };

    let mut st = state.lock().await;
    if let Some(new_config) = new_config {
        tracing::info!(
            template_name = %new_config.template_name,
            has_format_override = new_config.format.is_some(),
            has_config_error = config_err.is_some(),
            "config reloaded"
        );
        st.config = new_config;
    }
    st.config_error = config_err;

    let cache_dir = st.cache_dir.clone();

    let repos: Vec<PathBuf> = st.cache.keys().cloned().collect();
    for repo_path in repos {
        if let Some((status, _)) = st.cache.get(&repo_path).cloned() {
            let formatted = st.format(&status);
            write_cache_file(&cache_dir, &repo_path, &formatted);
            st.update_cache(&repo_path, status, formatted);
        }
    }
}

/// Query the new binary's directory-version and clean the runtime directory
/// (preserving log files) if it is greater than the current one.
fn maybe_clean_runtime_dir(new_exe: &Path, runtime_dir: &Path) {
    let output = match std::process::Command::new(new_exe)
        .arg("directory-version")
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return,
    };
    let new_version: u32 = match String::from_utf8_lossy(&output.stdout).trim().parse() {
        Ok(v) => v,
        Err(_) => return,
    };

    if new_version <= DIRECTORY_VERSION {
        return;
    }

    tracing::info!(
        old = DIRECTORY_VERSION,
        new = new_version,
        "directory version increased, cleaning runtime directory"
    );

    // Remove everything except log files
    if let Ok(entries) = std::fs::read_dir(runtime_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("daemon.log") {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Replace the current process with a new invocation of the given binary.
fn exec_binary(exe: &Path, args: &[String]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    // args[0] is the program name, args[1..] are the actual arguments.
    let mut cmd = std::process::Command::new(exe);
    if args.len() > 1 {
        cmd.args(&args[1..]);
    }
    // exec replaces the process; only returns on error.
    cmd.exec()
}

/// Watch the daemon's own binary for replacement and trigger a restart.
///
/// Records the initial inode of the binary. When the file at that path changes
/// to a different inode (i.e. it was replaced, not just touched), signals shutdown
/// with the restart flag set.
async fn watch_binary(shutdown: Arc<Notify>, restart: Arc<AtomicBool>) -> Result<()> {
    use anyhow::Context;
    use notify::{Event, EventKind, RecursiveMode, Watcher};
    use std::os::unix::fs::MetadataExt;

    let exe_path = std::env::current_exe().context("failed to get current exe")?;
    let exe_path = exe_path.canonicalize().unwrap_or_else(|_| exe_path.clone());

    let original_meta = std::fs::metadata(&exe_path).context("failed to stat binary")?;
    let original_ino = original_meta.ino();
    let original_mtime = original_meta.mtime();
    let original_size = original_meta.size();

    let watch_dir = exe_path
        .parent()
        .context("binary has no parent directory")?
        .to_path_buf();

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut watcher = notify::RecommendedWatcher::new(
        move |res: std::result::Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        },
        notify::Config::default(),
    )?;
    watcher.watch(&watch_dir, RecursiveMode::NonRecursive)?;
    tracing::info!(path = %exe_path.display(), ino = original_ino, size = original_size, "watching binary for replacement");

    // Periodic check interval: catches cases where the watcher misses events
    // (e.g. parent directory deleted, watcher itself removed).
    let mut existence_check = tokio::time::interval(Duration::from_secs(30));
    existence_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the first (immediate) tick
    existence_check.tick().await;

    loop {
        tokio::select! {
            event = rx.recv() => {
                let Some(event) = event else { break };

                let affects_exe = match event.kind {
                    EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) => {
                        event.paths.iter().any(|p| p == &exe_path)
                    }
                    _ => false,
                };
                if !affects_exe {
                    continue;
                }

                // Small delay to let the write/rename finish
                tokio::time::sleep(Duration::from_millis(500)).await;

                // Binary was deleted (e.g. package manager installed new version
                // at a different path). Shut down cleanly — the next client query
                // will auto-start the new daemon from the updated PATH.
                if !exe_path.exists() {
                    tracing::info!(
                        path = %exe_path.display(),
                        "binary deleted, shutting down so next client starts the new version"
                    );
                    // Don't set restart flag — there's no binary to re-exec.
                    shutdown.notify_one();
                    return Ok(());
                }

                // Check if the file at our path changed (different inode, mtime, or size)
                let Ok(meta) = std::fs::metadata(&exe_path) else {
                    continue;
                };
                let new_ino = meta.ino();
                let new_mtime = meta.mtime();
                let new_size = meta.size();
                if new_ino == original_ino && new_mtime == original_mtime && new_size == original_size {
                    continue;
                }

                tracing::info!(
                    path = %exe_path.display(),
                    old_ino = original_ino,
                    new_ino,
                    old_size = original_size,
                    new_size,
                    "binary replaced, restarting daemon"
                );
                restart.store(true, Ordering::Relaxed);
                shutdown.notify_one();
                return Ok(());
            }
            _ = existence_check.tick() => {
                // Periodic fallback: check if the binary still exists on disk.
                // Covers cases where the watcher fails (e.g. parent dir deleted).
                if !exe_path.exists() {
                    tracing::info!(
                        path = %exe_path.display(),
                        "binary no longer exists (periodic check), shutting down"
                    );
                    shutdown.notify_one();
                    return Ok(());
                }
            }
        }
    }

    Ok(())
}

/// Watch the config file for changes and hot-reload on valid updates.
///
/// On each detected change, re-reads the file, parses it, validates the template,
/// and (on success) swaps the config in DaemonState and re-renders all cached entries.
async fn watch_config_file(config_path: PathBuf, state: Arc<Mutex<DaemonState>>) {
    use notify::{Event, EventKind, RecursiveMode, Watcher};

    // Canonicalize so path comparisons work on macOS (/var → /private/var).
    let config_path = config_path.canonicalize().unwrap_or(config_path);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let watcher = notify::RecommendedWatcher::new(
        move |res: std::result::Result<Event, notify::Error>| {
            if let Ok(event) = res {
                let _ = tx.send(event);
            }
        },
        notify::Config::default(),
    );
    let mut watcher = match watcher {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "failed to create config file watcher");
            return;
        }
    };

    // Watch the parent directory so we also detect create-after-delete (editor save pattern).
    let watch_dir = match config_path.parent() {
        Some(dir) if dir.exists() => dir.to_path_buf(),
        _ => {
            tracing::warn!(path = %config_path.display(), "config file parent directory does not exist, skipping config watch");
            return;
        }
    };
    if let Err(e) = watcher.watch(&watch_dir, RecursiveMode::NonRecursive) {
        tracing::warn!(error = %e, "failed to watch config directory");
        return;
    }
    tracing::info!(path = %config_path.display(), "watching config file for changes");

    while let Some(event) = rx.recv().await {
        // Only react to writes/creates that affect our config file
        let dominated_by_config = match event.kind {
            EventKind::Modify(_) | EventKind::Create(_) => {
                event.paths.iter().any(|p| p == &config_path)
            }
            _ => false,
        };
        if !dominated_by_config {
            continue;
        }

        tracing::info!(path = %config_path.display(), "config file changed, attempting reload");

        // Small delay to coalesce rapid writes (editor save patterns)
        tokio::time::sleep(Duration::from_millis(50)).await;

        reload_config(&config_path, &state).await;
    }
}

#[tracing::instrument(skip_all)]
async fn refresh_task(
    state: Arc<Mutex<DaemonState>>,
    mut watch_rx: mpsc::UnboundedReceiver<WatchEvent>,
    jj_worker: mpsc::UnboundedSender<JjWorkerRequest>,
    git_worker: mpsc::UnboundedSender<GitWorkerRequest>,
) {
    // Per-repo concurrency control: at most one refresh per repo at a time.
    let mut in_flight: HashMap<PathBuf, RepoRefreshState> = HashMap::new();
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<RefreshDone>();

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

                        // Force a fresh refresh for all known repos.
                        // This ensures correctness even if the file watcher
                        // hasn't delivered events yet (e.g. on slow CI runners).
                        {
                            let st = state.lock().await;
                            let mut seen = HashSet::new();
                            let repos: Vec<(PathBuf, VcsKind)> = st
                                .dir_to_repo
                                .values()
                                .filter(|(rp, _)| seen.insert(rp.clone()))
                                .cloned()
                                .collect();
                            drop(st);
                            for (repo_path, vcs_kind) in repos {
                                if !in_flight.contains_key(&repo_path) {
                                    in_flight.insert(
                                        repo_path.clone(),
                                        RepoRefreshState::InFlight,
                                    );
                                    tokio::spawn(refresh_repo(
                                        repo_path,
                                        vcs_kind,
                                        false,
                                        vec![],
                                        None,
                                        state.clone(),
                                        jj_worker.clone(),
                                        git_worker.clone(),
                                        done_tx.clone(),
                                    ));
                                }
                            }
                        }

                        // Wait for the forced refreshes to complete
                        while !in_flight.is_empty() {
                            if let Some(done) = done_rx.recv().await {
                                handle_refresh_done(
                                    &done, &mut in_flight, &jj_worker, &git_worker, &state, &done_tx,
                                );
                            }
                        }

                        let _ = tx.send(());
                    }
                    WatchEvent::Change { repo_path, vcs_kind, working_copy_changed, vcs_change_hint, changed_paths } => {
                        state.lock().await.stats.fs_events += 1;

                        match in_flight.get_mut(&repo_path) {
                            None => {
                                // No refresh running — start one immediately
                                in_flight.insert(repo_path.clone(), RepoRefreshState::InFlight);
                                tokio::spawn(refresh_repo(
                                    repo_path, vcs_kind, working_copy_changed, changed_paths,
                                    vcs_change_hint,
                                    state.clone(), jj_worker.clone(), git_worker.clone(), done_tx.clone(),
                                ));
                            }
                            Some(entry) => {
                                entry.coalesce(vcs_kind, working_copy_changed, changed_paths, vcs_change_hint);
                                tracing::debug!(
                                    repo = %repo_path.display(),
                                    ?vcs_change_hint,
                                    working_copy_changed,
                                    "event coalesced while refresh in-flight"
                                );
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
        Some(RepoRefreshState::Pending(vcs_kind, wc, paths, hint)) => {
            // Changes arrived while refreshing — re-refresh immediately.
            in_flight.insert(done.repo_path.clone(), RepoRefreshState::InFlight);
            tokio::spawn(refresh_repo(
                done.repo_path.clone(),
                vcs_kind,
                wc,
                paths,
                hint,
                state.clone(),
                jj_worker.clone(),
                git_worker.clone(),
                done_tx.clone(),
            ));
        }
        _ => {
            // Done, no pending work for this repo
        }
    }
}

/// Create a log filter handle for tests (not attached to a global subscriber).
#[cfg(test)]
fn test_log_filter_handle() -> LogFilterHandle {
    use tracing_subscriber::layer::SubscriberExt;

    let filter = EnvFilter::new("off");
    let (filter_layer, handle) = reload::Layer::new(filter);
    let _subscriber = tracing_subscriber::registry().with(filter_layer);
    handle
}

/// Convenience wrapper for tests: calls `run_daemon` with no config file, no
/// startup error, and a dummy log-filter handle.
#[cfg(test)]
pub async fn run_daemon_for_test(config: Config, runtime_dir: PathBuf) -> Result<()> {
    run_daemon(config, runtime_dir, None, None, test_log_filter_handle()).await
}

/// Like `run_daemon_for_test` but with a config file for hot-reload testing.
#[cfg(test)]
pub async fn run_daemon_for_test_with_config(
    config: Config,
    runtime_dir: PathBuf,
    config_file: PathBuf,
) -> Result<()> {
    run_daemon(
        config,
        runtime_dir,
        Some(config_file),
        None,
        test_log_filter_handle(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;
    use tokio::process::Command;
    use tokio::time::Duration;

    // --- RepoRefreshState::coalesce unit tests ---

    #[test]
    fn test_coalesce_inflight_to_wc_event() {
        let mut state = RepoRefreshState::InFlight;
        let paths = vec![PathBuf::from("/repo/file.txt")];
        state.coalesce(VcsKind::Jj, true, paths.clone(), None);
        match &state {
            RepoRefreshState::Pending(_, wc, p, hint) => {
                assert!(*wc);
                assert_eq!(p, &paths);
                assert_eq!(*hint, None);
            }
            _ => panic!("expected Pending"),
        }
    }

    #[test]
    fn test_coalesce_inflight_to_vcs_internal_event() {
        let mut state = RepoRefreshState::InFlight;
        state.coalesce(
            VcsKind::Jj,
            false,
            vec![],
            Some(VcsChangeHint::HeadMayHaveChanged),
        );
        match &state {
            RepoRefreshState::Pending(_, wc, p, hint) => {
                assert!(!*wc);
                assert!(p.is_empty());
                assert_eq!(*hint, Some(VcsChangeHint::HeadMayHaveChanged));
            }
            _ => panic!("expected Pending"),
        }
    }

    #[test]
    fn test_coalesce_wc_then_wc_accumulates() {
        // Two working-copy events: paths should accumulate
        let mut state =
            RepoRefreshState::Pending(VcsKind::Jj, true, vec![PathBuf::from("/repo/a.txt")], None);
        state.coalesce(VcsKind::Jj, true, vec![PathBuf::from("/repo/b.txt")], None);
        match &state {
            RepoRefreshState::Pending(_, wc, paths, hint) => {
                assert!(*wc);
                assert_eq!(paths.len(), 2);
                assert_eq!(*hint, None);
            }
            _ => panic!("expected Pending"),
        }
    }

    #[test]
    fn test_coalesce_wc_then_vcs_hint_merges() {
        // Working-copy event pending, then VCS-internal arrives — hint is added,
        // paths are preserved (worker will decide based on baseline validation)
        let mut state =
            RepoRefreshState::Pending(VcsKind::Jj, true, vec![PathBuf::from("/repo/a.txt")], None);
        state.coalesce(
            VcsKind::Jj,
            false,
            vec![],
            Some(VcsChangeHint::HeadMayHaveChanged),
        );
        match &state {
            RepoRefreshState::Pending(_, wc, paths, hint) => {
                assert!(*wc, "wc should stay true (OR of both events)");
                assert_eq!(
                    paths.len(),
                    1,
                    "paths preserved for potential incremental use"
                );
                assert_eq!(*hint, Some(VcsChangeHint::HeadMayHaveChanged));
            }
            _ => panic!("expected Pending"),
        }
    }

    #[test]
    fn test_coalesce_vcs_then_wc_merges() {
        // VCS-internal event pending, then working-copy event arrives — both preserved
        let mut state = RepoRefreshState::Pending(
            VcsKind::Jj,
            false,
            vec![],
            Some(VcsChangeHint::HeadMayHaveChanged),
        );
        state.coalesce(VcsKind::Jj, true, vec![PathBuf::from("/repo/a.txt")], None);
        match &state {
            RepoRefreshState::Pending(_, wc, paths, hint) => {
                assert!(*wc, "wc should be true (OR of both events)");
                assert_eq!(paths.len(), 1, "WC paths accumulated");
                assert_eq!(*hint, Some(VcsChangeHint::HeadMayHaveChanged));
            }
            _ => panic!("expected Pending"),
        }
    }

    #[test]
    fn test_coalesce_hint_takes_max_severity() {
        // MetadataOnly pending, then HeadMayHaveChanged arrives — takes max
        let mut state = RepoRefreshState::Pending(
            VcsKind::Git,
            false,
            vec![],
            Some(VcsChangeHint::MetadataOnly),
        );
        state.coalesce(
            VcsKind::Git,
            false,
            vec![],
            Some(VcsChangeHint::HeadMayHaveChanged),
        );
        match &state {
            RepoRefreshState::Pending(_, _, _, hint) => {
                assert_eq!(*hint, Some(VcsChangeHint::HeadMayHaveChanged));
            }
            _ => panic!("expected Pending"),
        }
    }

    // --- Existing tests ---

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

    use crate::test_util::{
        create_git_repo_async as create_git_repo, create_jj_repo_async as create_jj_repo,
        wait_for_socket,
    };

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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));

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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));

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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));

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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));

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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));
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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));
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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));

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

        wait_for_socket(&socket_path).await;
        child
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
        let kill_status = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap();
        assert!(
            kill_status.success(),
            "kill -TERM {pid} failed with {kill_status}"
        );

        let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .expect("daemon should exit within 10s")
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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));

        let dir = create_jj_repo().await;
        let formatted = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(!formatted.is_empty());

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_daemon_shuts_down_on_socket_deletion() {
        let dir = create_jj_repo().await;
        let rt = temp_runtime_dir("sockdel");
        let socket_path = rt.path().join("sock");
        let config = Config {
            color: false,
            ..Default::default()
        };

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));

        // Make sure daemon is running and serving
        let formatted = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(!formatted.is_empty());

        // Delete the socket file
        std::fs::remove_file(&socket_path).unwrap();

        // The daemon should shut down on its own
        for _ in 0..200 {
            if daemon.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            daemon.is_finished(),
            "daemon should shut down after socket deletion"
        );
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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));

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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));

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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));

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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));
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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));

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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));

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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));

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

        let daemon = tokio::spawn(run_daemon_for_test(config, rt.path().to_path_buf()));
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

    #[tokio::test]
    async fn test_config_reload_updates_template() {
        let dir = create_jj_repo().await;
        let rt = temp_runtime_dir("cfg-reload");
        let socket_path = rt.path().join("sock");

        // Write an initial config with a custom format
        let config_file = rt.path().join("config.toml");
        std::fs::write(
            &config_file,
            "color = false\nformat = \"before:{{ commit_id }}\"\n",
        )
        .unwrap();

        let config = crate::config::load_config_from(Some(&config_file)).unwrap();
        let daemon = tokio::spawn(run_daemon_for_test_with_config(
            config,
            rt.path().to_path_buf(),
            config_file.clone(),
        ));

        // Wait for initial status with "before:" prefix
        let formatted = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(
            formatted.starts_with("before:"),
            "expected 'before:' prefix, got: {formatted}"
        );

        // Rewrite config with a different format
        std::fs::write(
            &config_file,
            "color = false\nformat = \"after:{{ commit_id }}\"\n",
        )
        .unwrap();

        // Poll until the daemon picks up the new template
        let formatted = query_until_match(&socket_path, &dir.path().to_string_lossy(), |s| {
            s.starts_with("after:")
        })
        .await;
        assert!(
            formatted.starts_with("after:"),
            "expected 'after:' prefix after reload, got: {formatted}"
        );

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn test_config_reload_rejects_invalid_template() {
        let dir = create_jj_repo().await;
        let rt = temp_runtime_dir("cfg-bad");
        let socket_path = rt.path().join("sock");

        // Write an initial config with a custom format
        let config_file = rt.path().join("config.toml");
        std::fs::write(
            &config_file,
            "color = false\nformat = \"good:{{ commit_id }}\"\n",
        )
        .unwrap();

        let config = crate::config::load_config_from(Some(&config_file)).unwrap();
        let daemon = tokio::spawn(run_daemon_for_test_with_config(
            config,
            rt.path().to_path_buf(),
            config_file.clone(),
        ));

        // Wait for initial status
        let formatted = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(
            formatted.starts_with("good:"),
            "expected 'good:' prefix, got: {formatted}"
        );

        // Write an invalid template — should be accepted but with error appended
        std::fs::write(&config_file, "color = false\nformat = \"{{ broken }\"\n").unwrap();

        // Give the watcher time to process
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Status should include the template error
        let _ = send_request(&socket_path, &Request::Flush).await;
        let formatted = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(
            formatted.contains("template error:"),
            "invalid template should show error in output, got: {formatted}"
        );

        // Fix the template — error should clear
        std::fs::write(
            &config_file,
            "color = false\nformat = \"fixed:{{ commit_id }}\"\n",
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = send_request(&socket_path, &Request::Flush).await;
        let formatted = query_until_ready(&socket_path, &dir.path().to_string_lossy()).await;
        assert!(
            formatted.starts_with("fixed:"),
            "fixed template should render normally, got: {formatted}"
        );
        assert!(
            !formatted.contains("template error:"),
            "fixed template should not have error, got: {formatted}"
        );

        let _ = send_request(&socket_path, &Request::Shutdown).await;
        daemon.await.unwrap().unwrap();
    }
}
