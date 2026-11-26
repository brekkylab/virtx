//! Sandboxes: what one is, as the server answers it, and what making one takes.

use std::fmt;

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, MapAccess, Unexpected, Visitor, value::MapAccessDeserializer},
};

use crate::image::ImageName;

/// A sandbox, as the server answers it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sandbox {
    /// The sandbox's id, which every request about it names it by.
    pub id: String,

    /// The name it was made with, which finds it again
    /// ([`Client::find_sandbox`](crate::Client::find_sandbox)).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// The digest of the image it booted, as `algorithm:hex`.
    pub image: String,

    /// Whether it takes processes: `false` once its VM has exited on its own. One whose VM the
    /// server stopped for want of use is booted again by the next job, and is `true`; one that
    /// is `false` is still held until it is removed.
    pub running: bool,

    /// Each port it opened, in the order asked for: with its `url` in what making it answers
    /// only, since the URLs carry credentials, and without it in what asking for it answers.
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

    /// A name to find it again by, rather than keeping its id: unique among the sandboxes a
    /// listing answers, so one another sandbox has is refused, until that one is removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Whether its processes reach a network at all: the way out, which `ports` do not need.
    #[serde(default = "on", skip_serializing_if = "is_on")]
    pub network: bool,

    /// Ports in it to open, each answered with where it is reached from: a number, or one with
    /// what it is for, how it is reached and whom it lets in ([`PortSpec`]). Reserved as it is
    /// made, and kept until it is removed; `0` and a port given twice are refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortSpec>,

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
    ///     .name("dev")
    ///     .port(8000)
    ///     .readonly_volume("data", "/data")
    ///     .abin("index");
    ///
    /// assert_eq!(args.name.as_deref(), Some("dev"));
    /// assert!(args.volumes[0].readonly);
    /// assert_eq!(args.abins, ["index"]);
    /// ```
    pub fn new(image: impl Into<ImageName>) -> Self {
        Self {
            image: image.into(),
            name: None,
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

    /// Named `name`, which finds it again: letters, digits, `.`, `_` and `-`, not starting with
    /// `.` or `-`, at most 128, or it is refused `400`.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Whether its processes reach a network at all.
    pub fn network(mut self, network: bool) -> Self {
        self.network = network;
        self
    }

    /// Open the port `port` in it, beside those given before: a number for TCP, or a
    /// [`PortSpec`] to say what it is for, whether it is HTTP, and whom it lets in.
    pub fn port(mut self, port: impl Into<PortSpec>) -> Self {
        self.ports.push(port.into());
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

/// A port opened in a sandbox, and where it is reached from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPort {
    /// The port in the sandbox.
    pub port: u16,

    /// Where it is reached from: a loopback address from a server on the client's machine, or
    /// a URL carrying the credentials it needs from one reachable from other machines. As secret
    /// as those credentials, and the server's to form: a client does not build one or take it
    /// apart. Answered by the sandbox's making alone, and `None` in what asking for it answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// What it is for, as it was opened with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// A port to open in a sandbox: its number, and what it is for. Written as the number alone
/// when it says nothing more, `{port, label?}` otherwise; read as either.
///
/// ```
/// # use virtx_core::{PortSpec, SandboxCreateArgs};
/// let args = SandboxCreateArgs::new("node:22").port(PortSpec::new(5173).label("dev server"));
///
/// assert_eq!(args.ports[0].label.as_deref(), Some("dev server"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortSpec {
    /// The port in the sandbox.
    pub port: u16,

    /// What it is for, free text of up to 128 characters, answered with it.
    pub label: Option<String>,
}

impl PortSpec {
    /// The port `port`, saying nothing more.
    pub fn new(port: u16) -> PortSpec {
        PortSpec { port, label: None }
    }

    pub fn label(mut self, label: impl Into<String>) -> PortSpec {
        self.label = Some(label.into());
        self
    }
}

impl From<u16> for PortSpec {
    fn from(port: u16) -> PortSpec {
        PortSpec::new(port)
    }
}

/// [`PortSpec`] as an object.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PortSpecObject {
    port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
}

impl Serialize for PortSpec {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.label.is_none() {
            return serializer.serialize_u16(self.port);
        }
        PortSpecObject {
            port: self.port,
            label: self.label.clone(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for PortSpec {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Told apart by what was sent, so a failure inside the object is told as itself, at its
        // member, rather than as neither form.
        struct Either;

        impl<'de> Visitor<'de> for Either {
            type Value = PortSpec;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(r#"a whole number from 0 to 65535, or {"port", "label"?}"#)
            }

            fn visit_u64<E: de::Error>(self, port: u64) -> Result<PortSpec, E> {
                u16::try_from(port)
                    .map(PortSpec::new)
                    .map_err(|_| E::invalid_value(Unexpected::Unsigned(port), &self))
            }

            fn visit_i64<E: de::Error>(self, port: i64) -> Result<PortSpec, E> {
                match u64::try_from(port) {
                    Ok(port) => self.visit_u64(port),
                    Err(_) => Err(E::invalid_value(Unexpected::Signed(port), &self)),
                }
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<PortSpec, A::Error> {
                let spec = PortSpecObject::deserialize(MapAccessDeserializer::new(map))?;
                Ok(PortSpec {
                    port: spec.port,
                    label: spec.label,
                })
            }
        }

        deserializer.deserialize_any(Either)
    }
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
            "ports": [{"port": 80, "url": "http://127.0.0.1:1/", "region": "r1"}],
            "cell": "c1",
        });
        let sandbox: Sandbox = serde_json::from_value(value).unwrap();
        assert_eq!(sandbox.name, None);
        assert_eq!(
            sandbox.ports,
            [SandboxPort {
                port: 80,
                url: Some("http://127.0.0.1:1/".into()),
                label: None,
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
            .name("work")
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
        expected.name = Some("work".into());
        expected.network = false;
        expected.ports = vec![80.into(), 443.into()];
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

    #[test]
    fn a_port_is_written_as_its_number_unless_it_says_more() {
        let plain = PortSpec::new(8080);
        assert_eq!(serde_json::to_value(&plain).unwrap(), json!(8080));
        let labelled = PortSpec::new(5173).label("dev server");
        let written = serde_json::to_value(&labelled).unwrap();
        assert_eq!(written, json!({"port": 5173, "label": "dev server"}));
        let read: Vec<PortSpec> =
            serde_json::from_value(json!([8080, written, {"port": 22}])).unwrap();
        assert_eq!(read, [plain, labelled, PortSpec::new(22)]);
        assert!(serde_json::from_value::<PortSpec>(json!({"port": 1, "x": 2})).is_err());
    }

    #[test]
    fn a_port_that_is_neither_form_is_refused_saying_what_a_port_is() {
        let refused = serde_json::from_value::<PortSpec>(json!("5900")).unwrap_err();
        assert_eq!(
            refused.to_string(),
            r#"invalid type: string "5900", expected a whole number from 0 to 65535, or {"port", "label"?}"#
        );
    }

    #[test]
    fn a_port_asked_for_with_a_way_in_other_than_its_url_is_refused() {
        for asked in [
            json!({"port": 5173, "protocol": "http"}),
            json!({"port": 22, "inbound": [{"source": "203.0.113.7"}]}),
        ] {
            assert!(serde_json::from_value::<PortSpec>(asked).is_err());
        }
    }

    #[test]
    fn a_port_asked_for_again_is_read_without_its_url() {
        let asked = json!({"port": 5173, "label": "dev server"});
        let port: SandboxPort = serde_json::from_value(asked.clone()).unwrap();
        assert_eq!(port.url, None);
        assert_eq!(port.label.as_deref(), Some("dev server"));
        assert_eq!(serde_json::to_value(&port).unwrap(), asked);
    }

    #[test]
    fn a_sandbox_named_says_its_name_and_one_unnamed_says_none() {
        let named: Sandbox = serde_json::from_value(
            json!({"id": "s", "name": "work", "image": "sha256:00", "running": true}),
        )
        .unwrap();
        assert_eq!(named.name.as_deref(), Some("work"));
        let value = serde_json::to_value(&named).unwrap();
        assert_eq!(value["name"], "work");
        let unnamed = Sandbox {
            name: None,
            ..named
        };
        let value = serde_json::to_value(&unnamed).unwrap();
        assert!(value.get("name").is_none(), "{value}");
    }
}
