use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "gitsnapfs",
    version,
    about = "Git snapshots as a read-only FUSE filesystem"
)]
struct Cli {
    /// Path to the target Git repository (.git dir or bare repo).
    #[arg(long)]
    repo: PathBuf,

    /// Mount point for the FUSE filesystem.
    #[arg(long)]
    mountpoint: PathBuf,

    /// Allow other users to access the mount.
    #[arg(long)]
    allow_other: bool,

    /// Attribute TTL in seconds for commit tree entries.
    #[arg(long, default_value_t = 300)]
    attr_ttl: u64,

    /// Entry TTL in seconds for commit tree entries.
    #[arg(long, default_value_t = 300)]
    entry_ttl: u64,

    /// TTL in seconds for branch/tag resolutions.
    #[arg(long, default_value_t = 2)]
    ref_ttl: u64,

    /// Maximum number of tree objects cached in memory.
    #[arg(long, default_value_t = 4096)]
    tree_cache: usize,

    /// Cache capacity for small blobs in bytes.
    #[arg(long, default_value_t = 134_217_728)]
    blob_small_cache: usize,

    /// Adopt an existing FUSE file descriptor instead of mounting.
    #[arg(long)]
    takeover_fuse_fd: Option<i32>,

    /// Optional path to persist inode collision state.
    #[arg(long)]
    state_file: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    tracing::info!(
        "GitSnapFS starting (repo: {}, mountpoint: {})",
        cli.repo.display(),
        cli.mountpoint.display()
    );

    // Actual FUSE mounting and event loop will be implemented in later steps.
    tracing::warn!("GitSnapFS is not yet fully implemented.");

    Ok(())
}
