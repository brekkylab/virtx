//! Sandboxes: what one is, as the server answers it, and what making one takes.

use serde::{Deserialize, Serialize};

use crate::image::ImageName;

/// A sandbox, as the server answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sandbox {
    /// The sandbox's id, which every request about it names it by.
    pub id: String,

    /// The digest of the image it booted, as `algorithm:hex`.
    pub image: String,

    /// The snapshot it stands on, by name, if it was given one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,

    /// Whether it takes processes: `false` once its VM has exited on its own. One whose VM the
    /// server stopped for want of use is booted again by the next job, and is `true`; one that
    /// is `false` is still held until it is removed.
    pub running: bool,

    /// Where each port it opened is reached from, in the order asked for: in what making it
    /// answers only, since the URLs carry credentials.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<SandboxPort>,

    /// virtx's own programs it has at `/abin`, as it was made with them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub abins: Vec<String>,
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

    /// The snapshot, by name, that is its root filesystem's upper layer: it starts with the
    /// changes in it, and its writes go into it. Without one its writes go when it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,

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

    /// virtx's own programs it gets at `/abin`, by name, each with its `SKILL.md` at
    /// `/abin/skills/{name}/`. Without any it has no `/abin`; a name the server has no program
    /// by is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub abins: Vec<String>,
}

impl SandboxCreateArgs {
    /// A sandbox of `image`, with everything else left to the server. Each method after sets
    /// one field, so they chain.
    ///
    /// ```
    /// # use virtx_core::SandboxCreateArgs;
    /// let args = SandboxCreateArgs::new("jq:latest")
    ///     .snapshot("dev")
    ///     .port(8000)
    ///     .readonly_volume("data", "/data")
    ///     .abin("index");
    ///
    /// assert_eq!(args.snapshot.as_deref(), Some("dev"));
    /// assert!(args.volumes[0].readonly);
    /// assert_eq!(args.abins, ["index"]);
    /// ```
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
            abins: Vec::new(),
        }
    }

    /// Stand on the snapshot `name`.
    pub fn snapshot(mut self, name: impl Into<String>) -> Self {
        self.snapshot = Some(name.into());
        self
    }

    /// Whether its processes reach a network at all.
    pub fn network(mut self, network: bool) -> Self {
        self.network = network;
        self
    }

    /// Open the TCP port `port` in it, beside those given before.
    pub fn port(mut self, port: u16) -> Self {
        self.ports.push(port);
        self
    }

    pub fn vcpus(mut self, vcpus: u8) -> Self {
        self.vcpus = Some(vcpus);
        self
    }

    pub fn memory_mib(mut self, mib: u32) -> Self {
        self.memory_mib = Some(mib);
        self
    }

    /// Whether it gets a GPU.
    pub fn gpu(mut self, gpu: bool) -> Self {
        self.gpu = Some(gpu);
        self
    }

    pub fn gpu_memory_mib(mut self, mib: u32) -> Self {
        self.gpu_memory_mib = Some(mib);
        self
    }

    /// Mount the volume `name` at `path` in it, after those given before.
    pub fn volume(mut self, name: impl Into<String>, path: impl Into<String>) -> Self {
        self.volumes.push(SandboxVolume {
            volume: name.into(),
            path: path.into(),
            readonly: false,
        });
        self
    }

    /// Mount the volume `name` read-only at `path` in it, as [`volume`](Self::volume) does.
    pub fn readonly_volume(self, name: impl Into<String>, path: impl Into<String>) -> Self {
        let mut args = self.volume(name, path);
        args.volumes.last_mut().unwrap().readonly = true;
        args
    }

    /// Give it virtx's program `name` at `/abin`, beside those given before.
    pub fn abin(mut self, name: impl Into<String>) -> Self {
        self.abins.push(name.into());
        self
    }
}

fn on() -> bool {
    true
}

fn is_on(flag: &bool) -> bool {
    *flag
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
    fn each_method_sets_its_field() {
        let args = SandboxCreateArgs::new("img:latest")
            .snapshot("s")
            .network(false)
            .port(80)
            .port(443)
            .vcpus(2)
            .memory_mib(512)
            .gpu(true)
            .gpu_memory_mib(256)
            .volume("a", "/a")
            .readonly_volume("b", "/a/b");
        let mut expected = SandboxCreateArgs::new("img:latest");
        expected.snapshot = Some("s".into());
        expected.network = false;
        expected.ports = vec![80, 443];
        expected.vcpus = Some(2);
        expected.memory_mib = Some(512);
        expected.gpu = Some(true);
        expected.gpu_memory_mib = Some(256);
        expected.volumes = vec![
            SandboxVolume {
                volume: "a".into(),
                path: "/a".into(),
                readonly: false,
            },
            SandboxVolume {
                volume: "b".into(),
                path: "/a/b".into(),
                readonly: true,
            },
        ];
        assert_eq!(args, expected);
    }
}
