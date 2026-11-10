use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime};

use virtx_core::{ImageName, SandboxCreateArgs, SandboxPort};

use super::hooks::{Hooks, Wanted, Written};
use super::record::{Record, Resized};
use super::sandbox::{Shared, label_fits, pick, spec_of};
use super::{Error, Sandbox};
use crate::{Digest, ImageStore, ImageStoreError, Reference, Snapshot, SnapshotStore, VolumeStore};

/// How long a sandbox runs unused before it stops, and how long one is kept stopped and unused
/// before it is deleted, where its making names neither.
pub const AUTO_STOP_MINUTES: u32 = 15;
pub const AUTO_DELETE_DAYS: u32 = 30;

static MINUTE: OnceLock<Duration> = OnceLock::new();

/// A minute made `minute` long, as a test waiting on sandboxes stopping by themselves makes it;
/// before any sandbox is tended.
pub fn set_minute(minute: Duration) {
    let _ = MINUTE.set(minute);
}

/// How long one of a sandbox's `auto_stop_minutes` lasts, and a day of its `auto_delete_days`
/// is 1440 of: as [`set_minute`] made it, else `VIRTX_MINUTE_MS` milliseconds, else a minute.
pub fn minute() -> Duration {
    *MINUTE.get_or_init(|| {
        std::env::var("VIRTX_MINUTE_MS")
            .ok()
            .and_then(|ms| ms.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map_or(Duration::from_secs(60), Duration::from_millis)
    })
}

/// A sandbox made again here under the id it had elsewhere, and the key each of its ports had
/// there, by key.
pub struct Given {
    pub id: String,
    pub keys: HashMap<String, u16>,
}

/// A server's sandboxes, running or stopped, from each one's making until it is deleted: each
/// kept on disk ([`record`](super::record)), so a server started again knows each, stopped.
///
/// A handle: cloned, it is the same sandboxes.
#[derive(Clone)]
pub struct Sandboxes(Arc<Inner>);

struct Inner {
    shared: Arc<Shared>,
    /// Every sandbox, by id.
    all: Mutex<HashMap<String, Arc<Sandbox>>>,
}

impl Sandboxes {
    /// The sandboxes kept under `root`, of what `images`, `snapshots` and `volumes` hold, each
    /// machine run as `vm` (see [`Machine::start`](crate::Machine::start)), with `hooks`
    /// plugged in. An ephemeral sandbox a server before this one left -- ended before it could
    /// delete it -- is deleted now.
    pub fn new(
        images: ImageStore,
        snapshots: SnapshotStore,
        volumes: VolumeStore,
        root: &Path,
        vm: Vec<OsString>,
        hooks: Arc<dyn Hooks>,
    ) -> std::io::Result<Sandboxes> {
        let kept = root.join("sandbox");
        std::fs::create_dir_all(&kept)?;
        sweep(&kept);
        let records = Record::load_all(&kept, &images);
        let shared = Arc::new(Shared {
            images,
            snapshots,
            volumes,
            vm,
            hooks,
            kept,
        });
        let mut all = HashMap::new();
        for record in records {
            if record.ephemeral {
                // Left alone while another server runs it.
                match shared.snapshots.remove(&record.id) {
                    Ok(()) | Err(crate::SnapshotError::Unknown(_)) => {
                        let _ =
                            std::fs::remove_file(shared.kept.join(format!("{}.json", record.id)));
                    }
                    Err(_) => {}
                }
                continue;
            }
            all.insert(record.id.clone(), Sandbox::new(record, Arc::clone(&shared)));
        }
        Ok(Sandboxes(Arc::new(Inner {
            shared,
            all: Mutex::new(all),
        })))
    }

    pub fn images(&self) -> &ImageStore {
        &self.0.shared.images
    }

    pub fn snapshots(&self) -> &SnapshotStore {
        &self.0.shared.snapshots
    }

    pub fn volumes(&self) -> &VolumeStore {
        &self.0.shared.volumes
    }

    /// What its machines run as.
    pub fn vm(&self) -> &[OsString] {
        &self.0.shared.vm
    }

    /// The sandbox `id`, running or stopped.
    pub fn get(&self, id: &str) -> Result<Arc<Sandbox>, Error> {
        self.0
            .all
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("no sandbox here has the id {id}")))
    }

    /// Every sandbox, running or stopped, as the protocol answers each; those named `name`
    /// alone, given one.
    pub fn list(&self, name: Option<&str>) -> Vec<virtx_core::Sandbox> {
        let all: Vec<_> = self.0.all.lock().unwrap().values().cloned().collect();
        all.iter()
            .filter(|sandbox| name.is_none() || sandbox.name() == name)
            .map(|sandbox| sandbox.answer(false))
            .collect()
    }

    /// Every sandbox that runs.
    fn running(&self) -> Vec<Arc<Sandbox>> {
        let all = self.0.all.lock().unwrap();
        all.values()
            .filter(|sandbox| sandbox.is_running())
            .cloned()
            .collect()
    }

    /// A sandbox made of `args` -- or, `given` an id and its ports' keys, made again here
    /// under that id, its disk taken from where it is kept -- and booted, unless not `booted`:
    /// then made, and its disk brought here, for it to start at once when it is next asked to.
    /// Answered with its ports.
    pub async fn create(
        &self,
        mut args: SandboxCreateArgs,
        given: Option<Given>,
        booted: bool,
    ) -> Result<virtx_core::Sandbox, Error> {
        let shared = &self.0.shared;
        // What can be refused without starting anything, first.
        let mut ports = std::collections::HashSet::new();
        if let Some(spec) = args
            .ports
            .iter()
            .find(|spec| spec.port == 0 || !ports.insert(spec.port))
        {
            return Err(Error::Invalid(format!(
                "ports: {} is not a port, or is given twice",
                spec.port
            )));
        }
        if let Some(spec) = args
            .ports
            .iter()
            .find(|spec| !label_fits(spec.label.as_deref()))
        {
            return Err(Error::Invalid(format!(
                "ports: {}'s label is past {} characters",
                spec.port,
                super::LABEL_MOST
            )));
        }
        if let Some(spec) = args.ports.iter().find(|spec| spec.inbound.is_some()) {
            return Err(Error::Invalid(format!(
                "ports: {}'s inbound rules are virtx cloud's; this server keeps none",
                spec.port
            )));
        }
        spec_of(&args)?;
        if args.disk_gib == Some(0) {
            return Err(Error::Invalid(
                "disk_gib: a disk of 0 GiB holds nothing".into(),
            ));
        }
        if args.auto_stop_minutes == Some(0) {
            return Err(Error::Invalid(
                "auto_stop_minutes: 0 stops it before it is used".into(),
            ));
        }
        super::sandbox::abins_refused(&args.abins, &super::sandbox::abin_programs(&shared.vm))?;

        let digest = match &args.image {
            ImageName::Tag { tag } => {
                let reference: Reference = tag
                    .parse()
                    .map_err(|e| Error::Invalid(format!("{tag}: {e}")))?;
                shared.hooks.want(Wanted::Tag(tag)).await?;
                shared
                    .images
                    .get_tag(&reference)?
                    .ok_or_else(|| Error::NotFound(format!("no image here is tagged {tag}")))?
            }
            ImageName::Digest { digest } => {
                let digest = digest
                    .parse::<Digest>()
                    .map_err(|e| Error::Invalid(format!("{digest}: {e}")))?;
                // A registry's digest, as a pull or a push answers, names the image it copied.
                let digest = shared.images.get_image_origin(&digest)?.unwrap_or(digest);
                shared.hooks.want(Wanted::Image(&digest)).await?;
                digest
            }
        };
        let held = shared.images.hold_image(&digest).map_err(|e| match e {
            ImageStoreError::Missing(_) => Error::NotFound(format!("no image here is {digest}")),
            e => e.into(),
        })?;
        // Booted by digest every time it starts, whatever its tag points at by then.
        args.image = ImageName::Digest {
            digest: digest.to_string(),
        };

        // Each port named as the hooks name it: one made again keeps the key it had.
        let (given, kept) = match given {
            Some(Given { id, keys }) => (Some(id), keys),
            None => (None, HashMap::new()),
        };
        let mut opened = Vec::new();
        let mut keys = HashMap::new();
        for spec in &args.ports {
            let had = kept
                .iter()
                .find(|(_, port)| **port == spec.port)
                .map(|(key, _)| key.clone());
            let (key, url) = shared.hooks.port(spec, had);
            opened.push(SandboxPort {
                port: spec.port,
                url,
                label: spec.label.clone(),
                protocol: spec.protocol,
                // A public address is the cloud's to give (its gateway), not a server's.
                address: None,
            });
            if let Some(key) = key {
                keys.insert(key, spec.port);
            }
        }

        let moved = given.is_some();
        let sandbox = {
            let mut all = self.0.all.lock().unwrap();
            if let Some(name) = &args.name
                && all.values().any(|sandbox| sandbox.name() == Some(name))
            {
                return Err(Error::Exists(format!(
                    "a sandbox here is named {name} already"
                )));
            }
            let id = match given {
                Some(id) if all.contains_key(&id) => {
                    return Err(Error::Exists(format!("a sandbox here is {id} already")));
                }
                Some(id) => id,
                None => pick(|id| all.contains_key(id) || shared.snapshots.open(id).is_ok()),
            };
            let record = Record {
                id: id.clone(),
                name: args.name.clone(),
                image: digest,
                _held: held,
                ephemeral: args.ephemeral,
                auto_stop_minutes: args.auto_stop_minutes.unwrap_or(AUTO_STOP_MINUTES),
                auto_delete_days: args.auto_delete_days.unwrap_or(Some(AUTO_DELETE_DAYS)),
                args,
                opened: Mutex::new(opened),
                keys: Mutex::new(keys),
                used: Mutex::new(SystemTime::now()),
                disk_gib: Mutex::new(None),
                resized: Mutex::new(Resized::default()),
            };
            let sandbox = Sandbox::new(record, Arc::clone(shared));
            all.insert(id, Arc::clone(&sandbox));
            sandbox
        };
        let id = sandbox.id().to_owned();

        // Its disk: made here -- or, for one made again here, the one kept for it, brought
        // here. Where none was kept -- its server gone before it kept one -- a new one.
        let kept = match moved {
            false => Ok(false),
            true => match shared.hooks.adopted(&id, booted).await {
                Ok(Some(kept)) => Ok(kept),
                Ok(None) => Ok(shared.snapshots.open(&id).is_ok()),
                Err(e) => Err(e),
            },
        };
        let kept = match kept {
            Ok(kept) => kept,
            Err(e) => {
                self.0.all.lock().unwrap().remove(&id);
                return Err(e);
            }
        };
        let made = async {
            if !kept {
                if moved {
                    eprintln!("sandbox {id} made again with no disk kept for it: a new one");
                }
                let size = sandbox.record.disk_size();
                let snapshots = shared.snapshots.clone();
                let making = id.clone();
                tokio::task::spawn_blocking(move || snapshots.create(&making, size)).await??;
                shared.hooks.made(&id, size).await?;
            }
            if booted {
                sandbox.running().await?;
            }
            Ok::<_, Error>(())
        };
        if let Err(e) = made.await {
            self.0.all.lock().unwrap().remove(&id);
            if !kept {
                let _ = shared.hooks.deleting(&id).await;
                let _ = shared.snapshots.remove(&id);
            }
            return Err(e);
        }
        sandbox.keep();
        if kept && !booted {
            shared.hooks.readied(&id);
        }
        Ok(sandbox.record.answer(booted, true))
    }

    /// Sandbox `id` stopped, if it runs: its jobs ended, its machine stopped with what the guest
    /// wrote on its disk, and its disk kept -- or, for an ephemeral one, deleted with it.
    /// Answered as it was as it stopped.
    pub async fn stop(&self, id: &str) -> Result<virtx_core::Sandbox, Error> {
        let sandbox = self.get(id)?;
        sandbox.stop().await?;
        let stopped = sandbox.record.answer(false, false);
        if sandbox.ephemeral() {
            self.delete(id).await?;
        }
        Ok(stopped)
    }

    /// Sandbox `id` deleted: stopped first if it runs, its jobs removed, and its disk
    /// removed. Its volumes stay.
    pub async fn delete(&self, id: &str) -> Result<(), Error> {
        let sandbox = self.get(id)?;
        sandbox.delete().await?;
        self.0.all.lock().unwrap().remove(id);
        Ok(())
    }

    /// Sandbox `id`, stopped, let go of here, its disk left as it is: its record and its jobs
    /// gone, as a delete does, but nothing removed from where it is kept -- what a server does
    /// once the sandbox is to run elsewhere. Refused while it runs.
    pub async fn forget(&self, id: &str) -> Result<(), Error> {
        let sandbox = self.get(id)?;
        sandbox.forget().await?;
        self.0.all.lock().unwrap().remove(id);
        Ok(())
    }

    /// The sandbox that runs with a port named by `key`, and the port.
    pub fn find_port(&self, key: &str) -> Option<(Arc<Sandbox>, u16)> {
        self.running()
            .into_iter()
            .find_map(|sandbox| Some((Arc::clone(&sandbox), sandbox.keyed(key)?)))
    }

    /// The sandbox whose disk is `id`, with its machine up.
    pub fn running_on(&self, id: &str) -> Option<Arc<Sandbox>> {
        self.get(id).ok().filter(|sandbox| sandbox.is_running())
    }

    /// How many sandboxes run, and how many jobs run in them now.
    pub fn counts(&self) -> (usize, usize) {
        let running = self.running();
        let jobs = running.iter().map(|sandbox| sandbox.running_jobs()).sum();
        (running.len(), jobs)
    }

    /// What the machines up now may write.
    pub fn writes(&self) -> Vec<Written> {
        self.running()
            .iter()
            .flat_map(|sandbox| sandbox.writes())
            .collect()
    }

    /// Stop every sandbox, in use or not: what runs in each ends, and its disk is kept as it
    /// stopped.
    pub async fn stop_all(&self) {
        for sandbox in self.running() {
            if let Err(e) = self.stop(sandbox.id()).await {
                eprintln!("sandbox {} not stopped: {e}", sandbox.id());
            }
        }
    }

    /// Stop every sandbox that runs nothing and is reached through nothing now, whatever its
    /// `auto_stop_minutes`. A terminal open in one does not keep it.
    pub async fn stop_unused(&self) {
        for sandbox in self.running() {
            if sandbox.busy() {
                continue;
            }
            if let Err(e) = self.stop(sandbox.id()).await {
                eprintln!("sandbox {}, unused, not stopped: {e}", sandbox.id());
            }
        }
    }

    /// Stop every sandbox in a thread of its own: what they wrote is on their disks once each
    /// has ended, and each is kept, stopped, for a server started again -- but an ephemeral
    /// one, deleted.
    pub fn close_all(&self) {
        let closing = self.clone();
        std::thread::spawn(move || closing.close_all_blocking());
    }

    /// As [`close_all`](Self::close_all), side by side, blocking until each has stopped: not on
    /// an async runtime's thread.
    pub fn close_all_blocking(&self) {
        let all: Vec<_> = self.0.all.lock().unwrap().values().cloned().collect();
        std::thread::scope(|scope| {
            for sandbox in &all {
                scope.spawn(|| match sandbox.ephemeral() {
                    true => sandbox.delete_blocking(),
                    false => sandbox.stop_blocking(),
                });
            }
        });
        let mut kept = self.0.all.lock().unwrap();
        for sandbox in all.iter().filter(|sandbox| sandbox.ephemeral()) {
            kept.remove(sandbox.id());
        }
    }

    /// Stop each sandbox that has run nothing, and been reached through nothing, for its
    /// `auto_stop_minutes`; and delete each kept stopped and unused for its
    /// `auto_delete_days`. A terminal counts as use while it is typed in, or runs a command --
    /// its shell's foreground another's, as the guest says when asked -- not while it sits at a
    /// prompt.
    pub fn tend(&self) {
        let now = SystemTime::now();
        let idle = |sandbox: &Sandbox, limit: Duration| {
            now.duration_since(sandbox.used_at())
                .is_ok_and(|unused| unused >= limit)
        };
        let all: Vec<_> = self.0.all.lock().unwrap().values().cloned().collect();
        let (mut stopping, mut deleting) = (Vec::new(), Vec::new());
        for sandbox in all {
            if !sandbox.is_running() {
                if sandbox
                    .record
                    .auto_delete_days
                    .is_some_and(|days| idle(&sandbox, minute() * days * 24 * 60))
                {
                    deleting.push(sandbox.id().to_owned());
                }
                continue;
            }
            let limit = minute() * sandbox.record.auto_stop_minutes;
            if sandbox.busy() || !idle(&sandbox, limit) {
                sandbox
                    .asked
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                continue;
            }
            // A terminal open, untouched: asked first whether a command runs in it, which
            // keeps it used if one does; stopped the next time if none said so.
            let terminals = sandbox.terminals();
            if terminals.is_empty() || sandbox.asked.load(std::sync::atomic::Ordering::Relaxed) {
                stopping.push(sandbox.id().to_owned());
                continue;
            }
            sandbox
                .asked
                .store(true, std::sync::atomic::Ordering::Relaxed);
            for pid in terminals {
                sandbox.ask_foreground(pid);
            }
        }
        for id in stopping {
            let sandboxes = self.clone();
            tokio::spawn(async move {
                if let Err(e) = sandboxes.stop(&id).await {
                    eprintln!("sandbox {id}, unused, not stopped: {e}");
                }
            });
        }
        for id in deleting {
            let sandboxes = self.clone();
            tokio::spawn(async move {
                if let Err(e) = sandboxes.delete(&id).await {
                    eprintln!("sandbox {id}, unused past its days, not deleted: {e}");
                }
            });
        }
    }

    /// [`tend`](Self::tend) once a minute, or as often as a minute made shorter lasts, for as
    /// long as these sandboxes are.
    pub fn spawn_tending(&self) {
        let tended: Weak<Inner> = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            let mut every = tokio::time::interval(minute().min(Duration::from_secs(60)));
            loop {
                every.tick().await;
                let Some(inner) = tended.upgrade() else {
                    return;
                };
                Sandboxes(inner).tend();
            }
        });
    }
}

