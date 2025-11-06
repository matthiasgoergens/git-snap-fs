//! FUSE filesystem implementation for `GitSnapFS`.

use std::convert::TryFrom;
use std::ffi::CStr;
use std::io;
use std::str;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuse_backend_rs::abi::fuse_abi::{stat64, Attr, CreateIn, ROOT_ID};
use fuse_backend_rs::api::filesystem::{
    Context, DirEntry, Entry, FileSystem, FsOptions, OpenOptions, SetattrValid, ZeroCopyReader,
    ZeroCopyWriter,
};
use gix::bstr::ByteSlice;
use gix::object::tree::{EntryKind, EntryMode};
use gix::object::Kind;
use gix::ObjectId;
use libc::{S_IFDIR, S_IFLNK, S_IFREG};

use crate::inode::inode_from_oid;
use crate::repo::Repository;

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

struct DirRecord {
    name: Vec<u8>,
    ino: u64,
    dtype: u32,
    entry: Option<Entry>,
}

#[derive(Copy, Clone)]
enum RefNamespace {
    Branches,
    Tags,
}

impl RefNamespace {
    fn marker(self) -> u8 {
        match self {
            RefNamespace::Branches => NAMESPACE_BRANCH,
            RefNamespace::Tags => NAMESPACE_TAG,
        }
    }

    fn list(self, repo: &Repository) -> io::Result<Vec<(String, ObjectId)>> {
        match self {
            RefNamespace::Branches => repo.list_branches(),
            RefNamespace::Tags => repo.list_tags(),
        }
        .map_err(io::Error::other)
    }
}

pub struct GitSnapFs {
    repo: Repository,
    // TODO: instead of running time_to_unix_parts etc every time we need to build an attr, we can just do it once at the beginning, and store the result here, instead of storing as a SystemTime.
    mount_time: SystemTime,
}

impl GitSnapFs {
    pub fn new(repo: Repository) -> Self {
        Self {
            repo,
            mount_time: SystemTime::now(),
        }
    }

