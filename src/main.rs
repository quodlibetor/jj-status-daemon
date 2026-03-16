mod client;
mod config;
mod daemon;
mod git;
mod jj;
mod protocol;
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
        /// Unix socket path (overrides env var)
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Query the daemon for status (default)
    Query {
        /// Path to the jj repository
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Shut down the daemon
    Shutdown,
    /// Manage configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
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
    let first = match args.next() {
        None => return Some(None), // no args → query cwd
        Some(a) => a,
    };
    let s = first.to_str()?;
    match s {
        // Subcommands and help flags → fall through to clap
        "daemon" | "shutdown" | "query" | "config" | "-h" | "--help" | "--version" => None,
        "--repo" => {
            let path = args.next().map(PathBuf::from);
            Some(path)
        }
        _ => None,
    }
}

async fn run_query(path: Option<PathBuf>) -> anyhow::Result<()> {
    let path = match path {
        Some(p) => p,
        None => match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(_) => return Ok(()),
        },
    };

    let status = client::query(&path).await?;
    if !status.is_empty() {
        print!("{status}");
    }
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Fast path: skip clap for the common no-subcommand client case
    if let Some(repo) = try_fast_args() {
        return run_query(repo).await;
    }

    // Slow path: full clap parsing for daemon/shutdown/query/help
    run_clap().await
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

async fn run_clap() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Daemon { socket }) => {
            daemon::init_logging();
            let config = config::load_config()?;
            let socket_path = socket.unwrap_or_else(config::socket_path);
            daemon::run_daemon(config, socket_path).await?;
        }
        Some(Commands::Shutdown) => {
            client::shutdown().await?;
        }
        Some(Commands::Config { action }) => {
            run_config(action)?;
        }
        Some(Commands::Query { repo }) => {
            run_query(repo.or(cli.repo)).await?;
        }
        None => {
            run_query(cli.repo).await?;
        }
    }

    Ok(())
}
