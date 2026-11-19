use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use virtx_core::{ImageName, SandboxCreateArgs, SandboxPort};

use super::hooks::{Hooks, Wanted, Written};
use super::record::{Record, Resized};
use super::sandbox::{Shared, label_fits, pick, spec_of};
use super::{Error, Sandbox};
use crate::{Digest, ImageStore, ImageStoreError, Reference, SnapshotStore, VolumeStore};

/// How long a sandbox is unused before it is deleted, where `VIRTX_RECLAIM_AFTER_MS` does not
/// say.
const RECLAIM_AFTER: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// How long a sandbox is unused before it is deleted, `None` for never:
/// `VIRTX_RECLAIM_AFTER_MS`, in milliseconds, `0` for never; or 30 days.
pub fn reclaim_after() -> Option<Duration> {
    match std::env::var("VIRTX_RECLAIM_AFTER_MS").map(|ms| ms.parse::<u64>()) {
        Ok(Ok(0)) => None,
        Ok(Ok(ms)) => Some(Duration::from_millis(ms)),
        _ => Some(RECLAIM_AFTER),
    }
}

/// How often [`Sandboxes::spawn_reclaiming`] looks for sandboxes to reclaim, at most.
const RECLAIM_EVERY: Duration = Duration::from_secs(60);

/// A sandbox made again here under the id it had elsewhere, and the key each of its ports had
/// there, by key.
pub struct Given {
    pub id: String,
    pub keys: HashMap<String, u16>,
}

