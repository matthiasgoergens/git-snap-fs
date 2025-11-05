//! Repository access helpers for GitSnapFS.
//!
//! These abstractions wrap `gix` primitives so the filesystem code can remain
//! largely agnostic of the underlying git library.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use gix::{bstr::ByteSlice, ObjectId, ThreadSafeRepository};

/// Minimal repository wrapper that keeps a thread-safe handle.
#[derive(Debug)]
pub struct Repository {
    inner: ThreadSafeRepository,
}

impl Repository {
    pub fn open(path: &Path) -> Result<Self> {
        let repo = ThreadSafeRepository::open(path)
            .with_context(|| format!("failed to open repository at {}", path.display()))?;
        Ok(Self { inner: repo })
    }

    pub fn resolve_full_commit_id(&self, hex: &str) -> Result<ObjectId> {
        let repo = self.inner.to_thread_local();
        let id = repo.rev_parse_single(hex.as_bytes().as_bstr())?.detach();
        let commit = repo.find_commit(id.clone())?;
        Ok(commit.id)
    }

    pub fn resolve_head(&self) -> Result<ObjectId> {
        let repo = self.inner.to_thread_local();
        let mut head = repo.head()?;
        let id = head
            .try_peel_to_id()?
            .ok_or_else(|| anyhow!("repository HEAD is unborn and has no target commit"))?
            .detach();
        let commit = repo.find_commit(id.clone())?;
        Ok(commit.id)
    }

    pub fn list_branches(&self) -> Result<Vec<(String, ObjectId)>> {
        let repo = self.inner.to_thread_local();
        let platform = repo.references()?;
        let iter = platform.local_branches()?.peeled()?;
        collect_refs(iter, b"refs/heads/")
    }

    pub fn list_tags(&self) -> Result<Vec<(String, ObjectId)>> {
        let repo = self.inner.to_thread_local();
        let platform = repo.references()?;
        let iter = platform.tags()?.peeled()?;
        collect_refs(iter, b"refs/tags/")
    }

    pub fn thread_local(&self) -> gix::Repository {
        self.inner.to_thread_local()
    }
}

fn collect_refs(
    iter: gix::reference::iter::Iter<'_, '_>,
    prefix: &[u8],
) -> Result<Vec<(String, ObjectId)>> {
    let mut refs = Vec::new();
    for reference in iter {
        let mut reference = reference.map_err(|err| anyhow!(err))?;
        let id = reference.peel_to_id()?.detach();
        let name_bytes = reference.name().as_bstr().as_bytes();
        let short_bytes = name_bytes.strip_prefix(prefix).unwrap_or(name_bytes);
        let short = String::from_utf8_lossy(short_bytes).into_owned();
        refs.push((short, id));
    }
    refs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(refs)
}
