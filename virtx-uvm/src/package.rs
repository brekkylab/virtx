//! Packages: a sandbox kept under a name in a project, for its server to make as many times as
//! it is asked for. The store keeps each as it was put, checked as a sandbox's making checks
//! what it can without making one; what a package lets in, and the sandboxes made of it, are
//! its server's.
//!
//! # Layout
//!
//! ```text
//! <root>/
//!   package/<name>.json        a package, as it is answered
//!   package/<name>.json.part   one being written, renamed over it once whole
//! ```

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use virtx_core::{
    ImageName, Package, PackageAccess, PackagePutArgs, PackageSandboxCreateArgs, PortSide,
    SandboxCreateArgs, SandboxResizeArgs,
};

use crate::{
    DEFAULT_DISK_GIB, DEFAULT_MEMORY_MIB, DEFAULT_VCPUS, Reference, Sandbox, SandboxError,
};

const PACKAGE: &str = "package";

/// The packages of one project, kept under its root.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
    /// Held while packages are changed, or read by what must see them hold still
    /// ([`Store::steady`]).
    changing: Arc<Mutex<()>>,
}

/// The packages held still, as [`Store::steady`] answers: none is put or removed until it drops.
pub struct Steady<'a> {
    store: &'a Store,
    _held: MutexGuard<'a, ()>,
}

impl Store {
    /// The packages kept under `root`.
    pub fn new(root: &Path) -> io::Result<Store> {
        let dir = root.join(PACKAGE);
        fs::create_dir_all(&dir)?;
        Ok(Store {
            dir,
            changing: Arc::default(),
        })
    }

