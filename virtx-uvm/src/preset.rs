//! Presets: a sandbox kept under a name in a project, for its server to make as many times as
//! it is asked for. The store keeps each as it was put, checked as a sandbox's making checks
//! what it can without making one; what a preset lets in, and the sandboxes made of it, are
//! its server's.
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   preset/<name>.json        a preset, as it is answered
//!   preset/<name>.json.part   one being written, renamed over it once whole
//! ```

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use virtx_core::{ImageName, Preset, PresetAccess, PresetPutArgs, SandboxResizeArgs};

use crate::{
    DEFAULT_DISK_GIB, DEFAULT_MEMORY_MIB, DEFAULT_VCPUS, Reference, Sandbox, SandboxError,
};

const PRESET: &str = "preset";

/// The presets of one project, kept under its root.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
    /// Held while presets are changed, or read by what must see them hold still
    /// ([`Store::steady`]).
    changing: Arc<Mutex<()>>,
}

/// The presets held still, as [`Store::steady`] answers: none is put or removed until it drops.
pub struct Steady<'a> {
    store: &'a Store,
    _held: MutexGuard<'a, ()>,
}

impl Store {
    /// The presets kept under `root`.
    pub fn new(root: &Path) -> io::Result<Store> {
        let dir = root.join(PRESET);
        fs::create_dir_all(&dir)?;
        Ok(Store {
            dir,
            changing: Arc::default(),
        })
    }

    /// Every preset here, by name.
    pub fn list(&self) -> Result<Vec<Preset>, SandboxError> {
        let mut presets = Vec::new();
        for entry in fs::read_dir(&self.dir).map_err(|e| io_error(&self.dir, e))? {
            let path = entry.map_err(|e| io_error(&self.dir, e))?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                presets.push(read(&path)?);
            }
        }
        presets.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(presets)
    }

    /// The preset `named` names: by its id, or by its name.
    pub fn get(&self, named: &str) -> Result<Option<Preset>, SandboxError> {
        if crate::is_name(named) {
            let path = self.path(named);
            if path.exists() {
                return read(&path).map(Some);
            }
        }
        Ok(self.list()?.into_iter().find(|preset| preset.id == named))
    }

    /// Keep `args` as preset `name`, made under the id `id` gives where there is none of that
    /// name, or replacing it in whole and keeping its id: answered with whether it was made.
    /// Refused [`Invalid`](SandboxError::Invalid) for a name that is none, and for `args` no
    /// sandbox could be made of whatever is here ([`Sandbox::check`], with `vm` the program its
    /// machines run as), or whose `max`, `access` or `origins` are no such thing. Whether its
    /// image and volumes are here is the caller's to ask.
    pub fn put(
        &self,
        name: &str,
        args: PresetPutArgs,
        vm: &[OsString],
        id: impl FnOnce() -> String,
    ) -> Result<(Preset, bool), SandboxError> {
        if !crate::is_name(name) {
            return Err(SandboxError::Invalid(format!(
                "{name:?} is not a preset's name: letters, digits, '.', '_' and '-', not \
                 starting with '.' or '-', 128 at most"
            )));
        }
        check(&args, vm)?;
        let _held = self.changing.lock().unwrap();
        let path = self.path(name);
        let (id, made) = match path.exists() {
            true => (read(&path)?.id, false),
            false => (id(), true),
        };
        let preset = Preset {
            id,
            name: name.to_owned(),
            sandbox: args.sandbox,
            max: args.max,
            access: args.access,
            origins: args.origins,
            limits: args.limits,
        };
        let part = self.dir.join(format!("{name}.json.part"));
        let json = serde_json::to_vec_pretty(&preset).expect("a preset is JSON");
        fs::write(&part, json).map_err(|e| io_error(&part, e))?;
        fs::rename(&part, &path).map_err(|e| io_error(&path, e))?;
        Ok((preset, made))
    }

    /// Remove the preset `named` names, by its id or its name: `false` where there is none.
    pub fn remove(&self, named: &str) -> Result<bool, SandboxError> {
        let _held = self.changing.lock().unwrap();
        let Some(preset) = self.get(named)? else {
            return Ok(false);
        };
        let path = self.path(&preset.name);
        fs::remove_file(&path).map_err(|e| io_error(&path, e))?;
        Ok(true)
    }

    /// The presets held still until what this answers drops: what refuses to take away what a
    /// preset names asks it, so that no preset comes to name it meanwhile.
    pub fn steady(&self) -> Steady<'_> {
        Steady {
            store: self,
            _held: self.changing.lock().unwrap(),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }
}

