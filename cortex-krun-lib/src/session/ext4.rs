//! The writable half of a session's root.
//!
//! # Why a disk and not a directory
//!
//! Because overlayfs will not take anything else. The guest kernel refuses a virtio-fs
//! share as an upperdir outright -- `filesystem on %s not supported as upperdir`, which
//! is overlayfs rejecting a filesystem whose dentries need revalidating, as every FUSE
//! one does. It is allowed as a *lower* and never as the upper layer.
//!
//! And a share that got past that would still not work, because of what an upper layer
//! has to be able to hold. Deleting a file the image shipped is recorded as a character
//! device numbered 0:0, and on a macOS host `mknod` of one is `EPERM` for anything but
//! root -- the same wall [`ingest`](crate::image::ingest) is written around. So `rm` of
//! any file in the image would fail, on a session that otherwise looked fine.
//!
//! A block device the guest formats itself has neither problem: what is on it is a Linux
//! filesystem the guest kernel owns, and the host only ever sees one file.
//!
//! # ext4, and how big
//!
//! ext4 because it is what the guest kernel has that an upper layer needs: xattrs for
//! overlayfs's own markers, `d_type`, whiteouts, and `O_TMPFILE` for copy-up. No journal
//! -- [`arcbox_ext4`] writes none -- which the kernel mounts read-write all the same, and
//! which is one less thing to write for a disk whose whole content is about to be thrown
//! away.
//!
//! [`Ext4::SIZE`] is a **ceiling and not an allocation**. The file is sparse, so a
//! session that writes a kilobyte occupies a kilobyte on the host; what the number bounds
//! is how much a runaway command can write before the guest reports a full disk.
//!
//! # The session nobody named
//!
//! A caller who names a path gets a session that outlives its machine. A caller who names
//! none gets [`scratch`](Ext4::scratch): the same ext4, under the host's temporary
//! directory, under a name nothing else can guess -- and [`discard`](Ext4::discard)
//! then takes even that name away, as soon as the machine has the file open. What is left
//! is a disk with no path to it, which the host reclaims when the process holding it goes
//! down, however it goes down.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Error, Result};

/// The block size ext4 is written with. The only one [`arcbox_ext4`] accepts, and the
/// one the guest kernel's page size makes natural anyway.
pub(crate) const BLOCK_SIZE: u32 = 4096;

/// A session's writable layer: one ext4 image, on the host, holding everything the
/// session writes and nothing the image shipped.
///
/// Attached as a block device and formatted **here** rather than in the guest, because a
/// guest that had to format its own root would need `mkfs.ext4` in the image -- and the
/// whole point of this is that it works over an image nobody prepared.
#[derive(Clone, Debug)]
pub struct Ext4 {
    pub path: PathBuf,

    /// What the filesystem on it reports as its size. Only meaningful for one this made:
    /// an upper layer that was already there is whatever it was formatted as.
    pub size: u64,

    /// Whether this one is a throwaway -- made by [`scratch`](Ext4::scratch)
    /// because nobody named a path, and so nobody's to keep. It is what
    /// [`discard`](Ext4::discard) acts on, and what keeps `discard` from ever
    /// touching a session a caller means to come back to.
    pub scratch: bool,
}

impl Ext4 {
    /// How large a session's upper layer may grow, when nothing says otherwise. An
    /// associated constant rather than a free one because a caller building a layer has
    /// to name a size, and this is the one to name when there is no reason to pick
    /// another.
    pub const SIZE: u64 = 2 << 30;

    /// Where overlayfs's upperdir is on this filesystem.
    ///
    /// The guest's init makes it, and makes `work/` beside it -- a workdir has to be on
    /// the same filesystem as the upperdir, and this is the only one here that is
    /// writable and outlives the boot. So the filesystem's own root is not the layer:
    /// what a session wrote is one directory down, and `work/` next to it is overlayfs's
    /// own and is nobody's changeset.
    const UPPER: &'static str = "/upper";