    /// Every package here, by name.
    pub fn list(&self) -> Result<Vec<Package>, SandboxError> {
        let mut packages = Vec::new();
        for entry in fs::read_dir(&self.dir).map_err(|e| io_error(&self.dir, e))? {
            let path = entry.map_err(|e| io_error(&self.dir, e))?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                packages.push(read(&path)?);
            }
        }
        packages.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(packages)
    }

    /// The package `named` names: by its id, or by its name.
    pub fn get(&self, named: &str) -> Result<Option<Package>, SandboxError> {
        if crate::is_name(named) {
            let path = self.path(named);
            if path.exists() {
                return read(&path).map(Some);
            }
        }
        Ok(self.list()?.into_iter().find(|package| package.id == named))
    }

    /// Keep `args` as package `name`, made under the id `id` gives where there is none of that
    /// name, or replacing it in whole and keeping its id: answered with whether it was made.
    /// Refused [`Invalid`](SandboxError::Invalid) for a name that is none, and for `args` no
    /// sandbox could be made of whatever is here ([`Sandbox::check`], with `vm` the program its
    /// machines run as), or whose `max`, `access` or `origins` are no such thing. Whether its
    /// image and volumes are here is the caller's to ask.
    pub fn put(
        &self,
        name: &str,
        args: PackagePutArgs,
        vm: &[OsString],
        id: impl FnOnce() -> String,
    ) -> Result<(Package, bool), SandboxError> {
        if !crate::is_name(name) {
            return Err(SandboxError::Invalid(format!(
                "{name:?} is not a package's name: letters, digits, '.', '_' and '-', not \
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
        let package = Package {
            id,
            name: name.to_owned(),
            sandbox: args.sandbox,
            max: args.max,
            access: args.access,
            origins: args.origins,
            limits: args.limits,
        };
        let part = self.dir.join(format!("{name}.json.part"));
        let json = serde_json::to_vec_pretty(&package).expect("a package is JSON");
        fs::write(&part, json).map_err(|e| io_error(&part, e))?;
        fs::rename(&part, &path).map_err(|e| io_error(&path, e))?;
        Ok((package, made))
    }

    /// Remove the package `named` names, by its id or its name: `false` where there is none.
    pub fn remove(&self, named: &str) -> Result<bool, SandboxError> {
        let _held = self.changing.lock().unwrap();
        let Some(package) = self.get(named)? else {
            return Ok(false);
        };
        let path = self.path(&package.name);
        fs::remove_file(&path).map_err(|e| io_error(&path, e))?;
        Ok(true)
    }

    /// The packages held still until what this answers drops: what refuses to take away what a
    /// package names asks it, so that no package comes to name it meanwhile.
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
    /// The name of a package that mounts volume `volume`, if one does.
    pub fn naming_volume(&self, volume: &str) -> Result<Option<String>, SandboxError> {
        Ok((self.store.list()?.into_iter())
            .find(|package| (package.sandbox.volumes.iter()).any(|asked| asked.volume == volume))
            .map(|package| package.name))
    }

    /// The name of a package whose image is tag `tag`, if one's is.
    pub fn naming_tag(&self, tag: &Reference) -> Result<Option<String>, SandboxError> {
        Ok((self.store.list()?.into_iter())
            .find(|package| match &package.sandbox.image {
                ImageName::Tag { tag: named } => {
                    named.parse::<Reference>().ok().as_ref() == Some(tag)
                }
                ImageName::Digest { .. } => false,
            })
            .map(|package| package.name))
    }
}

/// What a sandbox made of `package` as `args` asks is made with: the package's sandbox, named
/// `args`' name, with `args`' ports on local beside its own. Refused
/// [`Invalid`](SandboxError::Invalid) for a port asked for in the sandbox, which is the
/// package's to give.
pub fn sandbox_args(
    package: &Package,
    args: PackageSandboxCreateArgs,
) -> Result<SandboxCreateArgs, SandboxError> {
    if let Some(inside) = (args.ports.iter()).find(|spec| spec.on != PortSide::Local) {
        return Err(SandboxError::Invalid(format!(
            "ports: {} is in the sandbox, which is the package's to give: ask for ports on local",
            inside.port
        )));
    }
    let mut made = package.sandbox.clone();
    made.name = args.name;
    made.ports.extend(args.ports);
    Ok(made)
}

/// Refuse `args` where no package could be made of them, whatever is here.
fn check(args: &PackagePutArgs, vm: &[OsString]) -> Result<(), SandboxError> {
    let invalid = |why: String| Err(SandboxError::Invalid(why));
    if args.sandbox.name.is_some() {
        return invalid("sandbox: name: each sandbox of a package is named as it is made".into());
    }
    if let Some(local) = (args.sandbox.ports.iter()).find(|spec| spec.on == PortSide::Local) {
        return invalid(format!(
            "sandbox: ports: {} is on local, which each sandbox's client asks for as it is made",
            local.port
        ));
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
    for PackageAccess::Jwt { issuer, jwks, .. } in &args.access {
        for (name, url) in [("issuer", Some(issuer)), ("jwks", jwks.as_ref())] {
            if let Some(url) = url
                && !is_https(url)
            {
                return invalid(format!("access: {name}: {url:?} is no https URL"));
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

fn read(path: &Path) -> Result<Package, SandboxError> {
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

    fn args(image: &str) -> PackagePutArgs {
        PackagePutArgs::new(SandboxCreateArgs::new(image))
    }

    #[test]
    fn a_package_is_put_found_by_its_id_or_name_replaced_keeping_its_id_and_removed() {
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
    fn a_package_no_sandbox_could_be_made_of_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path()).unwrap();
        let refused =
            |name: &str, args: PackagePutArgs| match store.put(name, args, &[], || "p".into()) {
                Err(SandboxError::Invalid(why)) => why,
                answered => panic!("{answered:?}"),
            };
        assert!(refused("../app", args("app:1")).contains("not a package's name"));
        let local = virtx_core::PortSpec::new(5037).on(PortSide::Local);
        let offering = PackagePutArgs::new(SandboxCreateArgs::new("app:1").port(local));
        assert!(refused("app", offering).starts_with("sandbox: ports: 5037"));
        let named = PackagePutArgs::new(SandboxCreateArgs::new("app:1").name("dev"));
        assert!(refused("app", named).starts_with("sandbox: name:"));
        let larger = args("app:1").max(SandboxResizeArgs::default().vcpus(DEFAULT_VCPUS + 1));
        let made = PackagePutArgs::new(SandboxCreateArgs::new("app:1").vcpus(2));
        assert!(
            refused("app", made.max(SandboxResizeArgs::default().vcpus(1)))
                .starts_with("max: vcpus")
        );
        store.put("app", larger, &[], || "p".into()).unwrap();
        let plain = PackageAccess::Jwt {
            issuer: "http://idp.example".into(),
            audience: "app".into(),
            jwks: None,
            claims: Default::default(),
        };
        assert!(refused("app", args("app:1").access(plain)).starts_with("access: issuer"));
        let local = PackageAccess::Jwt {
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

    /// A sandbox of a package is made of its sandbox, named as asked, with the ports on local
    /// asked for beside the package's in the sandbox; one asked for in the sandbox is refused.
    #[test]
    fn a_sandbox_of_a_package_has_the_ports_on_local_asked_for_beside_its_own() {
        use virtx_core::{PackageSandboxCreateArgs, PortSpec};

        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path()).unwrap();
        let args = PackagePutArgs::new(SandboxCreateArgs::new("app:1").port(8000));
        let (package, _) = store.put("app", args, &[], || "p".into()).unwrap();
        let asked = PackageSandboxCreateArgs::default()
            .name("dev")
            .port(PortSpec::new(5037).on(PortSide::Local));
        let made = sandbox_args(&package, asked).unwrap();
        assert_eq!(made.name.as_deref(), Some("dev"));
        let ports: Vec<_> = made.ports.iter().map(|spec| (spec.port, spec.on)).collect();
        assert_eq!(ports, [(8000, PortSide::Sandbox), (5037, PortSide::Local)]);
        let inside = PackageSandboxCreateArgs::default().port(PortSpec::new(9000));
        assert!(matches!(
            sandbox_args(&package, inside),
            Err(SandboxError::Invalid(_))
        ));
    }

    #[test]
    fn what_a_package_names_is_found_while_the_packages_hold_still() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::new(root.path()).unwrap();
        let mounting = SandboxCreateArgs::new("app:1").volume("data", "/data");
        store
            .put("app", PackagePutArgs::new(mounting), &[], || "p".into())
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
