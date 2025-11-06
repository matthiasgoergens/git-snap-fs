//! Conversion utilities between Git object ids and 64-bit inode numbers.
//!
//! The inode space is derived directly from the low 64 bits of the object id.
//! We intentionally avoid tracking collisions here – higher layers will consult
//! the Git object database with the derived prefix and surface an error if the
//! prefix is ambiguous.

use gix::ObjectId;

/// Convert a Git object id into a 64-bit inode by taking the low 64 bits.
///
/// # Panics
///
/// Panics if the object id is shorter than eight bytes, which cannot occur for valid Git object ids.
#[must_use]
pub fn inode_from_oid(oid: &ObjectId) -> u64 {
    u64::from_be_bytes(oid.as_bytes()[..8].try_into().unwrap())
}

/// Render the inode as a hexadecimal prefix string suitable for prefix
/// resolution in the Git object database.
#[must_use]
pub fn inode_to_hex_prefix(ino: u64) -> String {
    format!("{ino:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::hash::ObjectId as HashObjectId;

    fn oid(hex: &str) -> ObjectId {
        HashObjectId::from_hex(hex.as_bytes()).unwrap()
    }

    #[test]
    fn inode_roundtrip_low_bits() {
        let object = oid("0123456789abcdef0123456789abcdef01234567");
        let ino = inode_from_oid(&object);
        assert_eq!(ino, 0x0123_4567_89ab_cdef);
        assert_eq!(inode_to_hex_prefix(ino), "0123456789abcdef");
    }
}
