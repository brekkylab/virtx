//! The disk a boot is handed, and the descriptor that makes one out of several files.
//!
//! A [`Disk`] is the whole of what [`provision`](crate::image::provision) answers
//! with: a file to give a VM as its root block device, and what the image said to run
//! once it is mounted. Nothing else is provision's to say. Which slot the disk is
//! attached in, whether anything writable goes over it, and how the guest is told to
//! pivot onto it are decisions of the boot -- and one disk serves every answer to
//! them, as a read-only root or as the lower of an overlay alike.
//!
//! # The descriptor
//!
//! What the file is, is a VMDK flat descriptor: forty lines of plain text naming a
//! list of files and how many sectors to read of each, in order. That is what turns a
//! merged metadata image and the layers behind it into one block device -- no copy of
//! anything, and a guest that sees a filesystem rather than a stack.
//!
//! The descriptor is what fixes where each layer starts, so the addresses written into
//! the metadata image and the order of the extents here are two halves of one
//! statement. They are made in one place, by [`provision`](crate::image::provision),
//! for that reason.

use std::io::Write;
use std::path::{Path, PathBuf};

use super::{Error, ImageState, Result};

/// What a boot is handed.
///
/// Two things, because two things are all that is known here: the disk this host made,
/// and what the image that became it said about running. What the disk is made of is
/// written down in the cache, which is where a caller that wants to know asks.
#[derive(Clone, Debug)]
pub struct Disk {
    /// The VMDK descriptor to give a VM as its root block device, read as `erofs`.
    ///
    /// Attached read-only. Nothing behind it is a file this owns alone -- the layers
    /// are shared with every image stacked over them -- so a guest allowed to write to
    /// it would be writing into those.
    pub path: PathBuf,

    /// What to run once it is mounted, and with what: the image's environment, its
    /// entrypoint and command, where it stands and who it runs as.
    pub config: ImageState,
}

/// One extent line addresses at most 2 GiB, in 512-byte sectors. A longer file is
/// several lines reading successive parts of it.
const MAX_EXTENT_SECTORS: u64 = 4_194_304;

/// The sector a descriptor counts in.
const SECTOR: u64 = 512;

/// Geometry nothing reads and every descriptor states.
const HEADS: u64 = 16;
const SECTORS_PER_TRACK: u64 = 63;

