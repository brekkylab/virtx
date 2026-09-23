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
const BLOCK_SIZE: u32 = 4096;

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
    })
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
}
