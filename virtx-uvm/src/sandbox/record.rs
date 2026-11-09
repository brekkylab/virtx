//! What lasts of a sandbox past its machine, and how it is kept on disk: `<id>.json` in the
//! directory its server keeps records in, for a server started again to know each, stopped.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use virtx_core::{SandboxCreateArgs, SandboxPort, SandboxState};

use crate::{Digest, ImageHold, ImageStore, Snapshot};

/// A sandbox as it lasts past its machine, from its making until it is deleted.
pub(super) struct Record {
    pub(super) id: String,
    pub(super) name: Option<String>,
    /// The image it stands on, by digest, held here for as long as it exists: what it boots
    /// every time it starts.
    pub(super) image: Digest,
    pub(super) _held: ImageHold,
    pub(super) ephemeral: bool,
    pub(super) auto_stop_minutes: u32,
    pub(super) auto_delete_days: Option<u32>,
    /// What it boots with each time: its machine's size and network, and its volumes. Its
    /// `image` is the digest it stands on, and its `ports` are not read: those it has are
    /// `opened`'s.
    pub(super) args: SandboxCreateArgs,
    /// Each port it opened, and where it is answered: those it was made with, and those
    /// opened since.
    pub(super) opened: Mutex<Vec<SandboxPort>>,
    /// The port each key names, the secret a server reaches a port by (a tcp server's
    /// capability): by key. Empty on a server that names its ports by nothing.
    pub(super) keys: Mutex<HashMap<String, u16>>,
    /// When it was made, or last used: a job started, run or ended in it, a connection through
    /// its ports, a request to its files.
    pub(super) used: Mutex<SystemTime>,
    /// How large its disk is to be, in GiB, as grown since it was made: grown so as it next
    /// boots ([`Snapshot::grow`]). None for one never grown, whose `args` say it.
    pub(super) disk_gib: Mutex<Option<u32>>,
    /// Its machine as resized while it was stopped: booted so from then on. None for what was
    /// never resized, whose `args` say it.
    pub(super) resized: Mutex<Resized>,
}

/// A sandbox's machine as resized since it was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resized {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
}

/// A [`Record`] as it is kept on disk. A server whose ports are named by no key keeps none:
/// `opened` and `capabilities` are read as empty where they are left out.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub(super) struct Kept {
    pub(super) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) name: Option<String>,
    pub(super) image: String,
    pub(super) ephemeral: bool,
    pub(super) auto_stop_minutes: u32,
    pub(super) auto_delete_days: Option<u32>,
    pub(super) args: SandboxCreateArgs,
    #[serde(default)]
    pub(super) opened: Vec<SandboxPort>,
    /// [`Record::keys`].
    #[serde(default)]
    pub(super) capabilities: HashMap<String, u16>,
    /// Seconds since the epoch.
    pub(super) used: u64,
    /// [`Record::disk_gib`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) disk_gib: Option<u32>,
    /// [`Record::resized`].
    #[serde(default)]
    pub(super) resized: Resized,
}

impl Record {
    /// The record `kept` is, holding its image in `images`.
    pub(super) fn of(kept: Kept, images: &ImageStore) -> Result<Record, String> {
        let image: Digest = kept.image.parse().map_err(|e| format!("{e}"))?;
        let held = images.hold_image(&image).map_err(|e| e.to_string())?;
        Ok(Record {
            id: kept.id,
            name: kept.name,
            image,
            _held: held,
            ephemeral: kept.ephemeral,
            auto_stop_minutes: kept.auto_stop_minutes,
            auto_delete_days: kept.auto_delete_days,
            args: kept.args,
            opened: Mutex::new(kept.opened),
            keys: Mutex::new(kept.capabilities),
            used: Mutex::new(UNIX_EPOCH + Duration::from_secs(kept.used)),
            disk_gib: Mutex::new(kept.disk_gib),
            resized: Mutex::new(kept.resized),
        })
    }

