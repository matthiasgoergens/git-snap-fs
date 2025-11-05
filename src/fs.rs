//! FUSE filesystem implementation for `GitSnapFS`.

use std::collections::HashMap;
use std::convert::TryFrom;
use std::ffi::CStr;
use std::io;
use std::str;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuse_backend_rs::abi::fuse_abi::{stat64, CreateIn, ROOT_ID};
use fuse_backend_rs::api::filesystem::{
    Context, DirEntry, Entry, FileSystem, OpenOptions, SetattrValid, ZeroCopyReader, ZeroCopyWriter,
};
use gix::bstr::ByteSlice;
use gix::object::tree::{EntryKind, EntryMode};
use gix::ObjectId;
use libc::{S_IFDIR, S_IFLNK, S_IFREG};
use parking_lot::RwLock;

use crate::inode::inode_from_oid;
use crate::repo::Repository;

#[derive(Debug, Clone)]
struct CommitMeta {
    id: ObjectId,
    tree: ObjectId,
    time: SystemTime,
}

#[derive(Debug, Clone)]
enum NodeKind {
    Commit {
        meta: Arc<CommitMeta>,
    },
    Tree {
        meta: Arc<CommitMeta>,
        tree: ObjectId,
    },
    Blob {
        meta: Arc<CommitMeta>,
        oid: ObjectId,
        executable: bool,
        size: u64,
    },
    Symlink {
        meta: Arc<CommitMeta>,
        target: Vec<u8>,
    },
    Submodule {
        meta: Arc<CommitMeta>,
        _oid: ObjectId,
    },
    SyntheticSymlink {
        target: Vec<u8>,
        time: SystemTime,
    },
}

#[derive(Debug, Clone)]
struct Node {
    inode: u64,
    parent: Option<u64>,
    kind: NodeKind,
}

const ROOT_ATTR_MODE: u32 = S_IFDIR | 0o755;
const DIRECTORY_ATTR_MODE: u32 = S_IFDIR | 0o755;
const SYMLINK_ATTR_MODE: u32 = S_IFLNK | 0o777;

const INODE_COMMITS: u64 = 2;
const INODE_BRANCHES: u64 = 3;
const INODE_TAGS: u64 = 4;
const INODE_HEAD: u64 = 5;

const NAMESPACE_BRANCH: u8 = 1;
const NAMESPACE_TAG: u8 = 2;

const ENTRY_TTL: Duration = Duration::from_secs(1);
const ATTR_TTL: Duration = Duration::from_secs(1);

const NAME_DOT: &[u8] = b".";
const NAME_DOT_DOT: &[u8] = b"..";
const NAME_COMMITS: &[u8] = b"commits";
const NAME_BRANCHES: &[u8] = b"branches";
const NAME_TAGS: &[u8] = b"tags";
const NAME_HEAD: &[u8] = b"HEAD";

pub struct GitSnapFs {
    repo: Arc<Repository>,
    start_time: SystemTime,
    nodes: RwLock<HashMap<u64, Node>>,
}

fn synthetic_inode(namespace: u8, name: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    namespace.hash(&mut hasher);
    name.hash(&mut hasher);
    let hash = hasher.finish();
    (u64::from(namespace) << 56) | (hash & 0x00FF_FFFF_FFFF_FFFF)
}

impl GitSnapFs {
    pub fn new(repo: Repository) -> Self {
        Self {
            repo: Arc::new(repo),
            start_time: SystemTime::now(),
            nodes: RwLock::new(HashMap::new()),
        }
    }

    fn root_attr(&self) -> stat64 {
        build_dir_attr(ROOT_ID, ROOT_ATTR_MODE, self.start_time)
    }

    fn special_dir_entry(&self, inode: u64) -> Entry {
        Entry {
            inode,
            generation: 0,
            attr: build_dir_attr(inode, DIRECTORY_ATTR_MODE, self.start_time),
            attr_flags: 0,
            attr_timeout: ATTR_TTL,
            entry_timeout: ENTRY_TTL,
        }
    }

