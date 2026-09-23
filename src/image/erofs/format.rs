//! The EROFS on-disk format, as the guest kernel reads it.

//!
//! The numbers here are facts about `fs/erofs/erofs_fs.h`, not choices this crate
//! makes. Everything on disk is little-endian, whatever the host is.

use crate::image::tree::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFMT, S_IFSOCK};

/// Block size. EROFS admits others, but 4096 is what every kernel mounts without
/// argument and the page size the guest runs with.
pub const BLOCK_SIZE: u64 = 4096;
pub const BLOCK_BITS: u8 = 12;

/// The superblock sits past the boot sector, so an image can be partitioned or
/// signed ahead of it without moving anything.
pub const SUPER_OFFSET: u64 = 1024;
pub const SUPER_MAGIC: u32 = 0xE0F5_E1E2;
pub const SUPER_SIZE: usize = 128;

/// An inode is named by its `nid`, a count of 32-byte slots from the start of the
/// metadata area. That is the whole reason a layer cannot be streamed: an inode's
/// name is where it landed, so nothing can be written until everything is placed.
pub const SLOT_SIZE: u64 = 32;

/// The 64-bit inode. The 32-byte compact form exists, but it dates timestamps from
/// a per-image epoch and caps uid/gid at 16 bits, and a container image survives
/// neither.
pub const INODE_SIZE: u64 = 64;

/// `erofs_xattr_ibody_header`, which precedes an inode's inline xattrs.
pub const XATTR_HEADER_SIZE: u64 = 12;

/// `erofs_dirent`: an nid, the offset of the name within its block, and a type.
pub const DIRENT_SIZE: usize = 12;

/// The superblock carries a crc32c of itself. Optional, and cheap enough to be
/// worth it: it catches a truncated image before the guest tries to boot one.
pub const FEATURE_COMPAT_SB_CHKSUM: u32 = 0x0000_0001;

/// `i_format` is the inode layout version in bit 0 and the data layout in bits 1..3.
pub const INODE_VERSION_EXTENDED: u16 = 1;
pub const LAYOUT_FLAT_PLAIN: u16 = 0;
pub const LAYOUT_FLAT_INLINE: u16 = 2;

/// Dirent file types. Not the same numbering as `S_IF*`.
pub const FT_REG: u8 = 1;
pub const FT_DIR: u8 = 2;
pub const FT_CHRDEV: u8 = 3;
pub const FT_BLKDEV: u8 = 4;
pub const FT_FIFO: u8 = 5;
pub const FT_SOCK: u8 = 6;
pub const FT_SYMLINK: u8 = 7;

/// An xattr name is stored as a one-byte prefix index and the rest of the name, so
/// only the prefixes the format knows can be written at all.
pub const XATTR_INDEX_USER: u8 = 1;
pub const XATTR_INDEX_POSIX_ACL_ACCESS: u8 = 2;
pub const XATTR_INDEX_POSIX_ACL_DEFAULT: u8 = 3;
pub const XATTR_INDEX_TRUSTED: u8 = 4;
pub const XATTR_INDEX_SECURITY: u8 = 6;

/// Split an xattr name into its prefix index and the remainder. `None` for a prefix
/// EROFS has no index for, which is a name that cannot be stored rather than an
/// error in the caller.
pub fn xattr_prefix(name: &[u8]) -> Option<(u8, &[u8])> {
    const PREFIXES: [(&[u8], u8); 5] = [
        (b"user.", XATTR_INDEX_USER),
        (b"system.posix_acl_access", XATTR_INDEX_POSIX_ACL_ACCESS),
        (b"system.posix_acl_default", XATTR_INDEX_POSIX_ACL_DEFAULT),
        (b"trusted.", XATTR_INDEX_TRUSTED),
        (b"security.", XATTR_INDEX_SECURITY),
    ];

    PREFIXES
        .iter()
        .find(|(prefix, _)| name.starts_with(prefix))
        .map(|(prefix, index)| (*index, &name[prefix.len()..]))
}

/// Every xattr entry starts on a four-byte boundary.
pub fn xattr_align(size: u64) -> u64 {
    size.div_ceil(4) * 4
}

/// The dirent type for a mode, which is the mode's type bits renumbered.
pub fn file_type(mode: u16) -> u8 {
    match mode & S_IFMT {
        S_IFDIR => FT_DIR,
        S_IFLNK => FT_SYMLINK,
        S_IFCHR => FT_CHRDEV,
        S_IFBLK => FT_BLKDEV,
        S_IFIFO => FT_FIFO,
        S_IFSOCK => FT_SOCK,
        _ => FT_REG,
    }
}

/// The kernel's `new_encode_dev`: minor is split so that a 32-bit `i_u` can carry
/// the 20-bit minor Linux has had since 2.6.
pub fn encode_dev(major: u32, minor: u32) -> u32 {
    (minor & 0xFF) | (major << 8) | ((minor & !0xFF) << 12)
}

/// crc32c (Castagnoli), which is what the superblock checksum is. Seeded and
/// returned raw, with no final inversion, because that is how EROFS stores it.
pub fn crc32c(seed: u32, data: &[u8]) -> u32 {
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0u32; 256];
        for (index, slot) in table.iter_mut().enumerate() {
            let mut crc = index as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0x82F6_3B78
                } else {
                    crc >> 1
                };
            }
            *slot = crc;
        }
        table
    });

    data.iter().fold(seed, |crc, byte| {
        (crc >> 8) ^ table[((crc ^ u32::from(*byte)) & 0xFF) as usize]
    })
}

/// Round `offset` up to the next block.
pub fn align_block(offset: u64) -> u64 {
    offset.div_ceil(BLOCK_SIZE) * BLOCK_SIZE
}