/// A server's sandboxes, from each one's making until it is deleted: each kept on disk
/// ([`record`](super::record)), so a server started again knows each, parked -- or exited, one
/// whose machine ended on its own.
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
    /// machine run as `vm` (see [`Machine::start`](crate::Machine::start)) and parked once its
    /// sandbox has been unused for `park_after` ([`park_after`](super::park_after) is a
    /// server's), with `hooks` plugged in.
    pub fn new(
        images: ImageStore,
        snapshots: SnapshotStore,
        volumes: VolumeStore,
        root: &Path,
        vm: Vec<OsString>,
        hooks: Arc<dyn Hooks>,
        park_after: Option<Duration>,
    ) -> std::io::Result<Sandboxes> {
        let kept = root.join("sandbox");
        std::fs::create_dir_all(&kept)?;
        forget_unrecorded(&snapshots, root);
        let records = Record::load_all(&kept, &images);
        let shared = Arc::new(Shared {
            images,
            snapshots,
            volumes,
            vm,
            hooks,
            kept,
            park_after,
        });
        let mut all = HashMap::new();
        for record in records {
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

    /// The sandbox `id`, whether its machine is up or not.
    pub fn get(&self, id: &str) -> Result<Arc<Sandbox>, Error> {
        self.0
            .all
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| Error::NotFound(format!("no sandbox here has the id {id}")))
    }

    /// Every sandbox, as the protocol answers each; those named `name` alone, given one.
    pub fn list(&self, name: Option<&str>) -> Vec<virtx_core::Sandbox> {
        let all: Vec<_> = self.0.all.lock().unwrap().values().cloned().collect();
        all.iter()
            .filter(|sandbox| name.is_none() || sandbox.name() == name)
            .map(|sandbox| sandbox.answer(false))
            .collect()
    }

    /// Every sandbox whose machine is up.
    fn up(&self) -> Vec<Arc<Sandbox>> {
        let all = self.0.all.lock().unwrap();
        all.values()
            .filter(|sandbox| sandbox.up())
            .cloned()
            .collect()
    }

    /// A sandbox made of `args` -- or, `given` an id and its ports' keys, made again here
    /// under that id, its disk taken from where it is kept -- and booted, unless not `booted`:
    /// then made, and its disk brought here, parked, for it to boot at once when it is next
    /// used. Answered with its ports.
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
        match args.disk_gib {
            Some(0) => {
                return Err(Error::Invalid(
                    "disk_gib: a disk of 0 GiB holds nothing".into(),
                ));
            }
            Some(gib) if gib > super::MAX_DISK_GIB => {
                return Err(Error::Invalid(format!(
                    "disk_gib: {gib} is over the {} a sandbox may have",
                    super::MAX_DISK_GIB
                )));
            }
            _ => {}
        }
        super::sandbox::abins_refused(&args.abins, &super::sandbox::abin_programs(&shared.vm))?;

        let digest = match &args.image {
            ImageName::Tag { tag } => {
                let reference: Reference = tag
                    .parse()
                    .map_err(|e| Error::Invalid(format!("{tag}: {e}")))?;
                shared.hooks.want(Wanted::Tag(tag)).await?;
                shared.images.get_tag(&reference)?.ok_or_else(|| {
                    Error::NotFound(format!("no image here is tagged {tag}: pull it first"))
                })?
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
        // Booted by digest every time, whatever its tag points at by then.
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
                // A public address is the cloud's to give (its gateway), not a server's, and
                // so are the inbound rules it would fail to set.
                address: None,
                inbound_error: None,
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
                args,
                opened: Mutex::new(opened),
                keys: Mutex::new(keys),
                used: Mutex::new(std::time::SystemTime::now()),
                exited: Mutex::new(false),
                disk_gib: Mutex::new(None),
                resized: Mutex::new(Resized::default()),
            };
            let sandbox = Sandbox::new(record, Arc::clone(shared));
            all.insert(id, Arc::clone(&sandbox));
            sandbox
        };
        // From here on its making ends one way or the other in a task of its own (issue #155):
        // a caller gone -- its request dropped, as a server drops a handler whose client went
        // away -- neither leaves it half made, nor skips the undo of one that failed. And one
        // made for a caller gone by then is undone too: nobody was told of it.
        let gone = Arc::new(AtomicBool::new(false));
        let _waiting = Waiting(Arc::clone(&gone));
        let this = self.clone();
        tokio::spawn(async move { this.make(sandbox, moved, booted, &gone).await }).await?
    }

    /// Sandbox `sandbox`, in the map already, made: its disk -- or, `moved` here, the one kept
    /// for it -- and booted, if `booted`; then kept, unless `gone` says its caller went away
    /// meanwhile. Undone, but for a disk kept elsewhere, if either fails or its caller left.
    async fn make(
        &self,
        sandbox: Arc<Sandbox>,
        moved: bool,
        booted: bool,
        gone: &AtomicBool,
    ) -> Result<virtx_core::Sandbox, Error> {
        let shared = &self.0.shared;
        let id = sandbox.id().to_owned();
        // Held until its record is written, or its making undone: a disk here no record names
        // yet is not one a server starting meanwhile takes for left over.
        let _making = match Making::begin(&shared.kept, &id) {
            Ok(making) => making,
            Err(e) => {
                self.0.all.lock().unwrap().remove(&id);
                return Err(Error::Failed(format!("making sandbox {id}: {e}")));
            }
        };
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
                sandbox.wake().await?;
            }
            Ok::<_, Error>(())
        };
        let made = match made.await {
            Ok(()) if gone.load(Ordering::SeqCst) => {
                eprintln!("sandbox {id} made for a caller gone: undone");
                sandbox.abandon().await;
                Err(Error::NotFound(format!("sandbox {id}'s caller went away")))
            }
            made => made,
        };
        if let Err(e) = made {
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
        Ok(sandbox.record.answer(true))
    }

    /// Sandbox `id` deleted: its machine stopped first if it is up, its jobs removed, and its
    /// disk removed. Its volumes stay.
    pub async fn delete(&self, id: &str) -> Result<(), Error> {
        let sandbox = self.get(id)?;
        sandbox.delete().await?;
        self.0.all.lock().unwrap().remove(id);
        Ok(())
    }

    /// Sandbox `id`, its machine down, let go of here, its disk left as it is: its record and
    /// its jobs gone, as a delete does, but nothing removed from where it is kept -- what a
    /// server does once the sandbox is to run elsewhere. Refused [`InUse`](Error::InUse) while
    /// its machine is up.
    pub async fn forget(&self, id: &str) -> Result<(), Error> {
        let sandbox = self.get(id)?;
        sandbox.forget().await?;
        self.0.all.lock().unwrap().remove(id);
        Ok(())
    }

    /// The sandbox with a port named by `key`, its machine up or not, and the port.
    pub fn find_port(&self, key: &str) -> Option<(Arc<Sandbox>, u16)> {
        let all = self.0.all.lock().unwrap();
        all.values()
            .find_map(|sandbox| Some((Arc::clone(sandbox), sandbox.keyed(key)?)))
    }

    /// The sandbox whose disk is `id`, with its machine up.
    pub fn running_on(&self, id: &str) -> Option<Arc<Sandbox>> {
        self.get(id).ok().filter(|sandbox| sandbox.up())
    }

    /// How many sandboxes have their machines up, and how many jobs run in them now.
    pub fn counts(&self) -> (usize, usize) {
        let up = self.up();
        let jobs = up.iter().map(|sandbox| sandbox.running_jobs()).sum();
        (up.len(), jobs)
    }

    /// What the machines up now may write.
    pub fn writes(&self) -> Vec<Written> {
        self.up()
            .iter()
            .flat_map(|sandbox| sandbox.writes())
            .collect()
    }

    /// Park every sandbox, in use or not: what runs in each ends, as cancelling it does, and
    /// its disk is kept as its machine stopped.
    pub async fn park_all(&self) {
        for sandbox in self.up() {
            if let Err(e) = sandbox.park_anyway().await {
                eprintln!("sandbox {} not parked: {e}", sandbox.id());
            }
        }
    }

    /// Park every sandbox that nothing uses now ([`Sandbox::park`]), however recently it was
    /// used.
    pub async fn park_unused(&self) {
        for sandbox in self.up() {
            match sandbox.park().await {
                Ok(()) | Err(Error::InUse(_)) => {}
                Err(e) => eprintln!("sandbox {}, unused, not parked: {e}", sandbox.id()),
            }
        }
    }

    /// Park every sandbox in a thread of its own: what they wrote is on their disks once each
    /// machine has ended, and each is kept, parked, for a server started again.
    pub fn close_all(&self) {
        let closing = self.clone();
        std::thread::spawn(move || closing.close_all_blocking());
    }

    /// As [`close_all`](Self::close_all), side by side, blocking until each machine has
    /// stopped: not on an async runtime's thread.
    pub fn close_all_blocking(&self) {
        let all: Vec<_> = self.0.all.lock().unwrap().values().cloned().collect();
        std::thread::scope(|scope| {
            for sandbox in &all {
                scope.spawn(|| sandbox.park_blocking());
            }
        });
    }

    /// Delete each sandbox unused for `after`: nothing in use now, and last used that long
    /// ago, whether its machine is up, parked or exited.
    pub fn reclaim(&self, after: Duration) {
        let all: Vec<_> = self.0.all.lock().unwrap().values().cloned().collect();
        for sandbox in all {
            if sandbox.idle().is_none_or(|idle| idle < after) {
                continue;
            }
            let (sandboxes, id) = (self.clone(), sandbox.id().to_owned());
            tokio::spawn(async move {
                match sandboxes.delete(&id).await {
                    Ok(()) => eprintln!("sandbox {id}, unused, reclaimed"),
                    Err(e) => eprintln!("sandbox {id}, unused, not reclaimed: {e}"),
                }
            });
        }
    }

    /// [`reclaim`](Self::reclaim) those unused for [`reclaim_after`], looked for once a
    /// minute -- or as often as that lasts, if it is shorter -- for as long as these sandboxes
    /// are.
    pub fn spawn_reclaiming(&self) {
        let Some(after) = reclaim_after() else {
            return;
        };
        let reclaimed: Weak<Inner> = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            let mut every = tokio::time::interval(after.min(RECLAIM_EVERY));
            loop {
                every.tick().await;
                let Some(inner) = reclaimed.upgrade() else {
                    return;
                };
                Sandboxes(inner).reclaim(after);
            }
        });
    }
}

