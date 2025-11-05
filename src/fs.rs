//! FUSE filesystem implementation for GitSnapFS.
//!
//! The full server logic will be filled in subsequent iterations; for now we
//! keep a lightweight placeholder so the crate structure compiles while other
//! building blocks (inode/state handling) are implemented and tested.

use std::sync::Arc;

use fuse_backend_rs::api::filesystem::FileSystem;
use fuse_backend_rs::api::Vfs;
use parking_lot::RwLock;

use crate::inode::InodeTable;
use crate::repo::Repository;

pub struct GitSnapFs {
    #[allow(dead_code)]
    repo: Arc<Repository>,
    #[allow(dead_code)]
    inodes: Arc<InodeTable>,
    #[allow(dead_code)]
    vfs: Arc<RwLock<Vfs>>,
}

impl GitSnapFs {
    pub fn new(repo: Repository, inodes: Arc<InodeTable>, vfs: Arc<RwLock<Vfs>>) -> Self {
        Self {
            repo: Arc::new(repo),
            inodes,
            vfs,
        }
    }
}

impl FileSystem for GitSnapFs {
    type Inode = u64;
    type Handle = u64;
}