    fn root_attr(&self) -> stat64 {
        build_dir_attr(ROOT_ID, ROOT_ATTR_MODE, self.mount_time)
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

    fn synthetic_dir_entry(&self, inode: u64) -> Entry {
        Self::make_entry(
            inode,
            build_dir_attr(inode, DIRECTORY_ATTR_MODE, self.mount_time),
        )
    }

    fn lookup_commit(&self, name: &[u8]) -> io::Result<Entry> {
        let name_str =
            str::from_utf8(name).map_err(|_| io::Error::from_raw_os_error(libc::ENOENT))?;
        let commit_id = self
            .repo
            .resolve_full_commit_id(name_str)
            .map_err(io::Error::other)?;
        let inode = inode_from_oid(&commit_id);
        Ok(Self::make_entry(
            inode,
            build_dir_attr(inode, DIRECTORY_ATTR_MODE, self.mount_time),
        ))
    }

    fn lookup_reference(&self, name: &[u8], ns: RefNamespace) -> io::Result<Entry> {
        let name_str =
            str::from_utf8(name).map_err(|_| io::Error::from_raw_os_error(libc::ENOENT))?;
        let refs = ns.list(&self.repo)?;
        let object_id = refs
            .into_iter()
            .find(|(ref_name, _)| ref_name == name_str)
            .map(|(_, id)| id)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        let (_, _, entry) = self.reference_entry_details(ns, name, object_id)?;
        Ok(entry)
    }

    fn head_entry(&self) -> io::Result<Entry> {
        let target = self.head_target()?;
        Ok(Self::make_entry(
            INODE_HEAD,
            build_symlink_attr(
                INODE_HEAD,
                SYMLINK_ATTR_MODE,
                self.mount_time,
                target.len() as u64,
            ),
        ))
    }

    fn head_target(&self) -> io::Result<Vec<u8>> {
        let commit_id = self.repo.resolve_head().map_err(io::Error::other)?;
        Ok(format!("commits/{commit_id}").into_bytes())
    }

    fn tree_root_id(&self, inode: u64) -> io::Result<ObjectId> {
        let oid = self.repo.resolve_inode(inode).map_err(io::Error::other)?;
        let repo = self.repo.thread_local();
        let object = repo.find_object(oid).map_err(io::Error::other)?;
        match object.kind {
            gix::object::Kind::Commit => {
                let commit = repo.find_commit(oid).map_err(io::Error::other)?;
                let tree_id = commit.tree_id().map_err(io::Error::other)?.detach();
                Ok(tree_id)
            }
            gix::object::Kind::Tree => Ok(oid),
            _ => Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
        }
    }

    fn entry_for_tree_child(&self, mode: EntryMode, oid: ObjectId) -> io::Result<(Entry, u32)> {
        let inode = inode_from_oid(&oid);
        let kind = mode.kind();
        let entry = match kind {
            EntryKind::Tree | EntryKind::Commit => Self::make_entry(
                inode,
                build_dir_attr(inode, DIRECTORY_ATTR_MODE, self.mount_time),
            ),
            EntryKind::Blob => {
                let repo = self.repo.thread_local();
                let blob = repo.find_blob(oid).map_err(io::Error::other)?;
                Self::make_entry(
                    inode,
                    build_file_attr(
                        inode,
                        S_IFREG | 0o444,
                        blob.data.len() as u64,
                        self.mount_time,
                    ),
                )
            }
            EntryKind::BlobExecutable => {
                let repo = self.repo.thread_local();
                let blob = repo.find_blob(oid).map_err(io::Error::other)?;
                Self::make_entry(
                    inode,
                    build_file_attr(
                        inode,
                        S_IFREG | 0o555,
                        blob.data.len() as u64,
                        self.mount_time,
                    ),
                )
            }
            EntryKind::Link => {
                let repo = self.repo.thread_local();
                let blob = repo.find_blob(oid).map_err(io::Error::other)?;
                Self::make_entry(
                    inode,
                    build_symlink_attr(
                        inode,
                        SYMLINK_ATTR_MODE,
                        self.mount_time,
                        blob.data.len() as u64,
                    ),
                )
            }
        };
        let dtype = match kind {
            EntryKind::Tree | EntryKind::Commit => libc::DT_DIR,
            EntryKind::Blob | EntryKind::BlobExecutable => libc::DT_REG,
            EntryKind::Link => libc::DT_LNK,
        };
        Ok((entry, u32::from(dtype)))
    }

    fn list_root(&self) -> io::Result<Vec<DirRecord>> {
        let head_entry = self.head_entry()?;
        Ok(vec![
            DirRecord {
                name: b"commits".to_vec(),
                ino: INODE_COMMITS,
                dtype: u32::from(libc::DT_DIR),
                entry: Some(self.synthetic_dir_entry(INODE_COMMITS)),
            },
            DirRecord {
                name: b"branches".to_vec(),
                ino: INODE_BRANCHES,
                dtype: u32::from(libc::DT_DIR),
                entry: Some(self.synthetic_dir_entry(INODE_BRANCHES)),
            },
            DirRecord {
                name: b"tags".to_vec(),
                ino: INODE_TAGS,
                dtype: u32::from(libc::DT_DIR),
                entry: Some(self.synthetic_dir_entry(INODE_TAGS)),
            },
            DirRecord {
                name: b"HEAD".to_vec(),
                ino: INODE_HEAD,
                dtype: u32::from(libc::DT_LNK),
                entry: Some(head_entry),
            },
        ])
    }

    fn list_refs_dir(&self, ns: RefNamespace) -> io::Result<Vec<DirRecord>> {
        let refs = ns.list(&self.repo)?;
        refs.into_iter()
            .map(|(name, object_id)| {
                let (inode, dtype, entry) =
                    self.reference_entry_details(ns, name.as_bytes(), object_id)?;
                Ok(DirRecord {
                    name: name.into_bytes(),
                    ino: inode,
                    dtype,
                    entry: Some(entry),
                })
            })
            .collect()
    }

    fn list_tree_dir(&self, inode: u64) -> io::Result<Vec<DirRecord>> {
        let tree_id = self.tree_root_id(inode)?;
        let repo = self.repo.thread_local();
        let tree = repo.find_tree(tree_id).map_err(io::Error::other)?;
        let records = tree
            .iter()
            .map(|entry| {
                let entry = entry.map_err(io::Error::other)?;
                let oid = entry.inner.oid.to_owned();
                let (child_entry, dtype) = self.entry_for_tree_child(entry.inner.mode, oid)?;
                Ok(DirRecord {
                    name: entry.inner.filename.as_bstr().to_vec(),
                    ino: child_entry.inode,
                    dtype,
                    entry: Some(child_entry),
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(records)
    }

    fn list_directory(&self, inode: u64) -> io::Result<Vec<DirRecord>> {
        match inode {
            ROOT_ID => self.list_root(),
            INODE_COMMITS => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "enumerating the commits directory is not supported",
            )),
            INODE_BRANCHES => self.list_refs_dir(RefNamespace::Branches),
            INODE_TAGS => self.list_refs_dir(RefNamespace::Tags),
            _ => self.list_tree_dir(inode),
        }
    }

    fn lookup_child(&self, parent: u64, name: &[u8]) -> io::Result<Entry> {
        let tree_id = self.tree_root_id(parent)?;
        let repo = self.repo.thread_local();
        let tree = repo.find_tree(tree_id).map_err(io::Error::other)?;
        for entry in tree.iter() {
            let entry = entry.map_err(io::Error::other)?;
            if entry.inner.filename.as_bytes() == name {
                let oid = entry.inner.oid.to_owned();
                let (child_entry, _) = self.entry_for_tree_child(entry.inner.mode, oid)?;
                return Ok(child_entry);
            }
        }
        Err(io::Error::from_raw_os_error(libc::ENOENT))
    }

    fn reference_entry_details(
        &self,
        ns: RefNamespace,
        name: &[u8],
        object_id: ObjectId,
    ) -> io::Result<(u64, u32, Entry)> {
        let repo = self.repo.thread_local();
        let object = repo.find_object(object_id).map_err(io::Error::other)?;
        match object.kind {
            Kind::Commit => {
                let inode = synthetic_inode(ns.marker(), name);
                let target = format!("../commits/{object_id}");
                let entry = Self::make_entry(
                    inode,
                    build_symlink_attr(
                        inode,
                        SYMLINK_ATTR_MODE,
                        self.mount_time,
                        target.len() as u64,
                    ),
                );
                Ok((inode, u32::from(libc::DT_LNK), entry))
            }
            Kind::Tree => {
                let inode = inode_from_oid(&object_id);
                let entry = Self::make_entry(
                    inode,
                    build_dir_attr(inode, DIRECTORY_ATTR_MODE, self.mount_time),
                );
                Ok((inode, u32::from(libc::DT_DIR), entry))
            }
            Kind::Blob => {
                let inode = inode_from_oid(&object_id);
                let blob = repo.find_blob(object_id).map_err(io::Error::other)?;
                let entry = Self::make_entry(
                    inode,
                    build_file_attr(
                        inode,
                        S_IFREG | 0o444,
                        blob.data.len() as u64,
                        self.mount_time,
                    ),
                );
                Ok((inode, u32::from(libc::DT_REG), entry))
            }
            Kind::Tag => Err(io::Error::other(
                "tag reference resolves to another tag, which is unsupported",
            )),
        }
    }

    fn reference_target(&self, inode: u64, ns: RefNamespace) -> io::Result<Vec<u8>> {
        let refs = ns.list(&self.repo)?;
        for (name, commit_id) in refs {
            let candidate = synthetic_inode(ns.marker(), name.as_bytes());
            if candidate == inode {
                return Ok(format!("../commits/{commit_id}").into_bytes());
            }
        }
        Err(io::Error::from_raw_os_error(libc::ENOENT))
    }

    fn attr_for_inode(&self, inode: u64) -> io::Result<stat64> {
        if inode == ROOT_ID {
            return Ok(self.root_attr());
        }
        if inode == INODE_COMMITS || inode == INODE_BRANCHES || inode == INODE_TAGS {
            return Ok(build_dir_attr(inode, DIRECTORY_ATTR_MODE, self.mount_time));
        }
        if inode == INODE_HEAD {
            let target = self.head_target()?;
            return Ok(build_symlink_attr(
                INODE_HEAD,
                SYMLINK_ATTR_MODE,
                self.mount_time,
                target.len() as u64,
            ));
        }
        if let Ok(target) = self.reference_target(inode, RefNamespace::Branches) {
            return Ok(build_symlink_attr(
                inode,
                SYMLINK_ATTR_MODE,
                self.mount_time,
                target.len() as u64,
            ));
        }
        if let Ok(target) = self.reference_target(inode, RefNamespace::Tags) {
            return Ok(build_symlink_attr(
                inode,
                SYMLINK_ATTR_MODE,
                self.mount_time,
                target.len() as u64,
            ));
        }

        let oid = self.repo.resolve_inode(inode).map_err(io::Error::other)?;
        let repo = self.repo.thread_local();
        let object = repo.find_object(oid).map_err(io::Error::other)?;
        match object.kind {
            Kind::Commit | Kind::Tree => {
                Ok(build_dir_attr(inode, DIRECTORY_ATTR_MODE, self.mount_time))
            }
            Kind::Blob => {
                let blob = repo.find_blob(oid).map_err(io::Error::other)?;
                Ok(build_file_attr(
                    inode,
                    S_IFREG | 0o444,
                    blob.data.len() as u64,
                    self.mount_time,
                ))
            }
            Kind::Tag => Ok(build_file_attr(
                inode,
                S_IFREG | 0o444,
                object.data.len() as u64,
                self.mount_time,
            )),
        }
    }
}

impl FileSystem for GitSnapFs {
    type Inode = u64;
    type Handle = u64;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        let required = FsOptions::EXPORT_SUPPORT
            | FsOptions::ZERO_MESSAGE_OPEN
            | FsOptions::ZERO_MESSAGE_OPENDIR;
        let optional = FsOptions::ASYNC_READ
            | FsOptions::DO_READDIRPLUS
            | FsOptions::READDIRPLUS_AUTO
            | FsOptions::PARALLEL_DIROPS
            | FsOptions::CACHE_SYMLINKS;
        let wanted = required | optional;
        let supported = capable & wanted;
        if !supported.contains(required) {
            return Err(io::Error::other(
                "kernel does not advertise required export support or zero-message open capabilities"
            ));
        }
        Ok(supported)
    }

    fn lookup(&self, _ctx: &Context, parent: Self::Inode, name: &CStr) -> io::Result<Entry> {
        let name = name.to_bytes();
        match parent {
            inode if inode == ROOT_ID => match name {
                b"commits" => Ok(self.synthetic_dir_entry(INODE_COMMITS)),
                b"branches" => Ok(self.synthetic_dir_entry(INODE_BRANCHES)),
                b"tags" => Ok(self.synthetic_dir_entry(INODE_TAGS)),
                b"HEAD" => self.head_entry(),
                _ => Err(io::Error::from_raw_os_error(libc::ENOENT)),
            },
            inode if inode == INODE_COMMITS => self.lookup_commit(name),
            inode if inode == INODE_BRANCHES => self.lookup_reference(name, RefNamespace::Branches),
            inode if inode == INODE_TAGS => self.lookup_reference(name, RefNamespace::Tags),
            other => self.lookup_child(other, name),
        }
    }

    fn getattr(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _handle: Option<Self::Handle>,
    ) -> io::Result<(stat64, Duration)> {
        let attr = self.attr_for_inode(inode)?;
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

    fn readlink(&self, _ctx: &Context, inode: Self::Inode) -> io::Result<Vec<u8>> {
        if inode == INODE_HEAD {
            return self.head_target();
        }
        if let Ok(target) = self.reference_target(inode, RefNamespace::Branches) {
            return Ok(target);
        }
        if let Ok(target) = self.reference_target(inode, RefNamespace::Tags) {
            return Ok(target);
        }

        let oid = self.repo.resolve_inode(inode).map_err(io::Error::other)?;
        let repo = self.repo.thread_local();
        let blob = repo.find_blob(oid).map_err(io::Error::other)?;
        Ok(blob.data.as_slice().to_vec())
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
        let records = self.list_directory(inode)?;
        let start =
            usize::try_from(offset).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        for (index, record) in records.into_iter().enumerate().skip(start) {
            let entry_offset = index as u64;
            let dirent = DirEntry {
                ino: record.ino,
                offset: entry_offset + 1,
                type_: record.dtype,
                name: &record.name,
            };
            if add_entry(dirent)? == 0 {
                break;
            }
        }
        Ok(())
    }

    fn readdirplus(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _handle: Self::Handle,
        _size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry, Entry) -> io::Result<usize>,
    ) -> io::Result<()> {
        let records = self.list_directory(inode)?;
        let start =
            usize::try_from(offset).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        for (index, record) in records.into_iter().enumerate().skip(start) {
            let entry_offset = index as u64;
            if let Some(entry) = record.entry {
                let dirent = DirEntry {
                    ino: record.ino,
                    offset: entry_offset + 1,
                    type_: record.dtype,
                    name: &record.name,
                };
                if add_entry(dirent, entry)? == 0 {
                    break;
                }
            }
        }
        Ok(())
    }

    fn opendir(
        &self,
        _ctx: &Context,
        _inode: Self::Inode,
        _flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions)> {
        Err(io::Error::from_raw_os_error(libc::ENOSYS))
    }

    fn open(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        flags: u32,
        _fuse_flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions, Option<u32>)> {
        let _ = (inode, flags);
        Err(io::Error::from_raw_os_error(libc::ENOSYS))
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
        let oid = self.repo.resolve_inode(inode).map_err(io::Error::other)?;
        let repo = self.repo.thread_local();
        let blob = repo.find_blob(oid).map_err(io::Error::other)?;
        let data = blob.data.as_slice();
        let start =
            usize::try_from(offset).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        if start >= data.len() {
            return Ok(0);
        }
        let span = usize::try_from(size).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
        let end = start.saturating_add(span).min(data.len());
        w.write_all(&data[start..end])?;
        Ok(end - start)
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

fn synthetic_inode(namespace: u8, name: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    namespace.hash(&mut hasher);
    name.hash(&mut hasher);
    let hash = hasher.finish();
    (u64::from(namespace) << 56) | (hash & 0x00FF_FFFF_FFFF_FFFF)
}

fn build_attr(inode: u64, mode: u32, nlink: u32, size: i64, time: SystemTime) -> stat64 {
    let (secs, nsecs) = time_to_unix_parts(time);
    let attr = Attr {
        ino: inode,
        size: u64::try_from(size).unwrap_or(u64::MAX),
        blocks: 0,
        atime: u64::try_from(secs).unwrap_or_default(),
        mtime: u64::try_from(secs).unwrap_or_default(),
        ctime: u64::try_from(secs).unwrap_or_default(),
        atimensec: u32::try_from(nsecs).unwrap_or_default(),
        mtimensec: u32::try_from(nsecs).unwrap_or_default(),
        ctimensec: u32::try_from(nsecs).unwrap_or_default(),
        mode,
        nlink,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
        flags: 0,
    };
    attr.into()
}

fn build_dir_attr(inode: u64, mode: u32, time: SystemTime) -> stat64 {
    build_attr(inode, mode, 2, 0, time)
}

// TODO: unify file and symlink attr builders.  They are virtually identical.
fn build_file_attr(inode: u64, mode: u32, size: u64, time: SystemTime) -> stat64 {
    build_attr(inode, mode, 1, saturating_i64_from_u64(size), time)
}

fn build_symlink_attr(inode: u64, mode: u32, time: SystemTime, size: u64) -> stat64 {
    build_attr(inode, mode, 1, saturating_i64_from_u64(size), time)
}

fn time_to_unix_parts(time: SystemTime) -> (i64, i64) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (
            saturating_i64_from_u64(duration.as_secs()),
            i64::from(duration.subsec_nanos()),
        ),
        Err(err) => {
            let duration = err.duration();
            (
                -saturating_i64_from_u64(duration.as_secs()),
                i64::from(duration.subsec_nanos()),
            )
        }
    }
}

fn saturating_i64_from_u64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
