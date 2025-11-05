//! Repository access helpers for `GitSnapFS`.
//!
//! These abstractions wrap `gix` primitives so the filesystem code can remain
//! largely agnostic of the underlying git library.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use gix::{bstr::ByteSlice, ObjectId, ThreadSafeRepository};
use gix::{revision::walk::Sorting, traverse::commit::simple::CommitTimeOrder};

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

    /// List commits reachable from repository heads, branches, and tags.
    ///
    /// # Errors
    ///
    /// Returns an error if traversing the commit graph fails.
    pub fn list_commits(&self) -> Result<Vec<ObjectId>> {
        let mut tips = Vec::new();
        let mut seen = HashSet::new();

        if let Ok(head) = self.resolve_head() {
            if seen.insert(head) {
                tips.push(head);
            }
        }

        for (_, id) in self.list_branches()? {
            if seen.insert(id) {
                tips.push(id);
            }
        }

        for (_, id) in self.list_tags()? {
            if seen.insert(id) {
                tips.push(id);
            }
        }

        if tips.is_empty() {
            return Ok(Vec::new());
        }

        let repo = self.inner.to_thread_local();
        let walk = repo
            .rev_walk(tips)
            .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
            .all()
            .map_err(|err| anyhow!(err))?;

        let mut commits = Vec::new();
        let mut seen_commits = HashSet::new();
        for item in walk {
            let info = item.map_err(|err| anyhow!(err))?;
            if seen_commits.insert(info.id) {
                commits.push(info.id);
            }
        }

        Ok(commits)
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