/// Write a descriptor on `out` that reads `extents`, in order, as one disk.
///
/// Every extent has to be a whole number of sectors, because a descriptor addresses
/// sectors and half of one has no spelling. An EROFS image is a whole number of 4 KiB
/// blocks, so this only fires on something that is not one.
pub fn write_descriptor(extents: &[&Path], out: &mut impl Write) -> Result<()> {
    if extents.is_empty() {
        return Err(Error::Disk("a disk needs at least one extent".into()));
    }

    let mut total = 0u64;
    let mut lines = Vec::new();

    for path in extents {
        let size = std::fs::metadata(path)
            .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
            .len();
        if !size.is_multiple_of(SECTOR) {
            return Err(Error::Disk(format!(
                "{} is {size} bytes, which is not a whole number of {SECTOR}-byte sectors",
                path.display()
            )));
        }

        // Absolute, because a descriptor is read from wherever the VM happens to
        // stand, which is not where this was written.
        let absolute = std::fs::canonicalize(path)
            .map_err(|e| Error::io(format!("resolving {}", path.display()), e))?;
        // Not `Path::display`, which puts `U+FFFD` where a byte was not text and
        // hands back a name that opens some other file or none.
        let absolute = absolute.to_str().ok_or_else(|| {
            Error::Disk(format!(
                "{} is not valid UTF-8, which a descriptor cannot spell",
                absolute.display()
            ))
        })?;
        // The format puts one extent on one line and quotes the path, and offers no
        // escape for either delimiter. Refused rather than written, because what would
        // be written is a descriptor naming some other file: a quote ends the name
        // early, and a line break ends the extent and starts one nobody asked for.
        if absolute.contains(['"', '\n', '\r']) {
            return Err(Error::Disk(format!(
                "{absolute} has a quote or a line break in its name, \
                 which a descriptor cannot spell"
            )));
        }

        let sectors = size / SECTOR;
        let mut at = 0u64;
        while at < sectors {
            let chunk = (sectors - at).min(MAX_EXTENT_SECTORS);
            lines.push(format!("RW {chunk} FLAT \"{absolute}\" {at}"));
            at += chunk;
        }
        total += sectors;
    }

    let cylinders = total.div_ceil(HEADS * SECTORS_PER_TRACK);
    let mut text = String::new();
    text.push_str("# Disk DescriptorFile\n");
    text.push_str("version=1\n");
    text.push_str("CID=fffffffe\n");
    text.push_str("parentCID=ffffffff\n");
    text.push_str("createType=\"twoGbMaxExtentFlat\"\n\n");
    text.push_str("# Extent description\n");
    for line in &lines {
        text.push_str(line);
        text.push('\n');
    }
    text.push_str("\n# The Disk Data Base\n#DDB\n");
    text.push_str("ddb.virtualHWVersion = \"4\"\n");
    text.push_str(&format!("ddb.geometry.cylinders = \"{cylinders}\"\n"));
    text.push_str(&format!("ddb.geometry.heads = \"{HEADS}\"\n"));
    text.push_str(&format!("ddb.geometry.sectors = \"{SECTORS_PER_TRACK}\"\n"));
    text.push_str("ddb.adapterType = \"ide\"\n");

    out.write_all(text.as_bytes())
        .map_err(|e| Error::io("writing a disk descriptor", e))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path = std::env::temp_dir()
                .join(format!("cortex-krun-vmdk-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        fn extent(&self, name: &str, bytes: u64) -> PathBuf {
            let path = self.0.join(name);
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(bytes).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn descriptor(extents: &[&Path]) -> Result<String> {
        let mut out = Vec::new();
        write_descriptor(extents, &mut out)?;
        Ok(String::from_utf8(out).unwrap())
    }

    fn extent_lines(text: &str) -> Vec<&str> {
        text.lines()
            .filter(|line| line.starts_with("RW "))
            .collect()
    }

    #[test]
    fn a_descriptor_names_every_extent_in_order() {
        let scratch = Scratch::new("order");
        let (a, b) = (scratch.extent("a.bin", 4096), scratch.extent("b.bin", 8192));

        let text = descriptor(&[&a, &b]).unwrap();
        let lines = extent_lines(&text);

        assert!(text.contains("createType=\"twoGbMaxExtentFlat\""), "{text}");
        assert_eq!(lines.len(), 2, "{text}");
        assert!(lines[0].starts_with("RW 8 FLAT "), "{}", lines[0]);
        assert!(lines[0].contains("a.bin"), "{}", lines[0]);
        assert!(lines[1].starts_with("RW 16 FLAT "), "{}", lines[1]);
        // 24 sectors over 16 heads * 63 sectors, rounded up.
        assert!(text.contains("ddb.geometry.cylinders = \"1\""), "{text}");
    }

    #[test]
    fn an_extent_is_named_absolutely() {
        let scratch = Scratch::new("absolute");
        let only = scratch.extent("a.bin", 512);

        let text = descriptor(&[&only]).unwrap();
        let named = text
            .lines()
            .find(|line| line.starts_with("RW "))
            .and_then(|line| line.split('"').nth(1))
            .expect("an extent line with a path");
        assert!(
            Path::new(named).is_absolute(),
            "the extent is named {named:?}, which a VM standing elsewhere cannot open"
        );
    }

    /// Over 2 GiB is several lines reading successive parts of one file, and the
    /// offsets are what say which part.
    #[test]
    fn an_extent_over_two_gibibytes_is_split() {
        let scratch = Scratch::new("big");
        let big = scratch.extent("big.bin", 3 << 30);

        let lines = descriptor(&[&big]).unwrap();
        let lines = extent_lines(&lines);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[0].starts_with("RW 4194304 FLAT "), "{}", lines[0]);
        assert!(lines[0].ends_with(" 0"), "{}", lines[0]);
        assert!(lines[1].starts_with("RW 2097152 FLAT "), "{}", lines[1]);
        assert!(lines[1].ends_with(" 4194304"), "{}", lines[1]);
    }

    #[test]
    fn an_extent_that_is_not_whole_sectors_is_refused() {
        let scratch = Scratch::new("ragged");
        let ragged = scratch.extent("ragged.bin", 1000);
        assert!(matches!(descriptor(&[&ragged]), Err(Error::Disk(_))));
    }

    /// A line break is the quote's twin: one ends the name early, the other ends the
    /// extent and begins one nobody asked for.
    #[test]
    fn a_name_that_would_forge_an_extent_line_is_refused() {
        let scratch = Scratch::new("forged");
        for forged in ["a\nRW 8 FLAT \"passwd\" 0", "a\rb", "a\"b"] {
            let path = scratch.extent(forged, 512);
            assert!(
                matches!(descriptor(&[&path]), Err(Error::Disk(_))),
                "{forged:?} was accepted"
            );
        }
    }

    #[test]
    fn a_disk_of_nothing_is_refused() {
        assert!(matches!(descriptor(&[]), Err(Error::Disk(_))));
    }
}
