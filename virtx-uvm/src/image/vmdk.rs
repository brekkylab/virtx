//! A VMDK flat descriptor: forty lines of plain text naming a list of files and how many
//! sectors to read of each, in order. That is what makes a root filesystem's metadata image
//! and the layers behind it one block device with no copy of any of them.

use std::fmt::Write as _;
use std::io;
use std::path::Path;

/// One extent line addresses at most 2 GiB, in 512-byte sectors. A longer file is several
/// lines reading successive parts of it.
const MAX_EXTENT_SECTORS: u64 = 4_194_304;

/// The sector a descriptor counts in.
const SECTOR: u64 = 512;

/// Geometry nothing reads and every descriptor states.
const HEADS: u64 = 16;
const SECTORS_PER_TRACK: u64 = 63;

/// A descriptor that reads `extents`, in order, as one disk.
///
/// Every extent has to be a whole number of sectors, because a descriptor addresses sectors
/// and half of one has no spelling. An EROFS image is a whole number of 4 KiB blocks, so
/// this only refuses something that is not one.
pub(super) fn descriptor(extents: &[&Path]) -> io::Result<String> {
    let refused = |what: String| io::Error::new(io::ErrorKind::InvalidInput, what);
    if extents.is_empty() {
        return Err(refused("a disk needs at least one extent".into()));
    }

    let mut total = 0u64;
    let mut lines = String::new();
    for path in extents {
        let size = std::fs::metadata(path)?.len();
        if !size.is_multiple_of(SECTOR) {
            return Err(refused(format!(
                "{} is {size} bytes, which is not a whole number of {SECTOR}-byte sectors",
                path.display()
            )));
        }

        // Absolute, because a descriptor is read from wherever the VM happens to stand,
        // which is not where this was written.
        let absolute = std::fs::canonicalize(path)?;
        // Not `Path::display`, which puts `U+FFFD` where a byte was not text and hands back
        // a name that opens some other file or none.
        let absolute = absolute.to_str().ok_or_else(|| {
            refused(format!(
                "{} is not valid UTF-8, which a descriptor cannot spell",
                absolute.display()
            ))
        })?;
        // The format puts one extent on one line and quotes the path, and offers no escape
        // for either delimiter. Refused rather than written, because what would be written
        // is a descriptor naming some other file: a quote ends the name early, and a line
        // break ends the extent and starts one nobody asked for.
        if absolute.contains(['"', '\n', '\r']) {
            return Err(refused(format!(
                "{absolute} has a quote or a line break in its name, which a descriptor \
                 cannot spell"
            )));
        }

        let sectors = size / SECTOR;
        let mut at = 0u64;
        while at < sectors {
            let chunk = (sectors - at).min(MAX_EXTENT_SECTORS);
            writeln!(lines, "RW {chunk} FLAT \"{absolute}\" {at}").unwrap();
            at += chunk;
        }
        total += sectors;
    }

    let cylinders = total.div_ceil(HEADS * SECTORS_PER_TRACK);
    Ok(format!(
        "# Disk DescriptorFile\n\
         version=1\n\
         CID=fffffffe\n\
         parentCID=ffffffff\n\
         createType=\"twoGbMaxExtentFlat\"\n\
         \n\
         # Extent description\n\
         {lines}\
         \n\
         # The Disk Data Base\n\
         #DDB\n\
         ddb.virtualHWVersion = \"4\"\n\
         ddb.geometry.cylinders = \"{cylinders}\"\n\
         ddb.geometry.heads = \"{HEADS}\"\n\
         ddb.geometry.sectors = \"{SECTORS_PER_TRACK}\"\n\
         ddb.adapterType = \"ide\"\n"
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let path =
                std::env::temp_dir().join(format!("virtx-uvm-vmdk-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        fn extent(&self, name: &str, bytes: u64) -> PathBuf {
            let path = self.0.join(name);
            std::fs::File::create(&path)
                .unwrap()
                .set_len(bytes)
                .unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
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
        let named = extent_lines(&text)[0].split('"').nth(1).unwrap();
        assert!(Path::new(named).is_absolute(), "{named:?}");
    }

    /// Over 2 GiB is several lines reading successive parts of one file, and the offsets
    /// are what say which part.
    #[test]
    fn an_extent_over_two_gibibytes_is_split() {
        let scratch = Scratch::new("big");
        let big = scratch.extent("big.bin", 3 << 30);

        let text = descriptor(&[&big]).unwrap();
        let lines = extent_lines(&text);
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
        assert!(descriptor(&[&ragged]).is_err());
    }

    /// A line break is the quote's twin: one ends the name early, the other ends the extent
    /// and begins one nobody asked for. Unix only, because NTFS refuses both in a name.
    #[cfg(unix)]
    #[test]
    fn a_name_that_would_forge_an_extent_line_is_refused() {
        let scratch = Scratch::new("forged");
        for forged in ["a\nRW 8 FLAT \"passwd\" 0", "a\rb", "a\"b"] {
            let path = scratch.extent(forged, 512);
            assert!(descriptor(&[&path]).is_err(), "{forged:?} was accepted");
        }
    }

    #[test]
    fn a_disk_of_nothing_is_refused() {
        assert!(descriptor(&[]).is_err());
    }
}