impl Steady<'_> {
    /// The name of a preset that mounts volume `volume`, if one does.
    pub fn naming_volume(&self, volume: &str) -> Result<Option<String>, SandboxError> {
        Ok((self.store.list()?.into_iter())
            .find(|preset| (preset.sandbox.volumes.iter()).any(|asked| asked.volume == volume))
            .map(|preset| preset.name))
    }

    /// The name of a preset whose image is tag `tag`, if one's is.
    pub fn naming_tag(&self, tag: &Reference) -> Result<Option<String>, SandboxError> {
        Ok((self.store.list()?.into_iter())
            .find(|preset| match &preset.sandbox.image {
                ImageName::Tag { tag: named } => {
                    named.parse::<Reference>().ok().as_ref() == Some(tag)
                }
                ImageName::Digest { .. } => false,
            })
            .map(|preset| preset.name))
    }
}

/// Refuse `args` where no preset could be made of them, whatever is here.
fn check(args: &PresetPutArgs, vm: &[OsString]) -> Result<(), SandboxError> {
    let invalid = |why: String| Err(SandboxError::Invalid(why));
    if args.sandbox.name.is_some() {
        return invalid("sandbox: name: each sandbox of a preset is named as it is made".into());
    }
    Sandbox::check(&args.sandbox, vm).map_err(|e| match e {
        SandboxError::Invalid(why) => SandboxError::Invalid(format!("sandbox: {why}")),
        e => e,
    })?;
    if let Some(max) = &args.max {
        let made = &args.sandbox;
        let under = |name: &str, max: Option<u32>, made: Option<u32>, default: u32| match max {
            Some(max) if max < made.unwrap_or(default) => Some(format!(
                "max: {name}: {max} is less than the {} a sandbox is made with",
                made.unwrap_or(default)
            )),
            _ => None,
        };
        let SandboxResizeArgs {
            vcpus,
            memory_mib,
            disk_gib,
        } = max;
        let why = (under(
            "vcpus",
            vcpus.map(u32::from),
            made.vcpus.map(u32::from),
            DEFAULT_VCPUS.into(),
        ))
        .or_else(|| {
            under(
                "memory_mib",
                *memory_mib,
                made.memory_mib,
                DEFAULT_MEMORY_MIB,
            )
        })
        .or_else(|| under("disk_gib", *disk_gib, made.disk_gib, DEFAULT_DISK_GIB));
        if let Some(why) = why {
            return invalid(why);
        }
    }
    for access in &args.access {
        if let PresetAccess::Jwt { issuer, jwks, .. } = access {
            for (name, url) in [("issuer", Some(issuer)), ("jwks", jwks.as_ref())] {
                if let Some(url) = url
                    && !is_https(url)
                {
                    return invalid(format!("access: {name}: {url:?} is no https URL"));
                }
            }
        }
    }
    if let Some(origin) = args.origins.iter().find(|origin| !is_origin(origin)) {
        return invalid(format!(
            "origins: {origin:?} is no origin: a scheme, a host and a port, as https://app.example"
        ));
    }
    Ok(())
}

