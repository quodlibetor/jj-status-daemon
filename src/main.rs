mod client;
mod config;
mod daemon;
mod git;
mod init;
mod jj;
mod protocol;
mod template;
#[cfg(test)]
mod test_util;
mod watcher;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};

fn format_version_info(version: &str, git_hash: &str, features: &[String]) -> String {
    let features_str = if features.is_empty() {
        "none".to_string()
    } else {
        features.join(", ")
    };
    format!("{version} ({git_hash})\nfeatures: {features_str}")
}

fn print_version() {
    let (version, git_hash, features) = protocol::version_info();
    println!(
        "vcs-status-daemon {}",
        format_version_info(&version, &git_hash, &features)
    );
    match client::daemon_version() {
        Ok((dv, dh, df)) => {
            println!("daemon          {}", format_version_info(&dv, &dh, &df));
        }
        Err(_) => {
            println!("daemon          not running");
        }
    }
}

fn long_version() -> &'static str {
    use std::sync::LazyLock;
    static VERSION: LazyLock<String> = LazyLock::new(|| {
        let (version, git_hash, features) = protocol::version_info();
        format_version_info(&version, &git_hash, &features)
    });
    &VERSION
}

#[derive(Parser)]
#[command(name = "vcs-status-daemon")]
#[command(about = "Fast jj status for shell prompts")]
#[command(version, long_version = long_version())]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to the jj repository (default: auto-detect)
    #[arg(long)]
    repo: Option<PathBuf>,

    /// Path to config file (overrides default config path)
    #[arg(long)]
    config_file: Option<PathBuf>,

    /// Allow running as root (not recommended)
    #[arg(long)]
    allow_root: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the background daemon
    Daemon {
        /// Runtime directory (contains socket, cache, and log files)
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Path to config file
        #[arg(long)]
        config_file: Option<PathBuf>,
    },
    /// Query the daemon for status (default)
    Query {
        /// Path to the jj repository
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Shut down the daemon
    Shutdown,
    /// Restart the daemon (graceful shutdown, then start)
    Restart,
    /// Show daemon status (running, PID, uptime, watched repos)
    Status,
    /// Print shell integration code (use with eval)
    Init {
        /// Shell to generate code for
        shell: init::Shell,
        /// Check starship.toml for correct VCS_STATUS configuration
        #[arg(long)]
        starship: bool,
    },
    /// Manage configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Preview and test templates
    Template {
        #[command(subcommand)]
        action: TemplateAction,
    },
    /// Update to the latest release
    SelfUpdate {
        /// Just check if an update is available, don't install it
        #[arg(long)]
        check: bool,
    },

    /// Change the daemon's log filter at runtime (e.g. "debug", "vcs_status_daemon=trace")
    SetLogFilter {
        /// Filter directive (e.g. "debug", "info", "vcs_status_daemon=trace")
        filter: String,
    },

    /// Print the directory layout version (used internally for upgrade cleanup)
    #[command(hide = true)]
    DirectoryVersion,
}