/// Its caller still waiting on a making ([`Sandboxes::create`]): once dropped -- answered, or gone
/// -- the making's task is told so.
struct Waiting(Arc<AtomicBool>);

impl Drop for Waiting {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A sandbox's making under way: `<id>.making` beside the records, locked from before its disk
/// is made until its record is written or its making undone, and gone after. While it is held
/// -- by this server or another over the same home -- its disk is not one left over.
struct Making {
    path: PathBuf,
    _held: File,
}

impl Making {
    fn begin(kept: &Path, id: &str) -> std::io::Result<Making> {
        let path = kept.join(format!("{id}.making"));
        // One a server starting removed between its creating and its locking -- left over, as
        // it looked -- made again.
        for _ in 0..3 {
            let file = File::create(&path)?;
            file.lock()?;
            if path.exists() {
                return Ok(Making { path, _held: file });
            }
        }
        Err(std::io::Error::other(format!(
            "{} kept being removed",
            path.display()
        )))
    }
}

impl Drop for Making {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The disks under `snapshots` that no sandbox's record under `root` names, and that no making
/// under way holds ([`Making`]), removed: those of a making cut short -- the server ended, or
/// its caller left, before it was kept -- which nothing lists or deletes (issue #155). One
/// another server is making stays. Called before anything counts what is here: a server's
/// keeping, and [`Sandboxes::new`].
pub fn forget_unrecorded(snapshots: &SnapshotStore, root: &Path) {
    let kept = root.join("sandbox");
    let Ok(all) = snapshots.list() else {
        return;
    };
    for snapshot in all {
        let recorded = || kept.join(format!("{}.json", snapshot.name)).exists();
        if recorded() {
            continue;
        }
        let making = kept.join(format!("{}.making", snapshot.name));
        let stale = match File::open(&making) {
            Ok(file) => match file.try_lock() {
                Ok(()) => Some(file),
                // Being made, here or by another server.
                Err(_) => continue,
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => continue,
        };
        // A making lets go only once its record is written, or its disk removed: one that has
        // since is recorded.
        if recorded() {
            continue;
        }
        if snapshots.remove(&snapshot.name).is_ok() {
            eprintln!(
                "disk {} removed: no sandbox here was made of it",
                snapshot.name
            );
        }
        if stale.is_some() {
            let _ = std::fs::remove_file(&making);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FsTree;
    use crate::sandbox::NoHooks;

    /// Sandboxes over a store under `root`, with an empty image tagged `here:latest`.
    fn sandboxes(root: &Path) -> Sandboxes {
        sandboxes_with(root, Arc::new(NoHooks))
    }

    /// As [`sandboxes`], with `hooks`.
    fn sandboxes_with(root: &Path, hooks: Arc<dyn Hooks>) -> Sandboxes {
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
            hooks,
            None,
        )
        .unwrap()
    }

    /// Hooks whose `made` waits, once it is reached, until it is let go on.
    #[derive(Default)]
    struct HeldMade {
        reached: tokio::sync::Notify,
        go_on: tokio::sync::Notify,
    }

    impl Hooks for HeldMade {
        fn made<'a>(
            &'a self,
            _: &'a str,
            _: u64,
        ) -> futures_util::future::BoxFuture<'a, Result<(), Error>> {
            Box::pin(async move {
                self.reached.notify_one();
                self.go_on.notified().await;
                Ok(())
            })
        }
    }

    /// A making whose caller goes away midway -- its future dropped, as a server drops a
    /// handler whose client left -- ends undone: no sandbox listed, no record, no disk; and
    /// the next server starts with none of it (issue #155).
    #[tokio::test]
    async fn a_making_whose_caller_goes_away_leaves_nothing() {
        let root = std::env::temp_dir().join(format!("virtx-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let hooks = Arc::new(HeldMade::default());
        let here = sandboxes_with(&root, hooks.clone());
        let making = here.create(SandboxCreateArgs::new("here:latest"), None, false);
        tokio::select! {
            made = making => panic!("made before its hook let it: {made:?}"),
            () = hooks.reached.notified() => {}
        }
        // Its caller gone, its disk made already: let go on.
        assert_eq!(here.list(None).len(), 1, "being made");
        hooks.go_on.notify_one();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !here.list(None).is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "never undone");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let left = |dir: &str| {
            std::fs::read_dir(root.join(dir))
                .map(|entries| entries.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        assert_eq!(
            left("sandbox"),
            Vec::<OsString>::new(),
            "its disk and its record"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A disk no sandbox's record names -- a making a server ended midway left it -- is removed
    /// as the next server starts; one a record names stays.
    #[tokio::test]
    async fn a_disk_no_sandbox_was_made_of_is_removed_as_a_server_starts() {
        let root = std::env::temp_dir().join(format!("virtx-unrecorded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let here = sandboxes(&root);
        let made = here
            .create(SandboxCreateArgs::new("here:latest"), None, false)
            .await
            .unwrap();
        here.snapshots().create("half-made", 64 << 20).unwrap();
        drop(here);
        let next = sandboxes(&root);
        assert!(next.snapshots().open("half-made").is_err());
        assert!(next.snapshots().open(&made.id).is_ok());
        assert_eq!(next.list(None).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A server starting while another makes a sandbox -- its disk there, its record not yet
    /// -- leaves that disk alone: the sandbox, once made, starts over it.
    #[tokio::test]
    async fn a_disk_being_made_is_left_alone_by_a_server_starting() {
        let root = std::env::temp_dir().join(format!("virtx-making-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let hooks = Arc::new(HeldMade::default());
        let here = sandboxes_with(&root, hooks.clone());
        let making = {
            let here = here.clone();
            tokio::spawn(async move {
                here.create(SandboxCreateArgs::new("here:latest"), None, false)
                    .await
            })
        };
        hooks.reached.notified().await;
        let disks = here.snapshots().list().unwrap();
        assert_eq!(disks.len(), 1, "its disk made, its record not yet");
        let started = sandboxes(&root);
        hooks.go_on.notify_one();
        let made = making.await.unwrap().unwrap();
        assert_eq!(made.id, disks[0].name);
        assert!(here.snapshots().open(&made.id).is_ok(), "its disk");
        drop(started);
        // Nor does one starting once it is made, nor is anything of its making left.
        drop(sandboxes(&root));
        assert!(here.snapshots().open(&made.id).is_ok(), "its disk");
        let mut left: Vec<_> = std::fs::read_dir(root.join("sandbox"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .filter(|name| !name.to_string_lossy().ends_with(".lock"))
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                OsString::from(format!("{}.ext4", made.id)),
                OsString::from(format!("{}.json", made.id))
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_sandbox_is_kept_for_the_next_server_until_it_is_deleted() {
        let root = std::env::temp_dir().join(format!("virtx-sandboxes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let here = sandboxes(&root);
        let args = SandboxCreateArgs::new("here:latest")
            .name("work")
            .port(8000);
        let made = here.create(args.clone(), None, false).await.unwrap();
        assert!(made.running, "parked, it is running still");
        assert!(!here.get(&made.id).unwrap().up());
        assert!(made.image.starts_with("sha256:"));
        assert!(root.join(format!("sandbox/{}.ext4", made.id)).is_file());
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
        assert_eq!(here.list(Some("work")).len(), 1);
        let used = here.get(&made.id).unwrap().last_used();

        // The next server over the same root knows it, parked, as it was made.
        drop(here);
        let next = sandboxes(&root);
        let listed = next.list(None);
        assert_eq!(listed.len(), 1);
        let kept = next.get(&made.id).unwrap();
        assert_eq!(
            (kept.id(), kept.name(), kept.up()),
            (made.id.as_str(), Some("work"), false)
        );
        assert!(listed[0].running);
        let at = |time: std::time::SystemTime| {
            time.duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        assert_eq!(at(kept.last_used()), at(used));
        assert_eq!(kept.answer(true).ports, made.ports);

        next.delete(&made.id).await.unwrap();
        assert!(matches!(next.get(&made.id), Err(Error::NotFound(_))));
        assert!(!root.join(format!("sandbox/{}.ext4", made.id)).exists());
        assert!(!root.join(format!("sandbox/{}.json", made.id)).exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// One whose machine ended on its own is kept exited, across a server started again: not
    /// running, and booted for nothing.
    #[tokio::test]
    async fn a_sandbox_whose_machine_exited_stays_exited_for_the_next_server() {
        let root = std::env::temp_dir().join(format!("virtx-exited-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let here = sandboxes(&root);
        let made = here
            .create(SandboxCreateArgs::new("here:latest"), None, false)
            .await
            .unwrap();
        // As its machine ending on its own leaves it.
        let sandbox = here.get(&made.id).unwrap();
        *sandbox.record.exited.lock().unwrap() = true;
        sandbox.keep();
        drop((sandbox, here));

        let next = sandboxes(&root);
        assert!(!next.list(None)[0].running);
        let sandbox = next.get(&made.id).unwrap();
        let started = sandbox.start().await;
        assert!(matches!(started, Err(Error::Ended)), "{started:?}");
        let exec = sandbox.exec(vec!["true".into()], false, None).await;
        assert!(matches!(exec, Err(Error::Ended)), "{:?}", exec.err());
        next.delete(&made.id).await.unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A sandbox unused for as long as reclaiming waits is deleted, whether its machine was
    /// ever up or not.
    #[tokio::test]
    async fn a_sandbox_unused_long_enough_is_reclaimed() {
        let root = std::env::temp_dir().join(format!("virtx-reclaim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let here = sandboxes(&root);
        let made = here
            .create(SandboxCreateArgs::new("here:latest"), None, false)
            .await
            .unwrap();
        here.reclaim(Duration::from_secs(3600));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(here.list(None).len(), 1, "used just now");

        here.reclaim(Duration::ZERO);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !here.list(None).is_empty() {
            assert!(tokio::time::Instant::now() < deadline, "never reclaimed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(!root.join(format!("sandbox/{}.ext4", made.id)).exists());
        assert!(!root.join(format!("sandbox/{}.json", made.id)).exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
