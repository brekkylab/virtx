//! What is kept of a sandbox past the process that made it: `<id>.json` beside its upper disk,
//! for a server started again over the same root to know it.
//!
//! ```text
//! <dir>/<id>.json        its record, written once it is made
//! <dir>/<id>.json.part   a record being written, renamed over the record once whole
//! ```

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use virtx_core::{SandboxCreateArgs, SandboxJob};

/// A sandbox as it is kept on disk.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Record {
    /// What it was made with, its `image` the digest it booted, which it boots every time.
    pub(super) args: SandboxCreateArgs,
    /// The port each key opens: what its server reaches a port by, such as a capability in the
    /// port's URL.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub(super) keys: HashMap<String, u16>,
    /// Whether its machine ended on its own: nothing boots it again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) exited: bool,
    /// Its jobs as they were when one last started or ended: each that ran then ran when its
    /// process let it go, and is known again as ended so.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) jobs: Vec<SandboxJob>,
}

impl Record {
    /// Where sandbox `id`'s record is under `dir`.
    pub(super) fn path(dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("{id}.json"))
    }

    /// Where sandbox `id`'s record is written before it is renamed over [`Record::path`].
    pub(super) fn part(dir: &Path, id: &str) -> PathBuf {
        dir.join(format!("{id}.json.part"))
    }

    /// The image it boots, by the digest its making resolved.
    pub(super) fn image(&self) -> Result<crate::Digest, String> {
        let virtx_core::ImageName::Digest { digest } = &self.args.image else {
            return Err("its record names its image by no digest".into());
        };
        digest.parse().map_err(|e| format!("{digest}: {e}"))
    }

    /// Sandbox `id`'s record under `dir`.
    pub(super) fn read(dir: &Path, id: &str) -> Result<Record, String> {
        let path = Record::path(dir, id);
        let json = fs::read(&path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        serde_json::from_slice(&json).map_err(|e| format!("reading {}: {e}", path.display()))
    }

    /// Write it as sandbox `id`'s record under `dir`, whole or not at all.
    pub(super) fn write(&self, dir: &Path, id: &str) -> io::Result<()> {
        let part = Record::part(dir, id);
        fs::write(&part, serde_json::to_vec_pretty(self)?)?;
        fs::rename(&part, Record::path(dir, id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_reads_as_it_was_written_and_leaves_no_part() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = Record {
            args: SandboxCreateArgs::new(
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            ),
            keys: HashMap::from([("key".to_owned(), 8080)]),
            exited: false,
            jobs: Vec::new(),
        };
        record.write(dir.path(), "kept").unwrap();
        assert_eq!(Record::read(dir.path(), "kept").unwrap(), record);
        record.exited = true;
        record.write(dir.path(), "kept").unwrap();
        assert_eq!(Record::read(dir.path(), "kept").unwrap(), record);
        assert!(!Record::part(dir.path(), "kept").exists());
        assert!(Record::read(dir.path(), "none").is_err());
    }
}
