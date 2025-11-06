use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};
use clap::Parser;
use fuse_backend_rs::api::server::Server;
use fuse_backend_rs::transport::FuseSession;
use tracing::error;
use tracing_subscriber::EnvFilter;

use gitsnapfs::fs::GitSnapFs;
use gitsnapfs::repo::Repository;

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

    if cli.takeover_fuse_fd.is_some() {
        bail!("takeover via existing FUSE fd is not supported yet in the MVP");
    }

    let repo = Repository::open(&cli.repo)?;
    let fs = GitSnapFs::new(repo);

    tracing::info!(
        "GitSnapFS mounting (repo: {}, mountpoint: {})",
        cli.repo.display(),
        cli.mountpoint.display()
    );

    let runtime = FuseRuntime::new(fs, &cli.mountpoint, cli.allow_other)?;
    runtime.serve()
}

struct FuseRuntime {
    server: Arc<Server<Arc<GitSnapFs>>>,
    session: FuseSession,
}

impl FuseRuntime {
    fn new(fs: GitSnapFs, mountpoint: &Path, allow_other: bool) -> Result<Self> {
        let server = Arc::new(Server::new(Arc::new(fs)));
        let mut session =
            FuseSession::new_with_autounmount(mountpoint, "gitsnapfs", "gitsnapfs", true, true)?;
        session.set_allow_other(allow_other);
        session.mount()?;
        Ok(Self { server, session })
    }

    fn serve(self) -> Result<()> {
        let mut channel = self.session.new_channel()?;
        while let Some((reader, writer)) = channel.get_request()? {
            if let Err(err) = self
                .server
                .handle_message(reader, writer.into(), None, None)
            {
                match err {
                    fuse_backend_rs::Error::EncodeMessage(ioe) => {
                        if let Some(libc::EBADF) = ioe.raw_os_error() {
                            break;
                        }
                        error!(?ioe, "encoding FUSE message failed");
                    }
                    other => error!(?other, "handling FUSE message failed"),
                }
            }
        }
        Ok(())
    }
}

impl Drop for FuseRuntime {
    fn drop(&mut self) {
        if let Err(err) = self.session.umount() {
            error!(?err, "failed to unmount FUSE session");
        }
    }
}
