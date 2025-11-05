//! FUSE filesystem implementation for GitSnapFS.
//!
//! The full server logic is still under construction. This module currently
//! provides a thin placeholder that keeps the project compiling while the
//! repository access layer and operation handlers are fleshed out.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuse_backend_rs::abi::fuse_abi::{stat64, ROOT_ID};
use fuse_backend_rs::api::filesystem::{Context, DirEntry, Entry, FileSystem};
use libc::{S_IFDIR, S_IFLNK};

use crate::repo::Repository;

const ROOT_ATTR_MODE: u32 = S_IFDIR | 0o755;
const DIRECTORY_ATTR_MODE: u32 = S_IFDIR | 0o755;
const SYMLINK_ATTR_MODE: u32 = S_IFLNK | 0o777;

const INODE_COMMITS: u64 = 2;
const INODE_BRANCHES: u64 = 3;
const INODE_TAGS: u64 = 4;
const INODE_HEAD: u64 = 5;

const ENTRY_TTL: Duration = Duration::from_secs(1);
const ATTR_TTL: Duration = Duration::from_secs(1);

const NAME_DOT: &[u8] = b".";
const NAME_DOT_DOT: &[u8] = b"..";
const NAME_COMMITS: &[u8] = b"commits";
const NAME_BRANCHES: &[u8] = b"branches";
const NAME_TAGS: &[u8] = b"tags";
const NAME_HEAD: &[u8] = b"HEAD";

pub struct GitSnapFs {
    #[allow(dead_code)]
    repo: Arc<Repository>,
    start_time: SystemTime,
}

impl GitSnapFs {
    pub fn new(repo: Repository) -> Self {
        Self {
            repo: Arc::new(repo),
            start_time: SystemTime::now(),
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

    fn head_entry(&self) -> Entry {
        Entry {
            inode: INODE_HEAD,
            generation: 0,
            attr: build_symlink_attr(INODE_HEAD, SYMLINK_ATTR_MODE, self.start_time),
            attr_flags: 0,
            attr_timeout: ATTR_TTL,
            entry_timeout: ENTRY_TTL,
        }
    }
}

impl FileSystem for GitSnapFs {
    type Inode = u64;
    type Handle = u64;

    fn lookup(&self, _ctx: &Context, parent: Self::Inode, name: &std::ffi::CStr) -> std::io::Result<Entry> {
        let name = name.to_bytes();
        if parent == ROOT_ID {
            match name {
                b"commits" => return Ok(self.special_dir_entry(INODE_COMMITS)),
                b"branches" => return Ok(self.special_dir_entry(INODE_BRANCHES)),
                b"tags" => return Ok(self.special_dir_entry(INODE_TAGS)),
                b"HEAD" => return Ok(self.head_entry()),
                _ => {}
            }
        }
        Err(std::io::Error::from_raw_os_error(libc::ENOENT))
    }

    fn getattr(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _handle: Option<Self::Handle>,
    ) -> std::io::Result<(stat64, Duration)> {
        let attr = match inode {
            ROOT_ID => self.root_attr(),
            INODE_COMMITS | INODE_BRANCHES | INODE_TAGS => {
                build_dir_attr(inode, DIRECTORY_ATTR_MODE, self.start_time)
            }
            INODE_HEAD => build_symlink_attr(inode, SYMLINK_ATTR_MODE, self.start_time),
            _ => return Err(std::io::Error::from_raw_os_error(libc::ENOENT)),
        };
        Ok((attr, ATTR_TTL))
    }

    fn readdir(
        &self,
        _ctx: &Context,
        inode: Self::Inode,
        _handle: Self::Handle,
        _size: u32,
        offset: u64,
        add_entry: &mut dyn FnMut(DirEntry) -> std::io::Result<usize>,
    ) -> std::io::Result<()> {
        if inode != ROOT_ID {
            // TODO: populate other directories in subsequent iterations.
            return Err(std::io::Error::from_raw_os_error(libc::ENOSYS));
        }

        let mut cursor = offset;
        if cursor == 0 {
            if add_entry(DirEntry {
                ino: ROOT_ID,
                offset: 1,
                type_: libc::DT_DIR as u32,
                name: NAME_DOT,
            })? == 0
            {
                return Ok(());
            }
            cursor = 1;
        }
        if cursor == 1 {
            if add_entry(DirEntry {
                ino: ROOT_ID,
                offset: 2,
                type_: libc::DT_DIR as u32,
                name: NAME_DOT_DOT,
            })? == 0
            {
                return Ok(());
            }
            cursor = 2;
        }

        let synthetic_entries: &[(u64, u32, &[u8])] = &[
            (INODE_COMMITS, libc::DT_DIR as u32, NAME_COMMITS),
            (INODE_BRANCHES, libc::DT_DIR as u32, NAME_BRANCHES),
            (INODE_TAGS, libc::DT_DIR as u32, NAME_TAGS),
            (INODE_HEAD, libc::DT_LNK as u32, NAME_HEAD),
        ];

        for (index, (ino, ty, name)) in synthetic_entries.iter().enumerate() {
            let entry_offset = (index as u64) + 3;
            if cursor > entry_offset {
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
}

fn build_dir_attr(inode: u64, mode: u32, start_time: SystemTime) -> stat64 {
    let (secs, nsecs) = time_to_unix_parts(start_time);
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

fn build_symlink_attr(inode: u64, mode: u32, start_time: SystemTime) -> stat64 {
    let (secs, nsecs) = time_to_unix_parts(start_time);
    let mut attr: stat64 = unsafe { std::mem::zeroed() };
    attr.st_ino = inode;
    attr.st_mode = mode;
    attr.st_nlink = 1;
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

fn time_to_unix_parts(time: SystemTime) -> (i64, i64) {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs() as i64, duration.subsec_nanos() as i64),
        Err(err) => {
            let duration = err.duration();
            (-(duration.as_secs() as i64), duration.subsec_nanos() as i64)
        }
    }
}