#[derive(Subcommand)]
enum TemplateAction {
    /// List available templates with representative outputs
    List {
        /// Print only template names, one per line
        #[arg(short = 'n', long)]
        name_only: bool,
    },
    /// Render a template with representative examples and the current repo
    Format {
        /// Template format string (Tera/Jinja2 syntax)
        template: String,
        /// Path to a repository to show live status
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Print the raw Tera template source (includes are inlined).
    /// Use `template list -n` to see available template names.
    Print {
        /// Template name (e.g. "ascii", "nerdfont", or a user-defined name).
        /// Run `template list -n` to see available names.
        name: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum ConfigAction {
    /// Write a default config file with explanatory comments
    Init,
    /// Open the config file in $EDITOR
    Edit,
    /// Print the config file path
    Path,
    /// Set a config value (e.g. `config set template_name nerdfont`)
    Set {
        /// Config key (e.g. "template_name", "bookmark_search_depth")
        key: String,
        /// Value to set (strings, numbers, and booleans are auto-detected)
        value: String,
    },
    /// Get a config value
    Get {
        /// Config key (e.g. "template_name", "bookmark_search_depth")
        key: String,
    },
}

/// Fast-path parsed arguments for the common client case.
struct FastArgs {
    repo: Option<PathBuf>,
    config_file: Option<PathBuf>,
}

/// Fast-path arg parsing for the common client case.
/// Returns `Some(args)` for a direct query, or `None` to fall through to clap.
fn try_fast_args() -> Option<FastArgs> {
    let mut args = std::env::args_os().skip(1);
    let mut repo = None;
    let mut config_file = None;
    loop {
        let arg = match args.next() {
            None => break,
            Some(a) => a,
        };
        let s = arg.to_str()?;
        match s {
            // Subcommands and help flags → fall through to clap
            "daemon" | "shutdown" | "query" | "config" | "init" | "restart" | "status"
            | "template" | "self-update" | "set-log-filter" | "directory-version" | "-h"
            | "--help" => return None,
            "-V" | "--version" => {
                print_version();
                std::process::exit(0);
            }
            "--repo" => {
                repo = Some(PathBuf::from(args.next()?));
            }
            "--config-file" => {
                config_file = Some(PathBuf::from(args.next()?));
            }
            _ => return None,
        }
    }

    Some(FastArgs { repo, config_file })
}

fn run_query(path: Option<PathBuf>, config_file: Option<&Path>) -> anyhow::Result<()> {
    let path = match path {
        Some(p) => p,
        None => match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(_) => return Ok(()),
        },
    };

    let status = client::query(&path, config_file)?;
    if !status.is_empty() {
        print!("{status}");
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    // Fast path: skip clap and tokio for the common no-subcommand client case
    if let Some(fast) = try_fast_args() {
        // --allow-root isn't handled by the fast path so if it's provided we'll never hit here
        config::check_not_root(false)?;
        return run_query(fast.repo, fast.config_file.as_deref());
    }

    // Slow path: full clap parsing, tokio runtime only started for daemon
    run_clap()
}

fn build_runtime() -> tokio::runtime::Runtime {
    // console-subscriber spawns a gRPC server that needs a multi-threaded runtime
    #[cfg(feature = "tokio-console")]
    let mut builder = {
        let mut b = tokio::runtime::Builder::new_multi_thread();
        b.worker_threads(2);
        b
    };
    #[cfg(not(feature = "tokio-console"))]
    let mut builder = tokio::runtime::Builder::new_current_thread();

    builder
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
}

fn resolve_config_path(config_file: Option<&Path>) -> Option<PathBuf> {
    config_file
        .map(|p| p.to_path_buf())
        .or_else(config::config_path)
}

pub(crate) fn run_config(action: ConfigAction, config_file: Option<&Path>) -> anyhow::Result<()> {
    match action {
        ConfigAction::Init => {
            let path = config_file
                .map(|p| p.to_path_buf())
                .map(Ok)
                .unwrap_or_else(config::config_init_path)?;
            if path.exists() {
                anyhow::bail!("config file already exists: {}", path.display());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, config::DEFAULT_CONFIG_TOML)?;
            eprintln!("Wrote default config to {}", path.display());
        }
        ConfigAction::Edit => {
            let path = resolve_config_path(config_file)
                .filter(|p| p.exists())
                .or_else(|| config::config_init_path().ok())
                .ok_or_else(|| anyhow::anyhow!("could not determine config path"))?;
            if !path.exists() {
                // Create it so the editor has something to open
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, config::DEFAULT_CONFIG_TOML)?;
            }
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
            let status = std::process::Command::new(&editor).arg(&path).status()?;
            if !status.success() {
                anyhow::bail!("{editor} exited with {status}");
            }
        }
        ConfigAction::Path => {
            let path = resolve_config_path(config_file)
                .or_else(|| config::config_init_path().ok())
                .ok_or_else(|| anyhow::anyhow!("could not determine config path"))?;
            println!("{}", path.display());
        }
        ConfigAction::Set { key, value } => {
            let path = resolve_config_path(config_file)
                .or_else(|| config::config_init_path().ok())
                .ok_or_else(|| anyhow::anyhow!("could not determine config path"))?;

            // Read existing file or start from default template
            let contents = if path.exists() {
                std::fs::read_to_string(&path)?
            } else {
                config::DEFAULT_CONFIG_TOML.to_string()
            };

            let mut doc = contents
                .parse::<toml_edit::DocumentMut>()
                .map_err(|e| anyhow::anyhow!("failed to parse config: {e}"))?;

            // Auto-detect value type: bool, integer, or string
            let toml_value = if value == "true" {
                toml_edit::value(true)
            } else if value == "false" {
                toml_edit::value(false)
            } else if let Ok(n) = value.parse::<i64>() {
                toml_edit::value(n)
            } else {
                toml_edit::value(&value)
            };

            doc[&key] = toml_value;

            // Validate the result parses as a valid Config
            let validated: config::Config = toml::from_str(doc.to_string().as_str())
                .map_err(|e| anyhow::anyhow!("invalid config after setting {key}={value}: {e}"))?;

            // Extra validation: template_name must be a builtin or user-defined template
            if key == "template_name"
                && template::builtin_template(&validated.template_name).is_none()
                && !validated.templates.contains_key(&validated.template_name)
            {
                let mut valid: Vec<&str> = template::BUILTIN_NAMES.to_vec();
                let mut user_names: Vec<&str> =
                    validated.templates.keys().map(|s| s.as_str()).collect();
                user_names.sort();
                valid.extend(user_names);
                anyhow::bail!(
                    "unknown template name \"{value}\". Valid names: {}",
                    valid.join(", ")
                );
            }

            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, doc.to_string())?;
            eprintln!("Set {key} = {value} in {}", path.display());

            // Tell the daemon to reload config so the next prompt picks up the change immediately
            if let Err(e) = client::reload_config() {
                tracing::debug!("config reload failed (daemon may not be running): {e}");
            }
        }
        ConfigAction::Get { key } => {
            let cfg = config::load_config_from(config_file)?;
            let val = match key.as_str() {
                "idle_timeout_secs" => anyhow::bail!("idle_timeout_secs has been removed"),
                "debounce_ms" => anyhow::bail!("debounce_ms has been removed"),
                "format" => cfg.format.unwrap_or_default(),
                "not_ready_format" => cfg.not_ready_format.unwrap_or_default(),
                "template_name" => cfg.template_name,
                "bookmark_search_depth" => cfg.bookmark_search_depth.to_string(),
                "color" => cfg.color.to_string(),
                "query_timeout_ms" => cfg.query_timeout_ms.to_string(),
                _ => anyhow::bail!("unknown config key: {key}"),
            };
            println!("{val}");
        }
    }
    Ok(())
}

fn print_template_samples(tmpl: &str, color: bool) {
    let samples = template::sample_statuses();
    for (label, status) in &samples {
        let rendered = template::format_status(status, tmpl, color);
        eprintln!("  {label:28} {rendered}");
    }
}

fn query_live_status(repo_path: &std::path::Path) -> anyhow::Result<template::RepoStatus> {
    let config = config::load_config()?;
    let rt = build_runtime();
    rt.block_on(async {
        let repo_path = repo_path.canonicalize()?;
        // Detect VCS type
        if repo_path.join(".jj").is_dir() {
            jj::query_jj_status(&repo_path, &config).await
        } else if repo_path.join(".git").exists() {
            git::query_git_status(&repo_path, &config).await
        } else {
            // Walk up to find repo root
            let mut p = repo_path.as_path();
            loop {
                if p.join(".jj").is_dir() {
                    return jj::query_jj_status(p, &config).await;
                }
                if p.join(".git").exists() {
                    return git::query_git_status(p, &config).await;
                }
                match p.parent() {
                    Some(parent) => p = parent,
                    None => anyhow::bail!("no VCS repo found at {}", repo_path.display()),
                }
            }
        }
    })
}

fn run_template(action: TemplateAction) -> anyhow::Result<()> {
    let color = std::io::IsTerminal::is_terminal(&std::io::stderr());

    match action {
        TemplateAction::List { name_only } => {
            let cfg = config::load_config()?;
            let mut user_names: Vec<&str> = cfg.templates.keys().map(|s| s.as_str()).collect();
            user_names.sort();

            if name_only {
                for name in template::BUILTIN_NAMES {
                    println!("{name}");
                }
                for name in &user_names {
                    println!("{name}");
                }
            } else {
                for name in template::BUILTIN_NAMES {
                    let tmpl = template::builtin_template(name).unwrap();
                    eprintln!("\x1b[1m{name}\x1b[0m:");
                    print_template_samples(tmpl, color);
                    eprintln!();
                }
                for name in &user_names {
                    let tmpl = &cfg.templates[*name];
                    eprintln!("\x1b[1m{name}\x1b[0m (user-defined):");
                    print_template_samples(tmpl, color);
                    eprintln!();
                }
            }
        }
        TemplateAction::Print { name } => {
            // Match daemon resolution order: user-defined templates first, then builtins
            let cfg = config::load_config()?;
            let tmpl = if let Some(user_tmpl) = cfg.templates.get(&name) {
                user_tmpl.clone()
            } else if let Some(builtin) = template::builtin_template(&name) {
                builtin.to_string()
            } else {
                anyhow::bail!(
                    "unknown template: {name}\nRun `vcs-status-daemon template list -n` to see available names."
                );
            };
            print!("{}", template::inline_includes(&tmpl));
        }
        TemplateAction::Format {
            template: tmpl,
            repo,
        } => {
            // Validate template first
            if let Err(e) = template::validate_template(&tmpl) {
                anyhow::bail!("{e}");
            }

            eprintln!("\x1b[1mSample outputs:\x1b[0m");
            print_template_samples(&tmpl, color);

            // Try to show live repo status
            let repo_path = repo
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_default();

            if !repo_path.as_os_str().is_empty() {
                match query_live_status(&repo_path) {
                    Ok(status) => {
                        let rendered = template::format_status(&status, &tmpl, color);
                        eprintln!();
                        eprintln!("\x1b[1mCurrent repo ({}):\x1b[0m", repo_path.display());
                        eprintln!("  {rendered}");
                    }
                    Err(e) => {
                        eprintln!();
                        eprintln!("  (could not query repo: {e})");
                    }
                }
            }
        }
    }
    Ok(())
}

/// Query the latest release version from GitHub without needing an install receipt.
/// Returns `Some(latest_version)` if a newer version exists, `None` if up to date.
fn check_latest_version() -> anyhow::Result<Option<String>> {
    use axoupdater::{ReleaseSource, ReleaseSourceType};

    let mut updater = axoupdater::AxoUpdater::new_for("vcs-status-daemon");
    let current: axoupdater::Version = env!("CARGO_PKG_VERSION")
        .parse()
        .expect("CARGO_PKG_VERSION is not a valid semver version");
    updater.set_current_version(current.clone())?;
    updater.set_release_source(ReleaseSource {
        release_type: ReleaseSourceType::GitHub,
        owner: "quodlibetor".to_string(),
        name: "vcs-status-daemon".to_string(),
        app_name: "vcs-status-daemon".to_string(),
    });

    if let Ok(token) = std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")) {
        updater.set_github_token(&token);
    }

