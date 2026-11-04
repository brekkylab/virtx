//! Sandboxes: what one is, as the server answers it, and what making one takes.
//!
//! A sandbox has a disk of its own, what its processes write over its image, which outlives its
//! VM: it stops when it is not used or is told to, and starts again as it was left, under the
//! same id, until it is deleted.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::image::ImageName;

/// A sandbox, as the server answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sandbox {
    /// The sandbox's id, which every request about it names it by.
    pub id: String,

    /// The name it was given, which finds it again ([`Client::find_sandbox`](crate::Client::find_sandbox)).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// The digest of the image it stands on, as `algorithm:hex`: what it boots every time it
    /// starts, whatever its tag points at by then.
    pub image: String,

    /// Whether its VM is up, or down with its disk kept.
    pub state: SandboxState,

    /// Whether it is deleted when it stops, rather than kept.
    pub ephemeral: bool,

    /// How long it runs unused before it stops, in minutes.
    pub auto_stop_minutes: u32,

    /// How long it is kept stopped and unused before it is deleted, in days; `None` keeps it
    /// until it is deleted.
    pub auto_delete_days: Option<u32>,

    /// When it was last used -- a job run or started in it, a connection through its ports --
    /// as RFC 3339.
    pub used_at: String,

    /// Where each port it opened is reached from, in the order asked for: in what making and
    /// starting it answer only, since the URLs carry credentials.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<SandboxPort>,
}

/// Whether a sandbox's VM is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxState {
    /// Its VM is up.
    Running,
    /// Its VM is down, and its disk kept: a job started in it, or a start, starts it again.
    Stopped,
}

/// What making a sandbox takes: `POST /sandboxes/`'s body.
///
/// The machine members (`vcpus`, `memory_mib`, `gpu`, `gpu_memory_mib`) each leave
/// it to the server without one. A server gives exactly what is asked or refuses; `0` for a
/// number is refused, not another way to leave it out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxCreateArgs {
    /// The image its root filesystem is built on.
    pub image: ImageName,

    /// A name to find it again by, unique among the sandboxes its credentials reach. One
    /// another sandbox has is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Delete it when it stops, rather than keep it.
    #[serde(default, skip_serializing_if = "is_off")]
    pub ephemeral: bool,

    /// How long it runs unused before it stops, in minutes. Without one the server picks it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_stop_minutes: Option<u32>,

    /// How long it is kept stopped and unused before it is deleted, in days: `Some(None)`
    /// keeps it until it is deleted, and `None` leaves it to the server.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "given_or_null"
    )]
    pub auto_delete_days: Option<Option<u32>>,

    /// How much it may write to its root filesystem over its image, in GiB. Without one the
    /// server picks it; `0` is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gib: Option<u32>,

    /// Whether its processes reach a network at all: the way out, which `ports` do not need.
    #[serde(default = "on", skip_serializing_if = "is_on")]
    pub network: bool,

    /// TCP ports in it to open, each answered with where it is reached from. Reserved as it is
    /// made, and kept until it is removed; `0` and a port given twice are refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,

    /// How many vCPUs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u8>,

    /// Memory, in MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,

    /// Whether it gets a GPU. `false` forbids one on a server that would attach one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu: Option<bool>,

    /// Memory the GPU may hold, in MiB, apart from `memory_mib`. Refused without a GPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_memory_mib: Option<u32>,

    /// The volumes mounted into it, in order, so a later path may be inside an earlier one.
    /// Two at one path are refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<SandboxVolume>,
}

impl SandboxCreateArgs {
    /// A sandbox of `image`, with everything else left to the server.
    pub fn new(image: impl Into<ImageName>) -> Self {
        Self {
            image: image.into(),
            name: None,
            ephemeral: false,
            auto_stop_minutes: None,
            auto_delete_days: None,
            disk_gib: None,
            network: true,
            ports: Vec::new(),
            vcpus: None,
            memory_mib: None,
            gpu: None,
            gpu_memory_mib: None,
            volumes: Vec::new(),
        }
    }
}

fn on() -> bool {
    true
}

fn is_on(flag: &bool) -> bool {
    *flag
}

fn is_off(flag: &bool) -> bool {
    !*flag
}

/// A member that is left out, given, or given as `null`, each meaning something else: its
/// outer `Option` is whether it is there, its inner whether it is `null`.
mod given_or_null {
    use super::*;

    pub fn serialize<S: Serializer>(value: &Option<Option<u32>>, to: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(given) => given.serialize(to),
            None => to.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        from: D,
    ) -> Result<Option<Option<u32>>, D::Error> {
        Option::<u32>::deserialize(from).map(Some)
    }
}

/// A TCP port opened in a sandbox, and where it is reached from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxPort {
    /// The port in the sandbox.
    pub port: u16,

    /// Where it is reached from: a loopback address from a server on the client's machine, or
    /// a URL carrying the credentials it needs from one reachable from other machines. As secret
    /// as those credentials, and the server's to form: a client does not build one or take it
    /// apart.
    pub url: String,
}

/// A volume mounted into a sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxVolume {
    /// Which volume, by name.
    pub volume: String,

    /// Where it appears to processes: absolute.
    pub path: String,

    /// Mount it read-only, so a process's write in it fails.
    #[serde(default)]
    pub readonly: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_create_says_only_what_it_asked_for() {
        let args = SandboxCreateArgs::new("img:latest");
        let value = serde_json::to_value(&args).unwrap();
        let keys: Vec<_> = value.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["image"]);

        let decoded: SandboxCreateArgs = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, args);
        assert!(decoded.network);
    }

    #[test]
    fn auto_delete_days_left_out_and_null_are_apart() {
        let mut args = SandboxCreateArgs::new("img:latest");
        args.auto_delete_days = Some(None);
        let value = serde_json::to_value(&args).unwrap();
        assert_eq!(value["auto_delete_days"], serde_json::Value::Null);
        assert!(value.as_object().unwrap().contains_key("auto_delete_days"));
        let decoded: SandboxCreateArgs = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.auto_delete_days, Some(None));

        args.auto_delete_days = Some(Some(7));
        let decoded: SandboxCreateArgs =
            serde_json::from_value(serde_json::to_value(&args).unwrap()).unwrap();
        assert_eq!(decoded.auto_delete_days, Some(Some(7)));

        let left_out: SandboxCreateArgs = serde_json::from_value(
            serde_json::json!({ "image": {"type": "tag", "tag": "img:latest"} }),
        )
        .unwrap();
        assert_eq!(left_out.auto_delete_days, None);
    }

    #[test]
    fn a_sandbox_answers_its_state_and_keeps_a_null_auto_delete() {
        let sandbox: Sandbox = serde_json::from_value(serde_json::json!({
            "id": "s1", "image": "sha256:ab", "state": "stopped", "ephemeral": false,
            "auto_stop_minutes": 15, "auto_delete_days": null, "used_at": "2026-11-04T00:00:00Z"
        }))
        .unwrap();
        assert_eq!(sandbox.state, SandboxState::Stopped);
        assert_eq!(sandbox.auto_delete_days, None);
        assert_eq!(
            serde_json::to_value(&sandbox).unwrap()["auto_delete_days"],
            serde_json::Value::Null
        );
    }
}
