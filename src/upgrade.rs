use std::ffi::CString;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use anyhow::{Context, Result};
use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use nix::unistd::{dup, execv};

/// Clears the CLOEXEC flag on the provided file descriptor so it survives an exec.
///
/// # Errors
///
/// Returns an error if `fcntl` fails while reading or updating the descriptor flags.
pub fn clear_cloexec(fd: RawFd) -> Result<()> {
    let flags = FdFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFD)?);
    if !flags.contains(FdFlag::FD_CLOEXEC) {
        return Ok(());
    }
    let mut new_flags = flags;
    new_flags.remove(FdFlag::FD_CLOEXEC);
    fcntl(fd, FcntlArg::F_SETFD(new_flags))
        .with_context(|| format!("failed to clear FD_CLOEXEC on fd {fd}"))?;
    Ok(())
}

/// Executes the current binary again, passing the provided environment overrides.
///
/// # Errors
///
/// Returns an error if the path contains interior NUL bytes or if `execv` fails.
pub fn exec_with_env(path: &Path, env: &[(&str, &str)]) -> Result<()> {
    let c_path = CString::new(path.as_os_str().as_bytes())
        .context("failed to convert exec path to CString")?;
    let args = [c_path.clone()];

    for (key, value) in env {
        std::env::set_var(key, value);
    }

    execv(&c_path, &args).context("execv failed")?;
    Ok(())
}

/// Helper that ensures the FD stays open across upgrades by dup'ing into an `OwnedFd`.
///
/// # Errors
///
/// Returns an error if duplicating the file descriptor fails.
pub fn dup_fd(fd: RawFd) -> Result<OwnedFd> {
    let duped = dup(fd)?;
    Ok(unsafe { OwnedFd::from_raw_fd(duped) })
}

fn _assert_send_sync()
where
    OwnedFd: Send + Sync,
{
}