    // query_new_version fetches from GitHub and returns the latest version.
    // We can't use is_update_needed_sync here because it requires a loaded
    // install receipt (check_receipt_is_for_this_executable).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let latest = rt.block_on(updater.query_new_version())?;
    match latest {
        Some(v) if *v > current => Ok(Some(v.to_string())),
        _ => Ok(None),
    }
}

fn self_update(check: bool) -> anyhow::Result<()> {
    let (current_version, _, _) = protocol::version_info();

    if check {
        match check_latest_version() {
            Ok(Some(new_version)) => {
                eprintln!("Update available: {current_version} -> {new_version}");
                eprintln!("Run `vcs-status-daemon self-update` to install");
            }
            Ok(None) => {
                eprintln!("Already up to date ({current_version})");
            }
            Err(e) => {
                anyhow::bail!("Failed to check for updates: {e}");
            }
        }
        return Ok(());
    }

    let mut updater = axoupdater::AxoUpdater::new_for("vcs-status-daemon");
    let current: axoupdater::Version = env!("CARGO_PKG_VERSION")
        .parse()
        .expect("CARGO_PKG_VERSION is not a valid semver version");
    updater.set_current_version(current)?;

    if let Ok(token) = std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")) {
        updater.set_github_token(&token);
    }

    // The install receipt is written by cargo-dist's shell installer. If it's
    // missing, the binary was installed some other way (homebrew, built from
    // source, etc.) and we shouldn't overwrite it.
    if let Err(e) = updater.load_receipt() {
        tracing::info!(error = %e, "Unable to load install receipt, recommending package manager");
        let update_hint = match check_latest_version() {
            Ok(Some(new_version)) => {
                format!(" (update available: {current_version} -> {new_version})")
            }
            Ok(None) => format!(" (already up to date, {current_version})"),
            Err(_) => String::new(),
        };
        anyhow::bail!(
            "vcs-status-daemon was not installed via the shell installer{update_hint}.\n\
            Use your package manager to update instead."
        );
    }

    if !updater.is_update_needed_sync()? {
        eprintln!("Already up to date ({current_version})");
        return Ok(());
    }

    if let Some(result) = updater.run_sync()? {
        eprintln!("Updated vcs-status-daemon to {}", result.new_version_tag);
    }

    // Restart the daemon so it picks up the new binary
    if client::shutdown().is_ok() {
        eprintln!("Stopped running daemon (it will restart on next query)");
    }

    Ok(())
}

