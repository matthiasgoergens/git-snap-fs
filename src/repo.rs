//! Repository access helpers for GitSnapFS.
//!
//! These abstractions wrap `gix` primitives so the filesystem code can remain
//! largely agnostic of the underlying git library. Everything here is still
//! evolving; the initial implementation focuses on the bare minimum needed
//! for the FUSE bindings to resolve commits and trees lazily.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use gix::{bstr::ByteSlice, Commit, ObjectId};

/// Minimal repository wrapper.
#[derive(Debug)]
pub struct Repository {
    inner: gix::Repository,
}

impl Repository {
    pub fn open(path: &Path) -> Result<Self> {
        let repo = gix::open(path).with_context(|| format!("failed to open repository at {}", path.display()))?;
        Ok(Self { inner: repo })
    }

    pub fn find_commit(&self, id: ObjectId) -> Result<Commit<'_>> {
        Ok(self.inner.find_commit(id)?)
    }

    pub fn resolve_full_commit_id(&self, hex: &str) -> Result<ObjectId> {
        let id = self.inner.rev_parse_single(hex.as_bytes().as_bstr())?.detach();
        // Ensure the target is a commit; this surfaces a clearer error if the
        // id resolves to a blob/tree/tag.
        let commit = self.inner.find_commit(id)?;
        Ok(commit.id)
    }

    pub fn resolve_head(&self) -> Result<ObjectId> {
        let mut head = self.inner.head()?;
        let id = head
            .try_peel_to_id()?
            .ok_or_else(|| anyhow!("repository HEAD is unborn and has no target commit"))?
            .detach();
        let commit = self.inner.find_commit(id)?;
        Ok(commit.id)
    }

    pub fn inner(&self) -> &gix::Repository {
        &self.inner
    }
}