    fn head_entry(&self) -> io::Result<Entry> {
        let (meta, target) = self.head_metadata()?;
        let attr = build_symlink_attr(
            INODE_HEAD,
            SYMLINK_ATTR_MODE,
            meta.time,
            target.len() as u64,
        );
        Ok(Entry {
            inode: INODE_HEAD,
            generation: 0,
            attr,
            attr_flags: 0,
            attr_timeout: ATTR_TTL,
            entry_timeout: ENTRY_TTL,
        })
    }

    fn make_entry(inode: u64, attr: stat64) -> Entry {
        Entry {
            inode,
            generation: 0,
            attr,
            attr_flags: 0,
            attr_timeout: ATTR_TTL,
            entry_timeout: ENTRY_TTL,
        }
    }

    fn lookup_commit(&self, name: &[u8]) -> io::Result<Entry> {
        let name_str =
            str::from_utf8(name).map_err(|_| io::Error::from_raw_os_error(libc::ENOENT))?;

        let commit_id = self
            .repo
            .resolve_full_commit_id(name_str)
            .map_err(io::Error::other)?;

        let (meta, inode) = self.ensure_commit_node(commit_id)?;
        let attr = build_dir_attr(inode, DIRECTORY_ATTR_MODE, meta.time);
        Ok(Self::make_entry(inode, attr))
    }

    fn ensure_commit_node(&self, commit_id: ObjectId) -> io::Result<(Arc<CommitMeta>, u64)> {
        let inode = inode_from_oid(&commit_id);
        if let Some(existing) = self.nodes.read().get(&inode) {
            if let NodeKind::Commit { meta } = &existing.kind {
                return Ok((meta.clone(), inode));
            }
        }

        let repo = self.repo.thread_local();
        let commit = repo.find_commit(commit_id).map_err(io::Error::other)?;
        let tree_id = commit.tree_id().map_err(io::Error::other)?.detach();
        let time = commit_time_to_system(&commit, self.start_time);

        let meta = Arc::new(CommitMeta {
            id: commit_id,
            tree: tree_id,
            time,
        });

        let node = Node {
            inode,
            parent: Some(INODE_COMMITS),
            kind: NodeKind::Commit { meta: meta.clone() },
        };
        self.nodes.write().insert(inode, node);
        Ok((meta, inode))
    }

    fn node_for_inode(&self, inode: u64) -> io::Result<Node> {
        self.nodes
            .read()
            .get(&inode)
            .cloned()
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
    }

    fn lookup_reference_symlink(
        &self,
        name: &[u8],
        namespace: u8,
        refs: Vec<(String, ObjectId)>,
        parent: u64,
    ) -> io::Result<Entry> {
        let name_str =
            str::from_utf8(name).map_err(|_| io::Error::from_raw_os_error(libc::ENOENT))?;
        let commit_id = refs
            .into_iter()
            .find(|(ref_name, _)| ref_name == name_str)
            .map(|(_, id)| id)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;

        let (meta, _) = self.ensure_commit_node(commit_id)?;
        let target = format!("../commits/{}", meta.id).into_bytes();
        let inode = synthetic_inode(namespace, name);
        let node = Node {
            inode,
            parent: Some(parent),
            kind: NodeKind::SyntheticSymlink {
                target: target.clone(),
                time: meta.time,
            },
        };
        self.nodes.write().insert(inode, node);

        let attr = build_symlink_attr(inode, SYMLINK_ATTR_MODE, meta.time, target.len() as u64);
        Ok(Self::make_entry(inode, attr))
    }

    fn readdir_refs(
        &self,
        dir_inode: u64,
        mut offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
        refs: Vec<(String, ObjectId)>,
        namespace: u8,
    ) -> io::Result<()> {
        if offset == 0 {
            if add_entry(DirEntry {
                ino: dir_inode,
                offset: 1,
                type_: u32::from(libc::DT_DIR),
                name: NAME_DOT,
            })? == 0
            {
                return Ok(());
            }
            offset = 1;
        }

        if offset == 1 {
            if add_entry(DirEntry {
                ino: ROOT_ID,
                offset: 2,
                type_: u32::from(libc::DT_DIR),
                name: NAME_DOT_DOT,
            })? == 0
            {
                return Ok(());
            }
            offset = 2;
        }

        for (index, (name, commit_id)) in refs.into_iter().enumerate() {
            let entry_offset = (index as u64) + 3;
            if offset > entry_offset {
                continue;
            }

            let name_bytes = name.as_bytes();
            let inode = synthetic_inode(namespace, name_bytes);
            let (meta, _) = self.ensure_commit_node(commit_id)?;
            let target = format!("../commits/{}", meta.id).into_bytes();
            {
                let mut nodes = self.nodes.write();
                nodes.insert(
                    inode,
                    Node {
                        inode,
                        parent: Some(dir_inode),
                        kind: NodeKind::SyntheticSymlink {
                            target,
                            time: meta.time,
                        },
                    },
                );
            }

            if add_entry(DirEntry {
                ino: inode,
                offset: entry_offset + 1,
                type_: u32::from(libc::DT_LNK),
                name: name_bytes,
            })? == 0
            {
                return Ok(());
            }
        }

        Ok(())
    }