/// What a server before sandboxes were kept left under `dir`: a disk, `<name>.ext4`, for each
/// lock file `<name>.owner` that the process that made it no longer holds.
fn sweep(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let owner = entry.path();
        if owner.extension().is_none_or(|e| e != "owner") {
            continue;
        }
        let Ok(file) = File::open(&owner) else {
            continue;
        };
        if file.try_lock().is_err() {
            continue;
        }
        let Some(name) = owner.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        Snapshot {
            name: name.to_owned(),
            path: dir.join(format!("{name}.ext4")),
        }
        .delete();
        let _ = std::fs::remove_file(&owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FsTree;
    use crate::sandbox::NoHooks;

    /// Sandboxes over a store under `root`, with an empty image tagged `here:latest`.
    fn sandboxes(root: &Path) -> Sandboxes {
        let images = ImageStore::new(root.join("images")).unwrap();
        if images
            .get_tag(&"here:latest".parse().unwrap())
            .unwrap()
            .is_none()
        {
            let layer = images.insert_layer(&FsTree::new()).unwrap();
            let config = serde_json::from_str(
                r#"{"architecture": "arm64", "os": "linux", "rootfs": {"type": "layers", "diff_ids": []}}"#,
            )
            .unwrap();
            let image = images.insert_image(&config, &[layer]).unwrap();
            images
                .insert_tag(&"here:latest".parse().unwrap(), &image)
                .unwrap();
        }
        Sandboxes::new(
            images,
            SnapshotStore::new(root).unwrap(),
            VolumeStore::new(root).unwrap(),
            root,
            Vec::new(),
            Arc::new(NoHooks),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn a_sandbox_is_kept_for_the_next_server_and_an_ephemeral_one_is_not() {
        let root = std::env::temp_dir().join(format!("virtx-sandboxes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let here = sandboxes(&root);
        let args = SandboxCreateArgs::new("here:latest")
            .name("work")
            .port(8000)
            .auto_delete_days(None);
        let made = here.create(args.clone(), None, false).await.unwrap();
        assert_eq!(made.state, virtx_core::SandboxState::Stopped);
        assert!(made.image.starts_with("sha256:"));
        assert!(root.join(format!("snapshot/{}.ext4", made.id)).is_file());
        // One name to one sandbox, and one id.
        let again = here.create(args, None, false).await;
        assert!(matches!(again, Err(Error::Exists(_))), "{again:?}");
        let given = Given {
            id: made.id.clone(),
            keys: HashMap::new(),
        };
        let again = here
            .create(SandboxCreateArgs::new("here:latest"), Some(given), false)
            .await;
        assert!(matches!(again, Err(Error::Exists(_))), "{again:?}");
        let gone = here
            .create(
                SandboxCreateArgs::new("here:latest").ephemeral(true),
                None,
                false,
            )
            .await
            .unwrap();
        assert_eq!(here.list(Some("work")).len(), 1);
        assert_eq!(here.list(None).len(), 2);

        // The next server over the same root knows it, stopped, as it was made; not the
        // ephemeral one.
        drop(here);
        let next = sandboxes(&root);
        let listed = next.list(None);
        assert_eq!(listed.len(), 1);
        let kept = &listed[0];
        assert_eq!(
            (
                kept.id.as_str(),
                kept.name.as_deref(),
                kept.auto_delete_days
            ),
            (made.id.as_str(), Some("work"), None)
        );
        assert_eq!(kept.used_at, made.used_at);
        assert_eq!(next.get(&made.id).unwrap().answer(true).ports, made.ports);
        assert!(!root.join(format!("snapshot/{}.ext4", gone.id)).exists());

        next.delete(&made.id).await.unwrap();
        assert!(matches!(next.get(&made.id), Err(Error::NotFound(_))));
        assert!(!root.join(format!("snapshot/{}.ext4", made.id)).exists());
        assert!(!root.join(format!("sandbox/{}.json", made.id)).exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