/// Whether `url` is an `https` URL with a host, or an `http` one to this host's loopback.
fn is_https(url: &str) -> bool {
    let authority = |rest: &str| {
        rest.split(['/', '?', '#'])
            .next()
            .unwrap_or_default()
            .to_owned()
    };
    if let Some(rest) = url.strip_prefix("https://") {
        return !authority(rest).is_empty();
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let authority = authority(rest);
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => host,
        _ => &authority,
    };
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

/// Whether `origin` is one as `Origin` sends it: `http` or `https`, a host, and a port where it
/// says one, with nothing after.
fn is_origin(origin: &str) -> bool {
    let Some(host) = origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
    else {
        return false;
    };
    !host.is_empty()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
}

fn read(path: &Path) -> Result<Preset, SandboxError> {
    let json = fs::read(path).map_err(|e| io_error(path, e))?;
    serde_json::from_slice(&json).map_err(|e| io_error(path, e.into()))
}

fn io_error(path: &Path, source: io::Error) -> SandboxError {
    SandboxError::Io {
        context: format!("reading or writing {}", path.display()),
        source,
    }
}

#[cfg(test)]
mod tests {
    use virtx_core::{SandboxCreateArgs, SandboxResizeArgs};

    use super::*;

    fn args(image: &str) -> PresetPutArgs {
        PresetPutArgs::new(SandboxCreateArgs::new(image))
    }

    #[test]
    fn a_preset_is_put_found_by_its_id_or_name_replaced_keeping_its_id_and_removed() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path()).unwrap();
        let (made, new) = store
            .put("app", args("app:1"), &[], || "p1".into())
            .unwrap();
        assert!(new);
        assert_eq!((made.id.as_str(), made.name.as_str()), ("p1", "app"));
        assert_eq!(store.get("app").unwrap(), Some(made.clone()));
        assert_eq!(store.get("p1").unwrap(), Some(made));

        let (replaced, new) = store
            .put("app", args("app:2"), &[], || unreachable!())
            .unwrap();
        assert!(!new);
        assert_eq!(replaced.id, "p1");
        assert_eq!(replaced.sandbox.image, ImageName::from("app:2"));
        store
            .put("other", args("app:1"), &[], || "p2".into())
            .unwrap();
        let names: Vec<_> = store.list().unwrap().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["app", "other"]);

        assert!(store.remove("p1").unwrap());
        assert!(!store.remove("app").unwrap());
        assert_eq!(store.get("app").unwrap(), None);
    }

    #[test]
    fn a_preset_no_sandbox_could_be_made_of_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path()).unwrap();
        let refused =
            |name: &str, args: PresetPutArgs| match store.put(name, args, &[], || "p".into()) {
                Err(SandboxError::Invalid(why)) => why,
                answered => panic!("{answered:?}"),
            };
        assert!(refused("../app", args("app:1")).contains("not a preset's name"));
        let named = PresetPutArgs::new(SandboxCreateArgs::new("app:1").name("dev"));
        assert!(refused("app", named).starts_with("sandbox: name:"));
        let forever = PresetPutArgs::new(SandboxCreateArgs::new("app:1").lifetime(0));
        assert!(refused("app", forever).starts_with("sandbox: lifetime:"));
        let larger = args("app:1").max(SandboxResizeArgs::default().vcpus(DEFAULT_VCPUS + 1));
        let made = PresetPutArgs::new(SandboxCreateArgs::new("app:1").vcpus(2));
        assert!(
            refused("app", made.max(SandboxResizeArgs::default().vcpus(1)))
                .starts_with("max: vcpus")
        );
        store.put("app", larger, &[], || "p".into()).unwrap();
        let plain = PresetAccess::Jwt {
            issuer: "http://idp.example".into(),
            audience: "app".into(),
            jwks: None,
            claims: Default::default(),
        };
        assert!(refused("app", args("app:1").access(plain)).starts_with("access: issuer"));
        let local = PresetAccess::Jwt {
            issuer: "https://idp.example".into(),
            audience: "app".into(),
            jwks: Some("http://127.0.0.1:8080/keys".into()),
            claims: Default::default(),
        };
        store
            .put("app", args("app:1").access(local), &[], || "p".into())
            .unwrap();
        assert!(refused("app", args("app:1").origin("app.example")).starts_with("origins:"));
        assert!(
            refused("app", args("app:1").origin("https://app.example/")).starts_with("origins:")
        );
    }

    #[test]
    fn what_a_preset_names_is_found_while_the_presets_hold_still() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path()).unwrap();
        let mounting = SandboxCreateArgs::new("app:1").volume("data", "/data");
        store
            .put("app", PresetPutArgs::new(mounting), &[], || "p".into())
            .unwrap();
        let steady = store.steady();
        assert_eq!(
            steady.naming_volume("data").unwrap().as_deref(),
            Some("app")
        );
        assert_eq!(steady.naming_volume("other").unwrap(), None);
        let tag = |tag: &str| tag.parse::<Reference>().unwrap();
        assert_eq!(
            steady.naming_tag(&tag("app:1")).unwrap().as_deref(),
            Some("app")
        );
        assert_eq!(
            steady
                .naming_tag(&tag("docker.io/library/app:1"))
                .unwrap()
                .as_deref(),
            Some("app")
        );
        assert_eq!(steady.naming_tag(&tag("app:2")).unwrap(), None);
    }
}