    /// Every record kept under `dir`, each holding its image in `images`; one whose image is
    /// gone, or that does not read, is said and left out.
    pub(super) fn load_all(dir: &Path, images: &ImageStore) -> Vec<Record> {
        let mut records = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return records;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let loaded = std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|json| serde_json::from_slice::<Kept>(&json).map_err(|e| e.to_string()))
                .and_then(|kept| Record::of(kept, images));
            match loaded {
                Ok(record) => records.push(record),
                Err(e) => eprintln!("sandbox at {} not read: {e}", path.display()),
            }
        }
        records
    }

    /// What it was made with, as resized since: what it boots as.
    pub(super) fn args_now(&self) -> SandboxCreateArgs {
        let mut args = self.args.clone();
        let resized = *self.resized.lock().unwrap();
        args.vcpus = resized.vcpus.or(args.vcpus);
        args.memory_mib = resized.memory_mib.or(args.memory_mib);
        args.disk_gib = self.disk_gib.lock().unwrap().or(args.disk_gib);
        args
    }

    /// How large its disk is to be, in bytes: as it was grown, or made.
    pub(super) fn disk_size(&self) -> u64 {
        self.disk_gib
            .lock()
            .unwrap()
            .or(self.args.disk_gib)
            .map_or(Snapshot::SIZE, |gib| u64::from(gib) << 30)
    }

    pub(super) fn kept(&self) -> Kept {
        Kept {
            id: self.id.clone(),
            name: self.name.clone(),
            image: self.image.to_string(),
            ephemeral: self.ephemeral,
            auto_stop_minutes: self.auto_stop_minutes,
            auto_delete_days: self.auto_delete_days,
            args: self.args.clone(),
            opened: self.opened.lock().unwrap().clone(),
            capabilities: self.keys.lock().unwrap().clone(),
            used: self
                .used
                .lock()
                .unwrap()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            disk_gib: *self.disk_gib.lock().unwrap(),
            resized: *self.resized.lock().unwrap(),
        }
    }

    /// It, as the protocol answers a sandbox: running or not, and with its ports or not.
    pub(super) fn answer(&self, running: bool, ports: bool) -> virtx_core::Sandbox {
        virtx_core::Sandbox {
            id: self.id.clone(),
            name: self.name.clone(),
            image: self.image.to_string(),
            state: match running {
                true => SandboxState::Running,
                false => SandboxState::Stopped,
            },
            ephemeral: self.ephemeral,
            auto_stop_minutes: self.auto_stop_minutes,
            auto_delete_days: self.auto_delete_days,
            used_at: rfc3339(*self.used.lock().unwrap()),
            ports: match ports {
                true => self.opened.lock().unwrap().clone(),
                false => Vec::new(),
            },
        }
    }
}

/// `at`, as RFC 3339 in UTC, to the second.
pub(super) fn rfc3339(at: SystemTime) -> String {
    let secs = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record as virtx cloud's nodes keep one.
    const NODE: &str = r#"{
  "id": "sbx_4f2a9c",
  "name": "dev",
  "image": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "ephemeral": false,
  "auto_stop_minutes": 15,
  "auto_delete_days": 30,
  "args": {
    "image": {
      "type": "digest",
      "digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    },
    "name": "dev",
    "ports": [
      8080,
      {
        "port": 5173,
        "label": "web",
        "protocol": "http"
      }
    ],
    "vcpus": 2
  },
  "opened": [
    {
      "port": 8080,
      "url": "ports/aaaa"
    },
    {
      "port": 5173,
      "url": "ports/bbbb",
      "label": "web",
      "protocol": "http"
    }
  ],
  "capabilities": {
    "aaaa": 8080,
    "bbbb": 5173
  },
  "used": 1762300000,
  "disk_gib": 4,
  "resized": {
    "memory_mib": 2048
  }
}"#;

    /// A record as virtx cloud's stdio server kept one: no ports apart from its args.
    const STDIO: &str = r#"{
  "id": "1a2b3c4d",
  "image": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
  "ephemeral": true,
  "auto_stop_minutes": 15,
  "auto_delete_days": null,
  "args": {
    "image": {
      "type": "digest",
      "digest": "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    },
    "ephemeral": true
  },
  "used": 1762300000
}"#;

    #[test]
    fn a_record_kept_by_a_node_reads_and_is_written_as_it_was() {
        let kept: Kept = serde_json::from_str(NODE).unwrap();
        assert_eq!(kept.capabilities["bbbb"], 5173);
        assert_eq!(kept.opened[1].label.as_deref(), Some("web"));
        assert_eq!(kept.disk_gib, Some(4));
        assert_eq!(kept.resized.memory_mib, Some(2048));
        let written = serde_json::to_string_pretty(&kept).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&written).unwrap(),
            serde_json::from_str::<serde_json::Value>(NODE).unwrap()
        );
    }

    #[test]
    fn a_record_kept_without_ports_reads_with_none() {
        let kept: Kept = serde_json::from_str(STDIO).unwrap();
        assert!(kept.ephemeral);
        assert_eq!(kept.auto_delete_days, None);
        assert!(kept.opened.is_empty() && kept.capabilities.is_empty());
        assert_eq!(kept.resized, Resized::default());
        let again: Kept = serde_json::from_str(&serde_json::to_string(&kept).unwrap()).unwrap();
        assert_eq!(again, kept);
    }

    #[test]
    fn a_time_is_written_as_rfc_3339() {
        assert_eq!(rfc3339(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let at = UNIX_EPOCH + Duration::from_secs(1_762_300_000);
        assert_eq!(rfc3339(at), "2025-11-04T23:46:40Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400);
        assert_eq!(rfc3339(leap), "2000-02-29T00:00:00Z");
    }
}
