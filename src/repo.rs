//! Repository access helpers for `GitSnapFS`.
//!
//! These abstractions wrap `gix` primitives so the filesystem code can remain
//! largely agnostic of the underlying git library.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use gix::objs::Kind;
use gix::{self, bstr::ByteSlice, ObjectId, ThreadSafeRepository};
use itertools::Itertools;

use crate::inode::inode_to_hex_prefix;

/// Minimal repository wrapper that keeps a thread-safe handle.
#[derive(Debug)]
pub struct Repository {
    inner: ThreadSafeRepository,
}

impl Repository {
    /// Open a repository at `path`.
    ///
    /// # Errors
    ///
    /// Returns an error if `gix` cannot open the repository at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let repo = ThreadSafeRepository::open(path)
            .with_context(|| format!("failed to open repository at {}", path.display()))?;
        Ok(Self { inner: repo })
    }

    /// Resolve a hex commit id string to its full 40-byte `ObjectId`.
    ///
    /// # Errors
    ///
    /// Returns an error if the hex string does not point to a commit reachable in the repository.
    pub fn resolve_full_commit_id(&self, hex: &str) -> Result<ObjectId> {
        let repo = self.inner.to_thread_local();
        let id = repo.rev_parse_single(hex.as_bytes().as_bstr())?.detach();
        let commit = repo.find_commit(id)?;
        Ok(commit.id)
    }

    /// Resolve the current `HEAD` reference to its commit `ObjectId`.
    ///
    /// # Errors
    ///
    /// Returns an error if `HEAD` cannot be peeled to a commit (for example in an unborn branch).
    pub fn resolve_head(&self) -> Result<ObjectId> {
        let repo = self.inner.to_thread_local();
        let mut head = repo.head()?;
        let id = head
            .try_peel_to_id()?
            .ok_or_else(|| anyhow!("repository HEAD is unborn and has no target commit"))?
            .detach();
        let commit = repo.find_commit(id)?;
        Ok(commit.id)
    }

    /// Enumerate local branches and the commits they reference.
    ///
    /// # Errors
    ///
    /// Returns an error if the reference database cannot be enumerated.
    pub fn list_branches(&self) -> Result<Vec<(String, ObjectId)>> {
        let repo = self.inner.to_thread_local();
        let platform = repo.references()?;
        let iter = platform.local_branches()?.peeled()?;
        collect_refs(iter, b"refs/heads/")
    }

    /// Enumerate tags and the commits they reference.
    ///
    /// # Errors
    ///
    /// Returns an error if the reference database cannot be enumerated.
    pub fn list_tags(&self) -> Result<Vec<(String, ObjectId)>> {
        let repo = self.inner.to_thread_local();
        let platform = repo.references()?;
        let iter = platform.tags()?.peeled()?;
        collect_refs(iter, b"refs/tags/")
    }

    /// List every commit object stored in the repository database.
    ///
    /// # Errors
    ///
    /// Returns an error if iterating the object database or decoding objects fails.
    pub fn list_commits(&self) -> Result<Vec<ObjectId>> {
        let repo = self.inner.to_thread_local();
        let store = repo.objects.store();
        let all = gix::odb::store::iter::AllObjects::new(&store).map_err(|err| anyhow!(err))?;
        Ok(all
            .flatten()
            .filter(|oid| {
                if let Ok(object) = repo.find_object(*oid) {
                    object.kind == Kind::Commit
                } else {
                    false
                }
            })
            .unique()
            .collect::<Vec<_>>())
    }

    pub fn thread_local(&self) -> gix::Repository {
        self.inner.to_thread_local()
    }

    /// Resolve an inode value back to a unique object id by treating it as a hexadecimal prefix.
    ///
    /// # Errors
    ///
    /// Returns an error if the hexadecimal prefix cannot be resolved to an object in the repository.
    pub fn resolve_inode(&self, inode: u64) -> Result<ObjectId> {
        let hex = inode_to_hex_prefix(inode);
        let repo = self.inner.to_thread_local();
        let id = repo.rev_parse_single(hex.as_bytes().as_bstr())?.detach();
        Ok(id)
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
    Ok(refs)
}