    /// The upper layer at `path`, formatted if there is nothing there yet.
    ///
    /// **An existing file is carried on rather than reformatted.** That is what makes a
    /// session persist: the overlay's upperdir is a directory on this disk, so a second
    /// run over the same path starts holding everything the first one wrote. Deleting the
    /// file is how a session is thrown away, and it is the only way.
    ///
    /// `size` is the ceiling for one being made now, and is ignored for one already here
    /// -- a formatted filesystem states its own, and this does not grow it.
    pub fn at(path: impl Into<PathBuf>, size: u64) -> Result<Ext4> {
        let path = path.into();

        match path.try_exists() {
            Ok(true) => {
                return Ok(Ext4 {
                    path,
                    size,
                    scratch: false,
                });
            }
            Ok(false) => {}
            Err(e) => return Err(Error::io(format!("looking for {}", path.display()), e)),
        }

        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("creating {}", parent.display()), e))?;
        }

        format(&path, size)?;
        Ok(Ext4 {
            path,
            size,
            scratch: false,
        })
    }

    /// Record that this filesystem is no longer mounted, having been flushed.
    ///
    /// **Call this only once the machine is down and was told to flush first.** Nothing
    /// here can check either; the caller is what stopped the machine.
    ///
    /// A Linux kernel clears `s_state`'s "valid" bit when it mounts a filesystem for
    /// writing and sets it again when it unmounts. This one is never unmounted: it is the
    /// upper half of the overlay the guest pivoted onto, so it is the root -- and a root
    /// cannot be unmounted by the system standing on it. The machine is stopped by killing
    /// it, and the bit is still clear afterwards.
    ///
    /// So the state on disk says "still mounted" about a filesystem nothing is mounting,
    /// and a reader is right to refuse that. What makes it safe to say otherwise is the
    /// flush: there is no journal on this filesystem -- [`arcbox_ext4`] writes none -- so
    /// once `sync(2)` has returned in the guest and nothing has run since, what is in the
    /// file is the whole of what the guest wrote.
    ///
    /// The other bit is left alone and refused. "Errors detected" is the kernel saying it
    /// found this filesystem inconsistent while it was using it, which no amount of
    /// flushing answers -- and a layer built out of one would be wrong in a way nothing
    /// downstream could notice.
    pub fn settled(&self) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom, Write};

        let io = |context: String| move |e: std::io::Error| Error::io(context.clone(), e);
        let named = || format!("the superblock of {}", self.path.display());

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .map_err(io(format!("opening {}", self.path.display())))?;

        let mut state = [0u8; 2];
        file.seek(SeekFrom::Start(STATE))
            .and_then(|_| file.read_exact(&mut state))
            .map_err(io(format!("reading {}", named())))?;
        let state = u16::from_le_bytes(state);

        if state & ERRORS != 0 {
            return Err(Error::io(
                format!("reading {}", named()),
                std::io::Error::other(
                    "the guest kernel found this filesystem inconsistent while it was                      running, so what it holds is not a changeset to keep",
                ),
            ));
        }

        file.seek(SeekFrom::Start(STATE))
            .and_then(|_| file.write_all(&(state | VALID).to_le_bytes()))
            .and_then(|_| file.sync_all())
            .map_err(io(format!("writing {}", named())))
    }

    /// Write what this session wrote onto `out` as a layer blob, and answer with the
    /// blob's digest -- which is the layer's `diff_id`.
    ///
    /// The whole of what a build is for: a machine boots on an image, writes on this, and
    /// what it wrote becomes a layer over that image. What comes out is an uncompressed
    /// OCI layer archive, which is what [`ingest`](crate::image::ingest) reads and so what
    /// every other part of the image side already knows how to take.
    ///
    /// `skip` names guest paths that are not the session's work, as the guest spells them.
    /// A share is mounted over a directory and making that directory is a write, so every
    /// mount point the boot created is recorded here -- and a layer claiming an empty
    /// `/.cortex` would be claiming something nobody asked for. What those paths are is
    /// the caller's to say, since the mounts were.
    ///
    /// **The machine has to be down, and to have synced before it went.** This reads the
    /// file, and a guest killed with writes still in its page cache has not put them in
    /// the file. Nothing here can check that; it is why the caller stops the machine
    /// rather than this.
    pub fn to_blob(&self, skip: &[&str], out: impl std::io::Write) -> Result<crate::image::Digest> {
        Ok(crate::image::ext4_to_blob(
            &self.path,
            Self::UPPER,
            skip,
            out,
        )?)
    }

    /// The upper layer at `path`, formatted with `tar` already written into it.
    ///
    /// **What a session that was handed its own writes back starts from.** `tar` is an
    /// uncompressed OCI layer archive -- what [`to_blob`](Ext4::to_blob) wrote out of some
    /// earlier session, handed back by the client that kept it -- and what comes back is
    /// a disk holding exactly that, ready for a guest to stack an image under.
    ///
    /// It is read from its start and read twice, so it has to be the whole archive and it
    /// has to be seekable.
    ///
    /// **A path that is already there is refused**, which is the one way this differs from
    /// [`at`](Ext4::at). That one carries an existing disk on, because a session that comes
    /// back to the same file is the same session. Seeding is the other thing: a layer is
    /// what a session *starts* from, and writing one over a disk that already holds writes
    /// would be two sessions' work in one filesystem with no way to tell which said what.
    pub fn seeded(
        path: impl Into<PathBuf>,
        size: u64,
        tar: &mut (impl std::io::Read + std::io::Seek),
    ) -> Result<Ext4> {
        let path = path.into();

        match path.try_exists() {
            Ok(false) => {}
            Ok(true) => {
                return Err(Error::io(
                    format!("seeding {}", path.display()),
                    std::io::Error::new(
                        std::io::ErrorKind::AlreadyExists,
                        "a layer is what a session starts from, and there is a disk here \
                         already holding one session's writes",
                    ),
                ));
            }
            Err(e) => return Err(Error::io(format!("looking for {}", path.display()), e)),
        }

        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::io(format!("creating {}", parent.display()), e))?;
        }

        crate::image::blob_to_ext4(&path, Self::UPPER, size, tar)?;
        Ok(Ext4 {
            path,
            size,
            scratch: false,
        })
    }

    /// An upper layer for a session that named none: a fresh ext4 under the host's
    /// temporary directory, which nothing is going to find and nothing is going to carry
    /// on from.
    ///
    /// The name holds the process's id and the moment it was made, so two sessions in one
    /// process do not land on one file. Nobody is meant to read it -- it is a name only
    /// because a block device has to be opened by one, and
    /// [`discard`](Ext4::discard) is what removes it once that is done.
    pub fn scratch(size: u64) -> Result<Ext4> {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "cortex-krun-session-{}-{unique}.ext4",
            std::process::id()
        ));

        format(&path, size)?;
        Ok(Ext4 {
            path,
            size,
            scratch: true,
        })
    }

    /// Unlink a [`scratch`](Ext4::scratch) upper layer, and do nothing at all to
    /// one a caller named.
    ///
    /// **Called once whatever is going to read the file has it open**, which for a
    /// session is after the machine is built and before it runs. A file with no name is
    /// still a file to the descriptor holding it, so the guest goes on writing to a disk
    /// that no longer exists as a path -- and the host reclaims it when that descriptor
    /// closes, which includes every way a process can die without running any code of its
    /// own. Unlinking any earlier would leave the machine nothing to open.
    ///
    /// Best effort, and silent: a host that refuses the unlink leaves a file in a
    /// temporary directory, which is the one place a leftover file is already expected.
    /// There is nothing a caller could do about it and nothing for it to fail.
    pub fn discard(&self) {
        if self.scratch {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Write an empty ext4 filesystem of `size` bytes at `path`.
///
/// Nothing is put in it. The two directories overlayfs wants -- the upperdir and its
/// workdir -- are made by the guest on the mounted filesystem instead, because the guest
/// has to make them anyway for a disk it is carrying on from and one place that makes
/// them is one fewer place they can disagree.
///
/// A failure leaves no file behind. A half-written filesystem is worse than none: the run
/// after it would find a path that exists, carry on from it as above, and mount garbage.
fn format(path: &Path, size: u64) -> Result<()> {
    let written =
        arcbox_ext4::Formatter::new(path, BLOCK_SIZE, size).and_then(arcbox_ext4::Formatter::close);

    written.map_err(|source| {
        let _ = std::fs::remove_file(path);
        Error::Format {
            path: path.display().to_string(),
            source,
        }
    })?;

    corrected(path).inspect_err(|_| {
        let _ = std::fs::remove_file(path);
    })
}

/// Where the superblock records how the filesystem was left, and the two bits of it that
/// mean anything here: cleanly unmounted, and found inconsistent while mounted.
const STATE: u64 = 0x400 + 0x3a;
const VALID: u16 = 0b01;
const ERRORS: u16 = 0b10;

/// Where the superblock records which operating system made the filesystem, and the value
/// that means Linux.
const CREATOR_OS: u64 = 0x400 + 0x48;
const LINUX: u32 = 0;

/// Where an inode keeps its flags, and the two that are acted on here: one that has to
/// come off everything, and one that has to come off a node with no blocks at all.
const INODE_FLAGS: u64 = 0x20;
const HUGE_FILE: u32 = 0x4_0000;
const EXTENTS: u32 = 0x8_0000;

/// Where an inode's mode is, which is its first field.
const INODE_MODE: u64 = 0x00;

/// Where the sixty bytes an inode maps its blocks with are -- and, for a node that has no
/// blocks, where the number of the device it is goes instead.
const INODE_BLOCK: u64 = 0x28;
const INODE_BLOCK_LEN: usize = 60;

/// Correct the two things [`arcbox_ext4`] writes that describe this filesystem as
/// something it is not.
///
/// Neither stops a guest mounting it: Linux ignores the first and does not act on the
/// second. Both matter because the filesystem is read again **on the host** -- see
/// [`Ext4::to_blob`] -- and a reader is right to believe what the metadata says about
/// itself.
///
/// `s_creator_os` is written as 3, FreeBSD. The field is what says how the twelve bytes of
/// `osd2` in every inode are laid out, and the high halves of an owner's uid and gid are in
/// there; this filesystem's inodes are Linux's, because a Linux guest is what fills it.
///
/// `EXT4_HUGE_FILE_FL` is set on every inode the formatter makes, including the root
/// directory. It means "`i_blocks` counts filesystem blocks and not 512-byte sectors",
/// which is a statement about files too large to count in sectors -- and a one-block root
/// directory is not one. The inodes that matter later are the guest's own and carry it
/// only when it is true, so this is only ever clearing it off what was written here.
pub(crate) fn corrected(path: &Path) -> Result<()> {
    use std::io::{Seek, SeekFrom, Write};

    let io = |context: String| move |e: std::io::Error| Error::io(context.clone(), e);

    // Every inode there is, which at this point is the root and whatever the formatter
    // put beside it. Read through the crate that wrote them, which is the one case its
    // reader is exactly right for.
    let mut reader = arcbox_ext4::Reader::new(path)
        .map_err(|e| Error::io(format!("reading {}", path.display()), std::io::Error::other(e)))?;
    let mut inodes = vec![ROOT_INODE];
    let mut walking = vec![reader.tree().root()];
    while let Some(index) = walking.pop() {
        let node = reader.tree().node(index);
        inodes.push(node.inode);
        walking.extend(node.children.iter().copied());
    }

    let sb = reader.superblock().clone();
    let block_size = 1024u64 << sb.log_block_size;
    let mut at = Vec::with_capacity(inodes.len());
    for ino in inodes {
        let group = (ino - 1) / sb.inodes_per_group;
        let within = u64::from((ino - 1) % sb.inodes_per_group);
        let table = u64::from(
            reader
                .get_group_descriptor(group)
                .map_err(|e| {
                    Error::io(
                        format!("reading {}", path.display()),
                        std::io::Error::other(e),
                    )
                })?
                .inode_table_lo,
        );
        at.push(table * block_size + within * u64::from(sb.inode_size));
    }
    drop(reader);

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(io(format!("opening {}", path.display())))?;

    file.seek(SeekFrom::Start(CREATOR_OS))
        .and_then(|_| file.write_all(&LINUX.to_le_bytes()))
        .map_err(io(format!("writing the superblock of {}", path.display())))?;

    for offset in at {
        let mut flags = [0u8; 4];
        let read = (|| {
            use std::io::Read;
            file.seek(SeekFrom::Start(offset + INODE_FLAGS))?;
            file.read_exact(&mut flags)
        })();
        read.map_err(io(format!("reading an inode of {}", path.display())))?;

        let cleared = u32::from_le_bytes(flags) & !HUGE_FILE;
        file.seek(SeekFrom::Start(offset + INODE_FLAGS))
            .and_then(|_| file.write_all(&cleared.to_le_bytes()))
            .map_err(io(format!("writing an inode of {}", path.display())))?;
    }

    Ok(())
}

/// The inode every ext4 filesystem's root directory is.
const ROOT_INODE: u32 = 2;

/// A node [`arcbox_ext4`]'s formatter has no way to write, as what it is and where it went.
///
/// Written as an empty regular file and named here, for [`special`] to turn into what it
/// actually is.
pub(crate) struct Special {
    /// Where it is on the filesystem, spelled the way the formatter's own tree spells one
    /// -- no leading separator, as in `upper/etc/gone`.
    pub path: String,
    /// The whole of `i_mode`, type bits included. What makes the inode a device or a pipe
    /// rather than the empty file it was written as.
    pub mode: u16,
    /// The device it is, for a node that is one. Both zero for a pipe, and both zero for
    /// the one device that means a deletion.
    pub major: u32,
    pub minor: u32,
}

/// Turn each of `nodes` from the empty file it was written as into the device or pipe it is.
///
/// **The one thing a layer holds that [`arcbox_ext4`] will not write.** Its formatter
/// takes a directory, a regular file and a symlink, and answers `UnsupportedFiletype` to
/// anything else -- which is understandable for a crate that writes OCI images, and
/// impossible for one that writes an *upper* layer, where a deletion is a character device
/// numbered 0:0 and nothing else says one.
///
/// So the inode is made as an empty file, with its ownership, its timestamps and its
/// extended attributes already right, and only the three things that cannot be asked for
/// are written here: the type bits of the mode, the flag that says the file's blocks are
/// mapped by an extent tree -- which a node with no blocks must not carry -- and the
/// number of the device, which lives in the sixty bytes an ordinary file would map its
/// blocks with.
///
/// Called after [`corrected`], which walks every inode there is; this one only ever
/// touches inodes that one has already been over.
pub(crate) fn special(disk: &Path, nodes: &[Special]) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom, Write};

    if nodes.is_empty() {
        return Ok(());
    }

    let io = |context: String| move |e: std::io::Error| Error::io(context.clone(), e);
    let unreadable = |e: arcbox_ext4::error::ReadError| {
        Error::io(
            format!("reading {}", disk.display()),
            std::io::Error::other(e),
        )
    };

    // Every one of them looked up before anything is written, because the reader holds the
    // file open and what follows writes to it.
    let mut reader = arcbox_ext4::Reader::new(disk).map_err(unreadable)?;
    let sb = reader.superblock().clone();
    let block_size = 1024u64 << sb.log_block_size;

    let mut at = Vec::with_capacity(nodes.len());
    for node in nodes {
        let found = reader.tree().lookup(Path::new(&node.path)).ok_or_else(|| {
            Error::io(
                format!("reading {}", disk.display()),
                std::io::Error::other(format!(
                    "{} is not on it, having just been written to it",
                    node.path
                )),
            )
        })?;
        let ino = reader.tree().node(found).inode;
        let group = (ino - 1) / sb.inodes_per_group;
        let within = u64::from((ino - 1) % sb.inodes_per_group);
        let table = u64::from(reader.get_group_descriptor(group).map_err(unreadable)?.inode_table_lo);
        at.push(table * block_size + within * u64::from(sb.inode_size));
    }
    drop(reader);

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(disk)
        .map_err(io(format!("opening {}", disk.display())))?;

    let wrote = || format!("writing an inode of {}", disk.display());

    for (node, offset) in nodes.iter().zip(at) {
        file.seek(SeekFrom::Start(offset + INODE_MODE))
            .and_then(|_| file.write_all(&node.mode.to_le_bytes()))
            .map_err(io(wrote()))?;

        let mut flags = [0u8; 4];
        (|| {
            file.seek(SeekFrom::Start(offset + INODE_FLAGS))?;
            file.read_exact(&mut flags)
        })()
        .map_err(io(format!("reading an inode of {}", disk.display())))?;
        let cleared = u32::from_le_bytes(flags) & !EXTENTS;
        file.seek(SeekFrom::Start(offset + INODE_FLAGS))
            .and_then(|_| file.write_all(&cleared.to_le_bytes()))
            .map_err(io(wrote()))?;

        let mut block = [0u8; INODE_BLOCK_LEN];
        block[..4].copy_from_slice(&device(node.major, node.minor).0.to_le_bytes());
        block[4..8].copy_from_slice(&device(node.major, node.minor).1.to_le_bytes());
        file.seek(SeekFrom::Start(offset + INODE_BLOCK))
            .and_then(|_| file.write_all(&block))
            .map_err(io(wrote()))?;
    }

    file.sync_all().map_err(io(wrote()))
}