fn run_clap() -> anyhow::Result<()> {
    let cli = Cli::parse();
    config::check_not_root(cli.allow_root)?;
    let cf = cli.config_file.as_deref();

    match cli.command {
        Some(Commands::Daemon { dir, config_file }) => {
            let runtime_dir = match dir {
                Some(d) => d,
                None => config::runtime_dir()?,
            };
            let log_filter_handle = daemon::init_logging(&runtime_dir);
            // Daemon's own --config-file takes priority over the top-level one
            let daemon_cf = config_file.as_deref().or(cf);
            let (config, config_err) = match config::load_config_from(daemon_cf) {
                Ok(c) => (c, None),
                Err(e) => {
                    let msg = format!("config error: {e}");
                    eprintln!("warning: {msg}");
                    (config::Config::default(), Some(msg))
                }
            };
            // Resolve the config file path for hot-reload watching.
            // Use explicit --config-file if given, otherwise fall back to the default path.
            let watch_cf = daemon_cf
                .map(|p| p.to_path_buf())
                .or_else(config::config_path);
            let rt = build_runtime();
            rt.block_on(daemon::run_daemon(
                config,
                runtime_dir,
                watch_cf,
                config_err,
                log_filter_handle,
            ))?;
            // Bound the shutdown: don't wait forever for in-flight spawn_blocking
            // tasks (e.g. jj-lib refresh) when the daemon is exiting.
            rt.shutdown_timeout(std::time::Duration::from_secs(2));
        }
        Some(Commands::Shutdown) => {
            client::shutdown()?;
        }
        Some(Commands::Restart) => {
            client::restart(cf)?;
        }
        Some(Commands::Status) => {
            client::status()?;
        }
        Some(Commands::Init { shell, starship }) => {
            init::run(&shell, starship)?;
        }
        Some(Commands::Config { action }) => {
            run_config(action, cf)?;
        }
        Some(Commands::Template { action }) => {
            run_template(action)?;
        }
        Some(Commands::SetLogFilter { filter }) => {
            client::set_log_filter(&filter)?;
            eprintln!("Log filter set to: {filter}");
        }
        Some(Commands::SelfUpdate { check }) => {
            self_update(check)?;
        }
        Some(Commands::DirectoryVersion) => {
            println!("{}", daemon::DIRECTORY_VERSION);
        }
        Some(Commands::Query { repo }) => {
            run_query(repo.or(cli.repo), cf)?;
        }
        None => {
            run_query(cli.repo, cf)?;
        }
    }

    Ok(())
}