    fn readdir_commits(
        &self,
        mut offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        if offset == 0 {
            if add_entry(DirEntry {
                ino: INODE_COMMITS,
                offset: 1,
                type_: u32::from(libc::DT_DIR),
                name: NAME_DOT,
            })? == 0
            {
                return Ok(());
            }
            offset = 1;
        }

        if offset == 1 {
            if add_entry(DirEntry {
                ino: ROOT_ID,
                offset: 2,
                type_: u32::from(libc::DT_DIR),
                name: NAME_DOT_DOT,
            })? == 0
            {
                return Ok(());
            }
            offset = 2;
        }

        let mut commits = {
            let nodes = self.nodes.read();
            nodes
                .values()
                .filter_map(|node| {
                    if let NodeKind::Commit { meta } = &node.kind {
                        Some((meta.id.to_string().into_bytes(), node.inode))
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };

        commits.sort_by(|a, b| a.0.cmp(&b.0));

        for (index, (name, inode)) in commits.iter().enumerate() {
            let entry_offset = (index as u64) + 3;
            if offset > entry_offset {
                continue;
            }

            if add_entry(DirEntry {
                ino: *inode,
                offset: entry_offset + 1,
                type_: u32::from(libc::DT_DIR),
                name: name.as_slice(),
            })? == 0
            {
                return Ok(());
            }
        }

        Ok(())
    }

    fn materialize_tree_child(&self, parent: &Node, name: &[u8]) -> io::Result<(Node, stat64)> {
        let (meta, tree_id) = match &parent.kind {
            NodeKind::Commit { meta } => (meta.clone(), meta.tree),
            NodeKind::Tree { meta, tree } => (meta.clone(), *tree),
            NodeKind::Submodule { .. }
            | NodeKind::Blob { .. }
            | NodeKind::Symlink { .. }
            | NodeKind::SyntheticSymlink { .. } => {
                return Err(io::Error::from_raw_os_error(libc::ENOTDIR))
            }
        };

        let repo = self.repo.thread_local();
        let tree = repo.find_tree(tree_id).map_err(io::Error::other)?;

        let mut entry_info: Option<(EntryMode, ObjectId)> = None;
        for entry in tree.iter() {
            let entry = entry.map_err(io::Error::other)?;
            if entry.inner.filename.as_bytes() == name {
                entry_info = Some((entry.inner.mode, entry.inner.oid.to_owned()));
                break;
            }
        }

        let Some((mode, oid_raw)) = entry_info else {
            return Err(io::Error::from_raw_os_error(libc::ENOENT));
        };
        let child_oid: ObjectId = oid_raw;
        let child_inode = inode_from_oid(&child_oid);

        if let Some(existing) = self.nodes.read().get(&child_inode) {
            let attr = Self::attr_for_node(existing);
            return Ok((existing.clone(), attr));
        }

        let kind = mode.kind();

        let (node, attr) = match kind {
            EntryKind::Tree => {
                let node = Node {
                    inode: child_inode,
                    parent: Some(parent.inode),
                    kind: NodeKind::Tree {
                        meta: meta.clone(),
                        tree: child_oid,
                    },
                };
                let attr = build_dir_attr(child_inode, DIRECTORY_ATTR_MODE, meta.time);
                (node, attr)
            }
            EntryKind::Blob | EntryKind::BlobExecutable => {
                let blob = repo.find_blob(child_oid).map_err(io::Error::other)?;
                let executable = matches!(kind, EntryKind::BlobExecutable);
                let file_mode = if executable {
                    S_IFREG | 0o555
                } else {
                    S_IFREG | 0o444
                };
                let size = blob.data.len() as u64;
                let node = Node {
                    inode: child_inode,
                    parent: Some(parent.inode),
                    kind: NodeKind::Blob {
                        meta: meta.clone(),
                        oid: child_oid,
                        executable,
                        size,
                    },
                };
                let attr = build_file_attr(child_inode, file_mode, size, meta.time);
                (node, attr)
            }
            EntryKind::Link => {
                let blob = repo.find_blob(child_oid).map_err(io::Error::other)?;
                let target = blob.data.clone();
                let size = target.len() as u64;
                let node = Node {
                    inode: child_inode,
                    parent: Some(parent.inode),
                    kind: NodeKind::Symlink {
                        meta: meta.clone(),
                        target,
                    },
                };
                let attr = build_symlink_attr(child_inode, SYMLINK_ATTR_MODE, meta.time, size);
                (node, attr)
            }
            EntryKind::Commit => {
                let node = Node {
                    inode: child_inode,
                    parent: Some(parent.inode),
                    kind: NodeKind::Submodule {
                        meta: meta.clone(),
                        _oid: child_oid,
                    },
                };
                let attr = build_dir_attr(child_inode, DIRECTORY_ATTR_MODE, meta.time);
                (node, attr)
            }
        };

        self.nodes.write().insert(child_inode, node.clone());
        Ok((node, attr))
    }

    fn attr_for_node(node: &Node) -> stat64 {
        match &node.kind {
            NodeKind::Commit { meta }
            | NodeKind::Tree { meta, .. }
            | NodeKind::Submodule { meta, .. } => {
                build_dir_attr(node.inode, DIRECTORY_ATTR_MODE, meta.time)
            }
            NodeKind::Blob {
                meta,
                executable,
                size,
                ..
            } => {
                let mode = if *executable {
                    S_IFREG | 0o555
                } else {
                    S_IFREG | 0o444
                };
                build_file_attr(node.inode, mode, *size, meta.time)
            }
            NodeKind::Symlink { meta, target, .. } => build_symlink_attr(
                node.inode,
                SYMLINK_ATTR_MODE,
                meta.time,
                target.len() as u64,
            ),
            NodeKind::SyntheticSymlink { target, time } => {
                build_symlink_attr(node.inode, SYMLINK_ATTR_MODE, *time, target.len() as u64)
            }
        }
    }

    fn head_metadata(&self) -> io::Result<(Arc<CommitMeta>, Vec<u8>)> {
        let commit_id = self.repo.resolve_head().map_err(io::Error::other)?;
        let (meta, _) = self.ensure_commit_node(commit_id)?;
        let target = format!("../commits/{}", meta.id).into_bytes();
        Ok((meta, target))
    }

    fn readdir_node(
        &self,
        node: &Node,
        mut offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let parent_inode = node.parent.unwrap_or(ROOT_ID);

        if offset == 0 {
            if add_entry(DirEntry {
                ino: node.inode,
                offset: 1,
                type_: u32::from(libc::DT_DIR),
                name: NAME_DOT,
            })? == 0
            {
                return Ok(());
            }
            offset = 1;
        }

        if offset == 1 {
            if add_entry(DirEntry {
                ino: parent_inode,
                offset: 2,
                type_: u32::from(libc::DT_DIR),
                name: NAME_DOT_DOT,
            })? == 0
            {
                return Ok(());
            }
            offset = 2;
        }

        let tree_id = match &node.kind {
            NodeKind::Commit { meta } => meta.tree,
            NodeKind::Tree { tree, .. } => *tree,
            NodeKind::Submodule { .. } | NodeKind::SyntheticSymlink { .. } => return Ok(()),
            NodeKind::Blob { .. } | NodeKind::Symlink { .. } => {
                return Err(io::Error::from_raw_os_error(libc::ENOTDIR))
            }
        };

        let repo = self.repo.thread_local();
        let tree = repo.find_tree(tree_id).map_err(io::Error::other)?;

        for (index, entry) in tree.iter().enumerate() {
            let entry = entry.map_err(io::Error::other)?;
            let entry_offset = (index as u64) + 3;
            if offset > entry_offset {
                continue;
            }

            let filename = entry.inner.filename.as_bytes();
            let (child, _) = self.materialize_tree_child(node, filename)?;
            let entry_type = match entry.inner.mode.kind() {
                EntryKind::Tree => libc::DT_DIR,
                EntryKind::Blob | EntryKind::BlobExecutable => libc::DT_REG,
                EntryKind::Link => libc::DT_LNK,
                EntryKind::Commit => libc::DT_DIR,
            };

            if add_entry(DirEntry {
                ino: child.inode,
                offset: entry_offset + 1,
                type_: u32::from(entry_type),
                name: filename,
            })? == 0
            {
                return Ok(());
            }
        }

        drop(tree);

        Ok(())
    }
}

impl FileSystem for GitSnapFs {
    type Inode = u64;
    type Handle = u64;

    fn lookup(
        &self,
        _ctx: &Context,
        parent: Self::Inode,
        name: &std::ffi::CStr,
    ) -> io::Result<Entry> {
        let name = name.to_bytes();
        if parent == ROOT_ID {
            return match name {
                b"commits" => Ok(self.special_dir_entry(INODE_COMMITS)),
                b"branches" => Ok(self.special_dir_entry(INODE_BRANCHES)),
                b"tags" => Ok(self.special_dir_entry(INODE_TAGS)),
                b"HEAD" => self.head_entry(),
                _ => Err(io::Error::from_raw_os_error(libc::ENOENT)),
            };
        }

        if parent == INODE_COMMITS {
            return self.lookup_commit(name);
        }

        if parent == INODE_BRANCHES {
            let refs = self.repo.list_branches().map_err(io::Error::other)?;
            return self.lookup_reference_symlink(name, NAMESPACE_BRANCH, refs, INODE_BRANCHES);
        }

        if parent == INODE_TAGS {
            let refs = self.repo.list_tags().map_err(io::Error::other)?;
            return self.lookup_reference_symlink(name, NAMESPACE_TAG, refs, INODE_TAGS);
        }

        let parent_node = self.node_for_inode(parent)?;
        let (node, attr) = self.materialize_tree_child(&parent_node, name)?;
        Ok(Self::make_entry(node.inode, attr))
    }

    fn getattr(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _handle: Option<Self::Handle>,
    ) -> io::Result<(stat64, Duration)> {
        let attr = match inode {
            ROOT_ID => self.root_attr(),
            INODE_COMMITS | INODE_BRANCHES | INODE_TAGS => {
                build_dir_attr(inode, DIRECTORY_ATTR_MODE, self.start_time)
            }
            INODE_HEAD => {
                let (meta, target) = self.head_metadata()?;
                build_symlink_attr(inode, SYMLINK_ATTR_MODE, meta.time, target.len() as u64)
            }
            _ => {
                let node = self.node_for_inode(inode)?;
                Self::attr_for_node(&node)
            }
        };
        Ok((attr, ATTR_TTL))
    }

    fn setattr(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _attr: stat64,
        _handle: Option<Self::Handle>,
        _valid: SetattrValid,
    ) -> io::Result<(stat64, Duration)> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    fn symlink(
        &self,
        _ctx: &Context,
        _linkname: &CStr,
        _parent: Self::Inode,
        _name: &CStr,
    ) -> io::Result<Entry> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    fn mknod(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _name: &CStr,
        _mode: u32,
        _rdev: u32,
        _umask: u32,
    ) -> io::Result<Entry> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    fn mkdir(
        &self,
        _ctx: &Context,
        _parent: Self::Inode,
        _name: &CStr,
        _mode: u32,
        _umask: u32,
    ) -> io::Result<Entry> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    fn unlink(&self, _ctx: &Context, _parent: Self::Inode, _name: &CStr) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    fn rmdir(&self, _ctx: &Context, _parent: Self::Inode, _name: &CStr) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    fn rename(
        &self,
        _ctx: &Context,
        _olddir: Self::Inode,
        _oldname: &CStr,
        _newdir: Self::Inode,
        _newname: &CStr,
        _flags: u32,
    ) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    fn link(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _newparent: Self::Inode,
        _newname: &CStr,
    ) -> io::Result<Entry> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    fn create(
        &self,
        _ctx: &Context,
        _parent: Self::Inode,
        _name: &CStr,
        _args: CreateIn,
    ) -> io::Result<(Entry, Option<Self::Handle>, OpenOptions, Option<u32>)> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    fn readdir(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _handle: Self::Handle,
        _size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
    ) -> io::Result<()> {
        if inode == ROOT_ID {
            return readdir_root(offset, add_entry);
        }

        if inode == INODE_COMMITS {
            return self.readdir_commits(offset, add_entry);
        }

        if inode == INODE_BRANCHES {
            let refs = self.repo.list_branches().map_err(io::Error::other)?;
            return self.readdir_refs(inode, offset, add_entry, refs, NAMESPACE_BRANCH);
        }

        if inode == INODE_TAGS {
            let refs = self.repo.list_tags().map_err(io::Error::other)?;
            return self.readdir_refs(inode, offset, add_entry, refs, NAMESPACE_TAG);
        }

        let node = self.node_for_inode(inode)?;
        self.readdir_node(&node, offset, add_entry)
    }

    fn readlink(&self, _ctx: &Context, inode: Self::Inode) -> io::Result<Vec<u8>> {
        if inode == INODE_HEAD {
            let (_, target) = self.head_metadata()?;
            return Ok(target);
        }
        let node = self.node_for_inode(inode)?;
        match node.kind {
            NodeKind::Symlink { target, .. } => Ok(target),
            NodeKind::Submodule { .. } => Err(io::Error::from_raw_os_error(libc::EINVAL)),
            NodeKind::SyntheticSymlink { target, .. } => Ok(target),
            _ => Err(io::Error::from_raw_os_error(libc::EINVAL)),
        }
    }

    fn open(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions, Option<u32>)> {
        let flag_bits =
            i32::try_from(flags).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let access_mode = flag_bits & libc::O_ACCMODE;
        if access_mode != libc::O_RDONLY {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }

        let inode_u64 = inode;
        match inode_u64 {
            ROOT_ID | INODE_COMMITS | INODE_BRANCHES | INODE_TAGS | INODE_HEAD => {}
            _ => {
                // Verify the inode exists for regular Git-backed objects.
                let _ = self.node_for_inode(inode_u64)?;
            }
        }

        Ok((Some(inode_u64), OpenOptions::empty(), None))
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _handle: Self::Handle,
        w: &mut dyn ZeroCopyWriter,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        let node = self.node_for_inode(inode)?;
        let blob_oid = match node.kind {
            NodeKind::Blob { ref oid, .. } => *oid,
            _ => return Err(io::Error::from_raw_os_error(libc::EINVAL)),
        };

        let repo = self.repo.thread_local();
        let blob = repo.find_blob(blob_oid).map_err(io::Error::other)?;
        let data = blob.data.as_slice();
        let offset = offset as usize;
        if offset >= data.len() {
            return Ok(0);
        }
        let end = offset.saturating_add(size as usize).min(data.len());
        w.write_all(&data[offset..end])?;
        Ok(end - offset)
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _handle: Self::Handle,
        _r: &mut dyn ZeroCopyReader,
        _size: u32,
        _offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        _flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<usize> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    fn fallocate(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _handle: Self::Handle,
        _mode: u32,
        _offset: u64,
        _length: u64,
    ) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EROFS))
    }

    fn access(&self, _ctx: &Context, _inode: Self::Inode, mask: u32) -> io::Result<()> {
        let mask_bits =
            i32::try_from(mask).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        if (mask_bits & libc::W_OK) != 0 {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }
        Ok(())
    }
}

fn readdir_root(
    mut offset: u64,
    add_entry: &mut dyn FnMut(DirEntry) -> io::Result<usize>,
) -> io::Result<()> {
    if offset == 0 {
        if add_entry(DirEntry {
            ino: ROOT_ID,
            offset: 1,
            type_: u32::from(libc::DT_DIR),
            name: NAME_DOT,
        })? == 0
        {
            return Ok(());
        }
        offset = 1;
    }
    if offset == 1 {
        if add_entry(DirEntry {
            ino: ROOT_ID,
            offset: 2,
            type_: u32::from(libc::DT_DIR),
            name: NAME_DOT_DOT,
        })? == 0
        {
            return Ok(());
        }
        offset = 2;
    }

    let synthetic_entries: &[(u64, u32, &[u8])] = &[
        (INODE_COMMITS, u32::from(libc::DT_DIR), NAME_COMMITS),
        (INODE_BRANCHES, u32::from(libc::DT_DIR), NAME_BRANCHES),
        (INODE_TAGS, u32::from(libc::DT_DIR), NAME_TAGS),
        (INODE_HEAD, u32::from(libc::DT_LNK), NAME_HEAD),
    ];

    for (index, (ino, ty, name)) in synthetic_entries.iter().enumerate() {
        let entry_offset = (index as u64) + 3;
        if offset > entry_offset {
            continue;
        }
        if add_entry(DirEntry {
            ino: *ino,
            offset: entry_offset + 1,
            type_: *ty,
            name,
        })? == 0
        {
            return Ok(());
        }
    }

    Ok(())
}

fn build_dir_attr(inode: u64, mode: u32, time: SystemTime) -> stat64 {
    let (secs, nsecs) = time_to_unix_parts(time);
    let mut attr: stat64 = unsafe { std::mem::zeroed() };
    attr.st_ino = inode;
    attr.st_mode = mode;
    attr.st_nlink = 2;
    attr.st_uid = 0;
    attr.st_gid = 0;
    attr.st_blksize = 4096;
    attr.st_blocks = 0;
    attr.st_size = 0;
    attr.st_atime = secs;
    attr.st_atime_nsec = nsecs;
    attr.st_mtime = secs;
    attr.st_mtime_nsec = nsecs;
    attr.st_ctime = secs;
    attr.st_ctime_nsec = nsecs;
    attr
}

fn build_file_attr(inode: u64, mode: u32, size: u64, time: SystemTime) -> stat64 {
    let (secs, nsecs) = time_to_unix_parts(time);
    let mut attr: stat64 = unsafe { std::mem::zeroed() };
    attr.st_ino = inode;
    attr.st_mode = mode;
    attr.st_nlink = 1;
    attr.st_uid = 0;
    attr.st_gid = 0;
    attr.st_blksize = 4096;
    attr.st_blocks = 0;
    attr.st_size = size as i64;
    attr.st_atime = secs;
    attr.st_atime_nsec = nsecs;
    attr.st_mtime = secs;
    attr.st_mtime_nsec = nsecs;
    attr.st_ctime = secs;
    attr.st_ctime_nsec = nsecs;
    attr
}

fn build_symlink_attr(inode: u64, mode: u32, time: SystemTime, size: u64) -> stat64 {
    let (secs, nsecs) = time_to_unix_parts(time);
    let mut attr: stat64 = unsafe { std::mem::zeroed() };
    attr.st_ino = inode;
    attr.st_mode = mode;
    attr.st_nlink = 1;
    attr.st_uid = 0;
    attr.st_gid = 0;
    attr.st_blksize = 4096;
    attr.st_blocks = 0;
    attr.st_size = size as i64;
    attr.st_atime = secs;
    attr.st_atime_nsec = nsecs;
    attr.st_mtime = secs;
    attr.st_mtime_nsec = nsecs;
    attr.st_ctime = secs;
    attr.st_ctime_nsec = nsecs;
    attr
}

fn time_to_unix_parts(time: SystemTime) -> (i64, i64) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs() as i64, duration.subsec_nanos() as i64),
        Err(err) => {
            let duration = err.duration();
            (-(duration.as_secs() as i64), duration.subsec_nanos() as i64)
        }
    }
}

fn commit_time_to_system(commit: &gix::Commit<'_>, default: SystemTime) -> SystemTime {
    match commit.committer() {
        Ok(signature) => match signature.time() {
            Ok(time) => seconds_to_system_time(time.seconds),
            Err(_) => default,
        },
        Err(_) => default,
    }
}

fn seconds_to_system_time(seconds: i64) -> SystemTime {
    let duration = Duration::from_secs(seconds.unsigned_abs());
    if seconds >= 0 {
        UNIX_EPOCH + duration
    } else {
        UNIX_EPOCH - duration
    }
}
