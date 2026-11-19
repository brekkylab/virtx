//! Sandboxes: what one is, as the server answers it, and what making one takes.

use serde::{Deserialize, Serialize};

use crate::image::ImageName;

/// A sandbox, as the server answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sandbox {
    /// The sandbox's id, which every request about it names it by.
    pub id: String,

    /// The digest of the image it booted, as `algorithm:hex`.
    pub image: String,

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

    /// Why it takes nothing but its `DELETE`, for one a server could not know again after it
    /// restarted: not `running`, and every other request about it refused `409`, `ended`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What making a sandbox takes: `POST /sandboxes/`'s body.
///
/// The machine members (`vcpus`, `memory_mib`, `disk_gib`, `gpu`, `gpu_memory_mib`) each leave
/// it to the server without one. A server gives exactly what is asked or refuses; `0` for a
/// number is refused, not another way to leave it out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxCreateArgs {
    /// The image its root filesystem is built on.
    pub image: ImageName,

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

    /// How much it may write over its image, in GiB, at most 1024: the size of its writable
    /// layer, which goes with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gib: Option<u32>,

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
    ///     .port(8000)
    ///     .readonly_volume("data", "/data")
    ///     .abin("index");
    ///
    /// assert!(args.volumes[0].readonly);
    /// assert_eq!(args.abins, ["index"]);
    /// ```
    pub fn new(image: impl Into<ImageName>) -> Self {
        Self {
            image: image.into(),
            network: true,
            ports: Vec::new(),
            vcpus: None,
            memory_mib: None,
            disk_gib: None,
            gpu: None,
            gpu_memory_mib: None,
            volumes: Vec::new(),
            abins: Vec::new(),
        }
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

    pub fn disk_gib(mut self, gib: u32) -> Self {
        self.disk_gib = Some(gib);
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

/// What resizing a sandbox takes: `PATCH /sandboxes/{id}`'s body, each member optional and left
/// as it is without one. Its vCPUs and memory are those its VM boots with from its next boot on;
/// its disk only grows, to as large as its VM next boots with.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxResizeArgs {
    /// How many vCPUs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u8>,

    /// Memory, in MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,

    /// How much it may write over its image, in GiB, at most 1024: grown, never shrunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_gib: Option<u32>,
}

impl SandboxResizeArgs {
    /// Each method sets one field, so they chain from [`default`](Self::default).
    ///
    /// ```
    /// # use virtx_core::SandboxResizeArgs;
    /// let args = SandboxResizeArgs::default().vcpus(2).disk_gib(8);
    ///
    /// assert_eq!((args.vcpus, args.memory_mib, args.disk_gib), (Some(2), None, Some(8)));
    /// ```
    pub fn vcpus(mut self, vcpus: u8) -> Self {
        self.vcpus = Some(vcpus);
        self
    }

    pub fn memory_mib(mut self, mib: u32) -> Self {
        self.memory_mib = Some(mib);
        self
    }

    pub fn disk_gib(mut self, gib: u32) -> Self {
        self.disk_gib = Some(gib);
        self
    }
}

/// A sandbox's size, as resizing it answers: what its VM boots with from its next boot on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxResources {
    pub vcpus: u8,

    /// Memory, in MiB.
    pub memory_mib: u32,

    /// How much it may write over its image, in GiB.
    pub disk_gib: u32,

    /// Whether its VM is up now, the size it booted with until it boots again.
    pub up: bool,
}

fn on() -> bool {
    true
}

fn is_on(flag: &bool) -> bool {
    *flag
}

/// A TCP port opened in a sandbox, and where it is reached from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    use serde_json::json;

    use super::*;

    #[test]
    fn a_sandbox_is_read_past_members_it_does_not_know() {
        let value = json!({
            "id": "s",
            "image": "sha256:00",
            "running": true,
            "ports": [{"port": 80, "url": "http://127.0.0.1:1/", "address": "10.0.0.2:80"}],
            "cell": "c1",
        });
        let sandbox: Sandbox = serde_json::from_value(value).unwrap();
        assert_eq!(
            sandbox.ports,
            [SandboxPort {
                port: 80,
                url: "http://127.0.0.1:1/".into(),
            }]
        );
    }

    #[test]
    fn a_create_with_a_member_it_does_not_take_is_refused() {
        let mut value = serde_json::to_value(SandboxCreateArgs::new("img:latest")).unwrap();
        value["cell"] = json!("c1");
        assert!(serde_json::from_value::<SandboxCreateArgs>(value).is_err());

        let mut value =
            serde_json::to_value(SandboxCreateArgs::new("img:latest").volume("a", "/a")).unwrap();
        value["volumes"][0]["cell"] = json!("c1");
        assert!(serde_json::from_value::<SandboxCreateArgs>(value).is_err());
    }

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
    fn a_resize_says_only_what_it_asked_for_and_takes_no_other_member() {
        let args = SandboxResizeArgs::default().memory_mib(512);
        let value = serde_json::to_value(&args).unwrap();
        assert_eq!(value, json!({ "memory_mib": 512 }));
        assert_eq!(
            serde_json::from_value::<SandboxResizeArgs>(value).unwrap(),
            args
        );
        let other = json!({ "cpus": 2 });
        assert!(serde_json::from_value::<SandboxResizeArgs>(other).is_err());
    }

    #[test]
    fn each_method_sets_its_field() {
        let args = SandboxCreateArgs::new("img:latest")
            .network(false)
            .port(80)
            .port(443)
            .vcpus(2)
            .memory_mib(512)
            .disk_gib(8)
            .gpu(true)
            .gpu_memory_mib(256)
            .volume("a", "/a")
            .readonly_volume("b", "/a/b");
        let mut expected = SandboxCreateArgs::new("img:latest");
        expected.network = false;
        expected.ports = vec![80, 443];
        expected.vcpus = Some(2);
        expected.memory_mib = Some(512);
        expected.disk_gib = Some(8);
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
