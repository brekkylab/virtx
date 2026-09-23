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
//! [`SIZE`] is a **ceiling and not an allocation**. The file is sparse, so a session that
//! writes a kilobyte occupies a kilobyte on the host; what the number bounds is how much
//! a runaway command can write before the guest reports a full disk.

use std::path::{Path, PathBuf};

use super::{Error, Result};

/// How large a session's upper layer may grow, when nothing says otherwise.
pub const SIZE: u64 = 2 << 30;

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
pub struct UpperLayer {
    pub path: PathBuf,

    /// What the filesystem on it reports as its size. Only meaningful for one this made:
    /// an upper layer that was already there is whatever it was formatted as.
    pub size: u64,
}

impl UpperLayer {
    /// The upper layer at `path`, formatted if there is nothing there yet.
    ///
    /// **An existing file is carried on rather than reformatted.** That is what makes a
    /// session persist: the overlay's upperdir is a directory on this disk, so a second
    /// run over the same path starts holding everything the first one wrote. Deleting the
    /// file is how a session is thrown away, and it is the only way.
    ///
    /// `size` is the ceiling for one being made now, and is ignored for one already here
    /// -- a formatted filesystem states its own, and this does not grow it.
    pub fn at(path: impl Into<PathBuf>, size: u64) -> Result<UpperLayer> {
        let path = path.into();

        match path.try_exists() {
            Ok(true) => return Ok(UpperLayer { path, size }),
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
        Ok(UpperLayer { path, size })
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
            let path = std::env::temp_dir().join(format!(
                "cortex-krun-upper-layer-{name}-{}",
                std::process::id()
            ));
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
    fn an_upper_layer_that_is_not_there_is_formatted() {
        let scratch = Scratch::new("new");
        let path = scratch.0.join("session.ext4");

        let upper_layer = UpperLayer::at(&path, 64 << 20).unwrap();
        assert_eq!(upper_layer.path, path);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 64 << 20);
        assert!(is_ext4(&path), "what was written is not an ext4 filesystem");
    }

    /// The whole of what makes a session persist: the file is the session, so a second
    /// call has to hand back the first one's and not a fresh filesystem over it.
    #[test]
    fn an_upper_layer_already_there_is_carried_on() {
        let scratch = Scratch::new("again");
        let path = scratch.0.join("session.ext4");

        let size: u64 = 64 << 20;
        UpperLayer::at(&path, size).unwrap();

        // A byte where nothing this wrote put one, which a reformat would take with it.
        let last = (size - 1) as usize;
        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes[last], 0, "the test's marker is not a marker");
        bytes[last] = 0x42;
        std::fs::write(&path, &bytes).unwrap();

        UpperLayer::at(&path, 1 << 30).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len() as u64, size, "the upper layer was resized");
        assert_eq!(bytes[last], 0x42, "the upper layer was formatted again");
    }

    /// The parent is made, so that a session directory nobody created is not a failure
    /// the caller has to have anticipated.
    #[test]
    fn an_upper_layer_under_a_directory_that_is_not_there_makes_it() {
        let scratch = Scratch::new("parents");
        let path = scratch.0.join("sessions/one/session.ext4");

        UpperLayer::at(&path, 64 << 20).unwrap();
        assert!(path.is_file());
    }
}
