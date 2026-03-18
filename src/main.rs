mod client;
mod config;
mod daemon;
mod git;
mod init;
mod jj;
mod protocol;
mod template;
mod watcher;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "vcs-status-daemon")]
#[command(about = "Fast jj status for shell prompts")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to the jj repository (default: auto-detect)
    #[arg(long)]
    repo: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the background daemon
    Daemon {
        /// Runtime directory (contains socket, cache, and log files)
        #[arg(long)]
        dir: Option<PathBuf>,
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
}

#[derive(Subcommand)]
enum TemplateAction {
    /// List all built-in templates with representative outputs
    List,
    /// Render a template with representative examples and the current repo
    Format {
        /// Template format string (Tera/Jinja2 syntax)
        template: String,
        /// Path to a repository to show live status
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Write a default config file with explanatory comments
    Init,
    /// Open the config file in $EDITOR
    Edit,
    /// Print the config file path
    Path,
}

/// Fast-path arg parsing for the common client case.
/// Returns `Some(path)` for a direct query, or `None` to fall through to clap.
fn try_fast_args() -> Option<Option<PathBuf>> {
    let mut args = std::env::args_os().skip(1);
    let mut repo = None;

    loop {
        let arg = match args.next() {
            None => break,
            Some(a) => a,
        };
        let s = arg.to_str()?;
        match s {
            // Subcommands and help flags → fall through to clap
            "daemon" | "shutdown" | "query" | "config" | "init" | "restart" | "status"
            | "template" | "-h" | "--help" | "--version" => return None,
            "--repo" => {
                repo = Some(PathBuf::from(args.next()?));
            }
            _ => return None,
        }
    }

    Some(repo)
}

fn run_query(path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = match path {
        Some(p) => p,
        None => match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(_) => return Ok(()),
        },
    };

    let status = client::query(&path)?;
    if !status.is_empty() {
        print!("{status}");
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    // Fast path: skip clap and tokio for the common no-subcommand client case
    if let Some(repo) = try_fast_args() {
        return run_query(repo);
    }

    // Slow path: full clap parsing, tokio runtime only started for daemon
    run_clap()
}

fn build_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
}

fn run_config(action: ConfigAction) -> anyhow::Result<()> {
    match action {
        ConfigAction::Init => {
            let path = config::config_init_path()?;
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
            let path = config::config_path()
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
            let path = config::config_path()
                .or_else(|| config::config_init_path().ok())
                .ok_or_else(|| anyhow::anyhow!("could not determine config path"))?;
            println!("{}", path.display());
        }
    }
    Ok(())
}

fn print_template_samples(tmpl: &str, color: bool) {
    let samples = template::sample_statuses();
    for (label, status) in &samples {
        let rendered = template::format_status(status, tmpl, color);
        eprintln!("  {label:25} {rendered}");
    }
}

fn query_live_status(repo_path: &std::path::Path) -> anyhow::Result<template::RepoStatus> {
    let config = config::load_config()?;
    let rt = build_runtime();
    rt.block_on(async {
        let repo_path = repo_path.canonicalize()?;
        // Detect VCS type
        if repo_path.join(".jj").is_dir() {
            jj::query_jj_status(&repo_path, &config, false).await
        } else if repo_path.join(".git").exists() {
            git::query_git_status(&repo_path, &config).await
        } else {
            // Walk up to find repo root
            let mut p = repo_path.as_path();
            loop {
                if p.join(".jj").is_dir() {
                    return jj::query_jj_status(p, &config, false).await;
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
        TemplateAction::List => {
            for name in template::BUILTIN_NAMES {
                let tmpl = template::builtin_template(name).unwrap();
                eprintln!("\x1b[1m{name}\x1b[0m:");
                print_template_samples(tmpl, color);
                eprintln!();
            }
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

fn run_clap() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Daemon { dir }) => {
            let runtime_dir = dir.unwrap_or_else(config::runtime_dir);
            daemon::init_logging(&runtime_dir);
            let config = config::load_config()?;
            build_runtime().block_on(daemon::run_daemon(config, runtime_dir))?;
        }
        Some(Commands::Shutdown) => {
            client::shutdown()?;
        }
        Some(Commands::Restart) => {
            client::restart()?;
        }
        Some(Commands::Status) => {
            client::status()?;
        }
        Some(Commands::Init { shell, starship }) => {
            init::run(&shell, starship)?;
        }
        Some(Commands::Config { action }) => {
            run_config(action)?;
        }
        Some(Commands::Template { action }) => {
            run_template(action)?;
        }
        Some(Commands::Query { repo }) => {
            run_query(repo.or(cli.repo))?;
        }
        None => {
            run_query(cli.repo)?;
        }
    }

    Ok(())
}
