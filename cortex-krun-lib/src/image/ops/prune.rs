//! Reclaiming everything in the cache that no image names.
//!
//! The wide version of what [`remove`](super::remove) does narrowly. That one deletes the
//! files of the image it was given, and looks at nothing else; this one sweeps the whole
//! store, so it also takes what an interrupted pull left half-done, what an older build of
//! this code stitched under a name it no longer computes, and whatever a process that died
//! between writing a layer and writing the index never got to file.
//!
//! # The reason it takes a grace period
//!
//! Because a layer exists before it is named. A pull writes every layer into `layer/` and
//! only then writes the index -- it has to, since an index naming a layer that is not
//! there yet is an index a boot cannot use. So there is a window, as long as a download,
//! in which a perfectly good layer is on disk and nothing points at it. A sweep that ran
//! in that window and believed what the index said would delete it, and the pull would
//! then file an image whose layer is gone.
//!
//! Nothing here can see the pull to ask. What it can see is how recently each file was
//! written, so a file younger than `grace` is left alone -- not because it is referenced,
//! but because whatever wrote it may not have finished saying so yet. `Some(Duration::ZERO)`
//! is how a caller who knows nothing else is running says to sweep everything.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use super::{referenced, unlink};
use crate::image::{Cache, Error, Result};

/// How recently a file may have been written and still be spared, when the caller does not
/// say.
///
/// An hour, which is the shape of the thing being guarded against rather than a measured
/// number: the window is one pull of one image, and an hour is longer than that over any
/// link worth using while being short enough that a sweep still reclaims the same day's
/// mistakes.
pub const GRACE: Duration = Duration::from_secs(60 * 60);

/// What a sweep came to.
#[derive(Clone, Debug, Default)]
pub struct Pruned {
    /// The files deleted.
    pub files: Vec<PathBuf>,

    /// What they occupied, in bytes.
    pub bytes: u64,

    /// Files that no image names and that were left anyway, being younger than the grace
    /// period. Reported rather than passed over silently, because a caller looking at a
    /// cache that is larger than the listing explains is owed the reason.
    pub spared: usize,
}

/// Delete everything under `cache` that no image in the index names.
///
/// The layers, merged metadata images and disk descriptors that nothing points at, and
/// whatever is lying in `tmp/`. Nothing in the index is changed -- an image with every
/// layer present is never a candidate, so this takes no image away and needs no name.
///
/// `grace` spares files written more recently than that. `None` takes [`GRACE`], which is
/// the answer for a sweep running while anything else might be. `Some(Duration::ZERO)`
/// sweeps everything and is only right when nothing else is using this cache -- see the
/// module doc for what it is that gets lost otherwise.
///
/// The index is locked for the whole sweep, so nothing files an image against a file while
/// this is deciding whether to delete it. That does not close the window above, which is
/// about a pull that has not reached the index yet and so is holding nothing.
///
/// ```no_run
/// # fn demo() -> Result<(), Box<dyn std::error::Error>> {
/// use cortex_krun::image::prune;
///
/// let swept = prune(None, None)?;
/// println!("reclaimed {} MiB, spared {}", swept.bytes >> 20, swept.spared);
/// # Ok(())
/// # }
/// ```
pub fn prune(grace: Option<Duration>, cache: Option<Cache>) -> Result<Pruned> {
    let cache = cache.unwrap_or_default();
    let grace = grace.unwrap_or(GRACE);

    // Held for the whole sweep and then dropped rather than committed: nothing here
    // changes the index, and what the lock is for is that nothing else changes it either
    // while this is reading what is still referenced.
    let index = cache.image_index_mut()?;
    let keep = referenced(&index, &cache);

    let mut swept = Pruned::default();
    let younger = SystemTime::now().checked_sub(grace);

    for shelf in ["layer", "fsmeta", "disk", "tmp"] {
        sweep(&cache.root().join(shelf), &keep, younger, &mut swept)?;
    }

    drop(index);
    Ok(swept)
}

