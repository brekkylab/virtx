//! Sandboxes: a VM booted from an image, standing on a snapshot, with volumes mounted into it,
//! and the commands run in one. Its files are read and written through the volumes it mounts.

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::{image::ImageName, method::Request};

/// `sandbox.list`: every sandbox the server holds, up to its `sandbox.close`.
pub struct SandboxList;

impl Request for SandboxList {
    const NAME: &'static str = "sandbox.list";
    type Args = SandboxListArgs;
    type Output = Vec<SandboxListOutputItem>;
}

/// What `sandbox.list` takes: nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxListArgs {}

/// One sandbox in what `sandbox.list` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxListOutputItem {
    /// The sandbox's id.
    pub id: String,

    /// The digest of the image it was made from, as `algorithm:hex`.
    pub image: String,

    /// The snapshot it stands on, by name, if it was given one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
}

/// `sandbox.init`: make a sandbox, and answer the id later calls name it by.
pub struct SandboxInit;

impl Request for SandboxInit {
    const NAME: &'static str = "sandbox.init";
    type Args = SandboxInitArgs;
    type Output = SandboxInitOutput;
}

/// What `sandbox.init` takes.
///
/// The machine members (`vcpus`, `memory_mib`, `gpu`, `gpu_memory_mib`) each leave
/// it to the server without one. A server gives exactly what is asked or refuses; `0` for a
/// number is refused, not another way to leave it out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxInitArgs {
    /// The image its root filesystem is built on.
    pub image: ImageName,

    /// The snapshot, by name, that is its root filesystem's upper layer: it starts with the
    /// changes in it, and its writes go into it. Without one its writes go when it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,

    /// Whether its commands reach a network at all.
    #[serde(default = "on", skip_serializing_if = "is_on")]
    pub network: bool,

    /// Ports on the server's loopback that lead into it, TCP only. Refused with the network
    /// off.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<SandboxPort>,

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

impl SandboxInitArgs {
    /// A sandbox of `image`, with everything else left to the server.
    pub fn new(image: impl Into<ImageName>) -> Self {
        Self {
            image: image.into(),
            snapshot: None,
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

fn is_on(network: &bool) -> bool {
    *network
}

/// A port on the server's loopback that leads into a sandbox. Port `0` on either side is
/// refused, and so is a host port already taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxPort {
    /// The port on the server's loopback a connection is made to.
    pub host: u16,

    /// The port in the sandbox it reaches.
    pub guest: u16,
}

/// A volume mounted into a sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxVolume {
    /// Which volume, by name.
    pub volume: String,

    /// Where it appears to commands: absolute.
    pub path: String,

    /// Mount it read-only, so a command's write in it fails.
    #[serde(default)]
    pub readonly: bool,
}

/// What `sandbox.init` answers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxInitOutput {
    /// The sandbox's id, which `sandbox.exec` names it by.
    pub id: String,

    /// Its starting working directory: the image's, or the server's default without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// `sandbox.exec`: run a command in a sandbox to its end.
pub struct SandboxExec;

impl Request for SandboxExec {
    const NAME: &'static str = "sandbox.exec";
    type Args = SandboxExecArgs;
    type Output = SandboxExecOutput;
}

/// What `sandbox.exec` takes. It runs in the sandbox's current directory, with no stdin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxExecArgs {
    /// Which sandbox, by id.
    pub sandbox: String,

    /// The argv, with no shell: `["sh", "-c", "..."]` for one. An empty one is refused.
    pub cmd: Vec<String>,

    /// Milliseconds before it is killed, with no grace period. Without it, no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

/// What `sandbox.exec` answers: everything the command wrote, and how it ended. A command
/// killed at its timeout is an error instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxExecOutput {
    /// Its exit code; `128 + signal` for one killed by a signal.
    pub code: i32,

    pub stdout: Bytes,

    /// Apart from `stdout`, with no order kept between them; `2>&1` keeps it.
    pub stderr: Bytes,

    /// The output outgrew one answer, and what is here is its beginning.
    #[serde(default)]
    pub truncated: bool,
}

/// `sandbox.close`: stop a sandbox and forget its id. Its snapshot and volumes are kept.
pub struct SandboxClose;

impl Request for SandboxClose {
    const NAME: &'static str = "sandbox.close";
    type Args = SandboxCloseArgs;
    type Output = SandboxCloseOutput;
}

/// What `sandbox.close` takes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxCloseArgs {
    /// Which sandbox, by id.
    pub sandbox: String,
}

/// What `sandbox.close` answers: nothing, once the sandbox is gone.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxCloseOutput {}

#[cfg(test)]
mod tests {
    use super::*;

    fn encode(value: &impl Serialize) -> ciborium::Value {
        let mut encoded = Vec::new();
        ciborium::into_writer(value, &mut encoded).unwrap();
        ciborium::from_reader(&encoded[..]).unwrap()
    }

    #[test]
    fn an_init_says_only_what_it_asked_for() {
        let args = SandboxInitArgs::new("img:latest");
        let value = encode(&args);
        let keys: Vec<_> = value
            .as_map()
            .unwrap()
            .iter()
            .map(|(k, _)| k.as_text().unwrap())
            .collect();
        assert_eq!(keys, ["image"]);

        let mut encoded = Vec::new();
        ciborium::into_writer(&args, &mut encoded).unwrap();
        let decoded: SandboxInitArgs = ciborium::from_reader(&encoded[..]).unwrap();
        assert_eq!(decoded, args);
        assert!(decoded.network);
    }

    #[test]
    fn exec_output_is_a_cbor_byte_string() {
        let output = SandboxExecOutput {
            code: 0,
            stdout: Bytes::from_static(b"hi"),
            stderr: Bytes::new(),
            truncated: false,
        };
        let value = encode(&output);
        let stdout = value
            .as_map()
            .unwrap()
            .iter()
            .find(|(k, _)| k.as_text() == Some("stdout"))
            .map(|(_, v)| v);
        assert_eq!(stdout, Some(&ciborium::Value::Bytes(b"hi".to_vec())));
    }
}
