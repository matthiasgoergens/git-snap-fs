//! Repository access helpers for GitSnapFS.
//!
//! These abstractions wrap `gix` primitives so the filesystem code can remain
//! largely agnostic of the underlying git library. Everything here is still
//! evolving; the initial implementation focuses on the bare minimum needed
//! for the FUSE bindings to resolve commits and trees lazily.

use anyhow::Result;
use gix::object::tree::EntryMode;
use gix::ObjectId;

/// Metadata for a single tree entry queried from the repository.
#[derive(Debug, Clone)]
pub struct TreeEntry {
    pub mode: EntryMode,
    pub oid: ObjectId,
    pub name: String,
}

/// Minimal repository wrapper. The complete functionality will arrive in later
/// steps; for now we only keep enough structure in place to let the rest of
/// the crate compile.
#[derive(Debug)]
pub struct Repository {
    inner: gix::Repository,
}

impl Repository {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let repo = gix::open(path)?;
        Ok(Self { inner: repo })
    }

    pub fn inner(&self) -> &gix::Repository {
        &self.inner
    }
}
