use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt;

use parking_lot::RwLock;

use gix::ObjectId;

/// Type tags used to isolate inode namespaces for each Git object kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeType {
    Blob,
    Tree,
    Commit,
    Symlink,
    /// Synthetic objects (e.g., `/commits`, `/branches`, etc.).
    ///
    /// The default tag for synthetic entries is `0x7F`, but the value can be
    /// overridden when a more fine-grained categorisation is desired.
    Synthetic(u8),
}

impl InodeType {
    pub fn tag(self) -> u8 {
        match self {
            Self::Blob => 0,
            Self::Tree => 1,
            Self::Commit => 2,
            Self::Symlink => 3,
            Self::Synthetic(value) => value,
        }
    }

    pub fn from_tag(tag: u8) -> Self {
        match tag {
            0 => Self::Blob,
            1 => Self::Tree,
            2 => Self::Commit,
            3 => Self::Symlink,
            other => Self::Synthetic(other),
        }
    }
}

/// Metadata tracking for a single inode slot.
#[derive(Debug, Clone)]
pub struct InodeEntry {
    pub oid: ObjectId,
    pub kind: InodeType,
    pub collisions: Vec<CollisionRecord>,
}

impl InodeEntry {
    fn new(oid: ObjectId, kind: InodeType) -> Self {
        Self {
            oid,
            kind,
            collisions: Vec::new(),
        }
    }
}

/// A collision records another `(oid, kind)` pair that hashed to the same inode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollisionRecord {
    pub oid: ObjectId,
    pub kind: InodeType,
}

/// Errors produced when registering Git objects with the inode table.
#[derive(thiserror::Error, Debug, Clone)]
pub enum InodeError {
    #[error(
        "inode collision on {ino:#x}: existing {existing_kind:?} ({existing_oid}), attempted {attempted_kind:?} ({attempted_oid})"
    )]
    Collision {
        ino: u64,
        existing_oid: ObjectId,
        existing_kind: InodeType,
        attempted_oid: ObjectId,
        attempted_kind: InodeType,
    },
}

impl InodeError {
    pub fn ino(&self) -> u64 {
        match self {
            Self::Collision { ino, .. } => *ino,
        }
    }
}

#[derive(Debug, Default)]
pub struct InodeTable {
    inner: RwLock<HashMap<u64, InodeEntry>>,
}

impl InodeTable {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Computes the inode for the given object id and type tag.
    pub fn ino_for(oid: &ObjectId, kind: InodeType) -> u64 {
        const LOW_60_MASK: u64 = (1u64 << 60) - 1;
        let bytes = oid.as_bytes();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes[bytes.len() - 8..]);
        let low = u64::from_be_bytes(buf) & LOW_60_MASK;
        low | ((kind.tag() as u64) << 60)
    }

    /// Records that the `(oid, kind)` pair has been observed. Returns the inode
    /// on success, or reports a collision with the existing mapping.
    pub fn register(&self, oid: &ObjectId, kind: InodeType) -> Result<u64, InodeError> {
        let ino = Self::ino_for(oid, kind);
        let mut guard = self.inner.write();
        match guard.entry(ino) {
            Entry::Vacant(slot) => {
                slot.insert(InodeEntry::new(oid.clone(), kind));
                Ok(ino)
            }
            Entry::Occupied(mut slot) => {
                if slot.get().oid == *oid && slot.get().kind == kind {
                    return Ok(ino);
                }

                let entry = slot.get_mut();
                let collision = CollisionRecord {
                    oid: oid.clone(),
                    kind,
                };
                if !entry.collisions.contains(&collision) {
                    entry.collisions.push(collision.clone());
                }

                Err(InodeError::Collision {
                    ino,
                    existing_oid: entry.oid.clone(),
                    existing_kind: entry.kind,
                    attempted_oid: collision.oid,
                    attempted_kind: collision.kind,
                })
            }
        }
    }

    pub fn get(&self, ino: u64) -> Option<InodeEntry> {
        self.inner.read().get(&ino).cloned()
    }

    pub fn snapshot(&self) -> HashMap<u64, InodeEntry> {
        self.inner.read().clone()
    }

    pub fn restore(&self, entries: HashMap<u64, InodeEntry>) {
        let mut guard = self.inner.write();
        guard.extend(entries);
    }
}

impl fmt::Display for CollisionRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({:?})", self.oid, self.kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::hash::ObjectId as HashObjectId;

    fn oid(hex: &str) -> ObjectId {
        HashObjectId::from_hex(hex.as_bytes()).unwrap().into()
    }

    #[test]
    fn ino_type_tags_use_high_bits() {
        let oid = oid("0123456789abcdef0123456789abcdef01234567");
        let blob = InodeTable::ino_for(&oid, InodeType::Blob);
        let tree = InodeTable::ino_for(&oid, InodeType::Tree);
        let commit = InodeTable::ino_for(&oid, InodeType::Commit);

        assert_eq!(blob & 0xF000_0000_0000_0000u64, 0);
        assert_eq!(tree >> 60, 1);
        assert_eq!(commit >> 60, 2);
        assert_ne!(blob, tree);
        assert_ne!(tree, commit);
    }

    #[test]
    fn collision_detection_flags_second_entry() {
        let table = InodeTable::new();
        let oid_a = oid("000000000000000000000000000000000000abcd");
        let oid_b = oid("111111111111111111111111111111111111abcd");

        // Force same inode by using manual tag and reusing lower bits.
        let ino = InodeTable::ino_for(&oid_a, InodeType::Blob);
        // Register first object
        assert_eq!(table.register(&oid_a, InodeType::Blob).unwrap(), ino);

        match table.register(&oid_b, InodeType::Blob) {
            Err(InodeError::Collision {
                ino: seen,
                existing_oid,
                attempted_oid,
                ..
            }) => {
                assert_eq!(seen, ino);
                assert_eq!(existing_oid, oid_a);
                assert_eq!(attempted_oid, oid_b);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }

        let entry = table.get(ino).expect("entry present");
        assert_eq!(entry.collisions.len(), 1);
        assert_eq!(entry.collisions[0].oid, oid_b);
    }

    #[test]
    fn synthetic_tag_roundtrip() {
        let tag = InodeType::Synthetic(0x7F);
        assert_eq!(InodeType::from_tag(tag.tag()), tag);
    }
}
