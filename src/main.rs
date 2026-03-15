mod client;
mod config;
mod daemon;
mod jj;
mod protocol;
mod watcher;

use std::path::{Path, PathBuf};

fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        if dir.join(".jj").is_dir() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Fast-path arg parsing for the common client case.
/// Returns `Some(repo)` for a direct query, or `None` to fall through to clap.
fn try_fast_args() -> Option<Option<PathBuf>> {
    let mut args = std::env::args_os().skip(1);
    let first = match args.next() {
        None => return Some(None), // no args → query cwd
        Some(a) => a,
    };
    let s = first.to_str()?;
    match s {
        // Subcommands and help flags → fall through to clap
        "daemon" | "shutdown" | "query" | "-h" | "--help" | "--version" => None,
        "--repo" => {
            let repo = args.next().map(PathBuf::from);
            Some(repo)
        }
        _ => None,
    }
}

async fn run_query(repo: Option<PathBuf>) -> anyhow::Result<()> {
    let repo_path = repo.or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| find_repo_root(&cwd))
    });

    let Some(repo_path) = repo_path else {
        return Ok(());
    };

    let status = client::query(&repo_path).await?;
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

async fn run_clap() -> anyhow::Result<()> {
    use clap::{Parser, Subcommand};

    #[derive(Parser)]
    #[command(name = "jj-status-daemon")]
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
    }

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Daemon { socket }) => {
            let config = config::load_config()?;
            let socket_path = socket.unwrap_or_else(config::socket_path);
            daemon::run_daemon(config, socket_path).await?;
        }
        Some(Commands::Shutdown) => {
            client::shutdown().await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_find_repo_root() {
        let dir = TempDir::new().unwrap();
        let jj_dir = dir.path().join(".jj");
        std::fs::create_dir(&jj_dir).unwrap();

        let sub = dir.path().join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();

        assert_eq!(find_repo_root(&sub), Some(dir.path().to_path_buf()));
    }

    #[test]
    fn test_find_repo_root_at_root() {
        let dir = TempDir::new().unwrap();
        let jj_dir = dir.path().join(".jj");
        std::fs::create_dir(&jj_dir).unwrap();

        assert_eq!(find_repo_root(dir.path()), Some(dir.path().to_path_buf()));
    }

    #[test]
    fn test_find_repo_root_not_found() {
        let dir = TempDir::new().unwrap();
        assert_eq!(find_repo_root(dir.path()), None);
    }
}