/// A device number, in the two spellings an inode has for one.
///
/// A device small enough to say the old way is said the old way, in the first of the
/// sixty bytes, because that is the one a kernel reads first and the only one anything
/// this writes has ever needed -- a whiteout is 0:0. The second is where the wider form
/// goes, for a node a session made with `mknod` of something large.
fn device(major: u32, minor: u32) -> (u32, u32) {
    if major < 256 && minor < 256 {
        ((major << 8) | minor, 0)
    } else {
        (0, (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12))
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir()
                .join(format!("cortex-krun-ext4-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The superblock's magic, where ext4 keeps it: 0x400 into the filesystem, 0x38 into
    /// the superblock.
    fn is_ext4(path: &Path) -> bool {
        let bytes = std::fs::read(path).unwrap();
        bytes[0x400 + 0x38..0x400 + 0x3a] == [0x53, 0xef]
    }

    #[test]
    fn an_ext4_that_is_not_there_is_formatted() {
        let scratch = Scratch::new("new");
        let path = scratch.0.join("session.ext4");

        let ext4 = Ext4::at(&path, 64 << 20).unwrap();
        assert_eq!(ext4.path, path);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 64 << 20);
        assert!(is_ext4(&path), "what was written is not an ext4 filesystem");
    }

    /// The whole of what makes a session persist: the file is the session, so a second
    /// call has to hand back the first one's and not a fresh filesystem over it.
    #[test]
    fn an_ext4_already_there_is_carried_on() {
        let scratch = Scratch::new("again");
        let path = scratch.0.join("session.ext4");

        let size: u64 = 64 << 20;
        Ext4::at(&path, size).unwrap();

        // A byte where nothing this wrote put one, which a reformat would take with it.
        let last = (size - 1) as usize;
        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes[last], 0, "the test's marker is not a marker");
        bytes[last] = 0x42;
        std::fs::write(&path, &bytes).unwrap();

        Ext4::at(&path, 1 << 30).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, size, "the upper layer was resized");
        assert_eq!(bytes[last], 0x42, "the upper layer was formatted again");
    }

    /// A session nobody named still needs a real ext4 to write to; what it does not need
    /// is a name anyone can reach it by.
    #[test]
    fn an_ext4_nobody_named_is_formatted_and_then_unlinked() {
        let ext4 = Ext4::scratch(64 << 20).unwrap();
        assert!(ext4.scratch);
        assert!(is_ext4(&ext4.path));

        ext4.discard();
        assert!(
            !ext4.path.exists(),
            "a throwaway upper layer outlived its discard"
        );
    }

    /// The whole reason `discard` asks whether it may: a session a caller named is one
    /// they mean to come back to, and discarding it would be the one thing that throws it
    /// away.
    #[test]
    fn discarding_an_ext4_a_caller_named_does_nothing() {
        let scratch = Scratch::new("named");
        let path = scratch.0.join("session.ext4");

        let ext4 = Ext4::at(&path, 64 << 20).unwrap();
        assert!(!ext4.scratch);

        ext4.discard();
        assert!(path.is_file(), "a named session was thrown away");
    }

    /// The parent is made, so that a session directory nobody created is not a failure
    /// the caller has to have anticipated.
    #[test]
    fn an_ext4_under_a_directory_that_is_not_there_makes_it() {
        let scratch = Scratch::new("parents");
        let path = scratch.0.join("sessions/one/session.ext4");

        Ext4::at(&path, 64 << 20).unwrap();
        assert!(path.is_file());
    }

    /// **The whole of what a client handing a session back and forth is.** A layer goes on
    /// a disk gzipped, the disk is read back out, and what comes off it is the layer that
    /// went on -- which is the round trip a `snapshot` answered and handed to the next
    /// `init` makes, minus the wire between them.
    #[test]
    fn a_session_handed_back_is_the_session_that_was_handed_out() {
        use std::io::Cursor;

        let scratch = Scratch::new("handed");

        // What a snapshot of some earlier session came to: a file it wrote, and the
        // whiteout for one it deleted.
        let mut archive = tar::Builder::new(Vec::new());
        for (path, body) in [("home/user/notes", &b"kept\n"[..]), ("etc/.wh.gone", b"")] {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(body.len() as u64);
            header.set_mtime(1_700_000_000);
            archive
                .append_data(&mut header, path, Cursor::new(body.to_vec()))
                .unwrap();
        }
        let sent = archive.into_inner().unwrap();

        // Down the wire and back: gzipped by the server answering `snapshot`, and read as
        // whatever it arrives as by the one taking an `init`.
        let mut carried = Vec::new();
        crate::image::compress_blob(&sent[..], &mut carried).unwrap();
        let mut tar = Vec::new();
        crate::image::decompress_blob("gzip", &carried[..], &mut tar).unwrap();
        assert_eq!(tar, sent, "the layer did not survive being carried");

        let upper = Ext4::seeded(
            scratch.0.join("session.ext4"),
            64 << 20,
            &mut Cursor::new(tar),
        )
        .unwrap();

        // No `settled` first: nothing has mounted this, so it is still saying it was left
        // cleanly -- which is the one thing a disk fresh off a format can say honestly.
        let mut back = Vec::new();
        upper.to_blob(&[], &mut back).unwrap();

        // By what it says and not byte for byte, because the two are not the same archive:
        // a tar may leave the directories above an entry out and a filesystem cannot, so
        // what comes off the disk names `home/` and `home/user/` as well. What has to
        // survive is every statement the layer made.
        let tree = crate::image::ingest(&mut Cursor::new(&back)).unwrap();
        assert!(
            matches!(
                tree.get(b"home/user/notes"),
                Some(crate::image::Node::File { .. })
            ),
            "the file the session wrote did not come back"
        );
        assert!(
            matches!(
                tree.get(b"etc/gone"),
                Some(crate::image::Node::Device {
                    block: false,
                    major: 0,
                    minor: 0,
                    ..
                })
            ),
            "the file the session deleted came back as something else"
        );

        // And the trip is a fixed point: what came off the disk, put on another one, comes
        // off it again unchanged. A client carrying a session for as long as it likes is
        // making this trip over and over.
        let again = Ext4::seeded(
            scratch.0.join("again.ext4"),
            64 << 20,
            &mut Cursor::new(back.clone()),
        )
        .unwrap();
        let mut twice = Vec::new();
        again.to_blob(&[], &mut twice).unwrap();
        assert_eq!(twice, back, "carrying a session again changed it");
    }
}
