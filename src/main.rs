mod client;
mod config;
mod daemon;
mod jj;
mod protocol;
mod watcher;

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

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
    Daemon,
    /// Query the daemon for status (default)
    Query {
        /// Path to the jj repository
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Shut down the daemon
    Shutdown,
}

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

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let config = config::load_config()?;

    match cli.command {
        Some(Commands::Daemon) => {
            daemon::run_daemon(config).await?;
        }
        Some(Commands::Shutdown) => {
            client::shutdown(&config).await?;
        }
        Some(Commands::Query { repo }) => {
            let repo_path = repo
                .or(cli.repo)
                .or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|cwd| find_repo_root(&cwd))
                });

            let Some(repo_path) = repo_path else {
                return Ok(());
            };

            let status = client::query(&repo_path, &config).await?;
            if !status.is_empty() {
                print!("{status}");
            }
        }
        None => {
            let repo_path = cli.repo
                .or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|cwd| find_repo_root(&cwd))
                });

            let Some(repo_path) = repo_path else {
                // Not in a jj repo - exit silently
                return Ok(());
            };

            let status = client::query(&repo_path, &config).await?;
            if !status.is_empty() {
                print!("{status}");
            }
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