/// Delete what `at` holds that is not in `keep` and is not too young, walking down into
/// whatever directories it has.
///
/// A directory that is not there is not an error: a cache that has never had a disk
/// stitched has no `disk/`, and a sweep of it is a sweep of nothing.
fn sweep(
    at: &Path,
    keep: &HashSet<PathBuf>,
    younger: Option<SystemTime>,
    swept: &mut Pruned,
) -> Result<()> {
    let entries = match std::fs::read_dir(at) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io(format!("reading {}", at.display()), e)),
    };

    for entry in entries {
        let entry = entry.map_err(|e| Error::io(format!("reading {}", at.display()), e))?;
        let path = entry.path();

        let kind = entry
            .file_type()
            .map_err(|e| Error::io(format!("reading {}", path.display()), e))?;
        if kind.is_dir() {
            sweep(&path, keep, younger, swept)?;
            // The directory itself goes only if the sweep emptied it, and only then --
            // `remove_dir` refuses a directory with anything left in it, which is the
            // check rather than something to do before it.
            let _ = std::fs::remove_dir(&path);
            continue;
        }

        if keep.contains(&path) {
            continue;
        }

        // Too young to judge. Not an error and not a deletion: whatever wrote it may
        // still be on its way to saying so.
        if let Some(younger) = younger
            && entry
                .metadata()
                .ok()
                .and_then(|at| at.modified().ok())
                .is_some_and(|written| written > younger)
        {
            swept.spared += 1;
            continue;
        }

        if let Some(bytes) = unlink(&path) {
            swept.bytes += bytes;
            swept.files.push(path);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::super::tests::Scratch;
    use super::*;
    use crate::image::{list, provision, untag};

    /// Nothing that is named goes, whatever else does.
    #[test]
    fn what_an_image_still_needs_is_never_swept() {
        let scratch = Scratch::new("prune-keep");
        let cache = scratch.cache();
        let pulled = scratch.pulled(&cache, "test/base:1", &[(b"etc/one", b"a")]);
        provision("test/base:1", Some(cache.clone())).unwrap();

        let swept = prune(Some(Duration::ZERO), Some(cache.clone())).unwrap();
        assert!(swept.files.is_empty(), "{:?}", swept.files);

        assert!(cache.has_layer(&pulled.layers[0].digest));
        provision("test/base:1", Some(cache.clone())).unwrap();
        assert_eq!(list(Some(cache.clone())).unwrap().len(), 1);
    }

    /// The case `remove` cannot reach: a name taken off leaves the files behind, and
    /// this is what collects them.
    #[test]
    fn what_untagging_orphaned_is_swept() {
        let scratch = Scratch::new("prune-orphan");
        let cache = scratch.cache();
        let pulled = scratch.pulled(&cache, "test/base:1", &[(b"etc/one", b"a")]);
        provision("test/base:1", Some(cache.clone())).unwrap();

        // Untagging is deliberately not a deletion, so after this the layer and the disk
        // are files nothing names.
        untag("test/base:1", Some(cache.clone())).unwrap();
        assert!(cache.has_layer(&pulled.layers[0].digest), "untag deleted it");

        let swept = prune(Some(Duration::ZERO), Some(cache.clone())).unwrap();
        assert_eq!(swept.files.len(), 3, "{:?}", swept.files);
        assert!(swept.bytes > 0);
        assert!(!cache.has_layer(&pulled.layers[0].digest));
        assert!(!cache.has_disk(&crate::image::stack::digest(&pulled.layers)));
    }

    /// Whatever a pull that died left in `tmp/` is nobody's.
    #[test]
    fn a_half_written_file_is_swept() {
        let scratch = Scratch::new("prune-tmp");
        let cache = scratch.cache();
        scratch.pulled(&cache, "test/base:1", &[(b"etc/one", b"a")]);

        let mut abandoned = cache.staged_writer(cache.root().join("never")).unwrap();
        abandoned.write_all(b"half a layer").unwrap();
        std::mem::forget(abandoned); // as a process that died would have left it

        let swept = prune(Some(Duration::ZERO), Some(cache.clone())).unwrap();
        assert_eq!(swept.files.len(), 1, "{:?}", swept.files);
        assert!(
            std::fs::read_dir(cache.root().join("tmp"))
                .map(|entries| entries.count() == 0)
                .unwrap_or(true),
            "tmp still holds something"
        );
    }

    /// The whole reason there is a grace period: a layer on disk that nothing names yet
    /// is what every pull looks like halfway through.
    #[test]
    fn a_file_younger_than_the_grace_period_is_spared() {
        let scratch = Scratch::new("prune-young");
        let cache = scratch.cache();
        let pulled = scratch.pulled(&cache, "test/base:1", &[(b"etc/one", b"a")]);
        untag("test/base:1", Some(cache.clone())).unwrap();

        // Just written, so under any grace worth having.
        let swept = prune(None, Some(cache.clone())).unwrap();
        assert!(swept.files.is_empty(), "{:?}", swept.files);
        assert!(swept.spared > 0, "nothing was reported as spared");
        assert!(
            cache.has_layer(&pulled.layers[0].digest),
            "a layer a pull might still be filing was deleted"
        );
    }
}
