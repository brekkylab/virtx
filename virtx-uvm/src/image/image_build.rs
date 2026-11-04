//! Building an image a [`Step`] at a time, over a base the caller has.
//!
//! A `RUN` is a machine booted over the image so far and a snapshot made empty for it,
//! running the command. What the command wrote is the overlay's upper directory on the
//! snapshot, which is a changeset already -- whiteouts and opaque directories included --
//! and becomes a layer as it is. `ENV` and `WORKDIR` change only the config, which every
//! later `RUN` runs under and the image keeps.

use std::ffi::OsString;
use std::fs::{self, File};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oci_spec::image::{Digest, ImageConfiguration};
use virtx_core::Step;

use super::store::{self, Store};
use crate::fstree::{self, FsTree};
use crate::machine::wire::MachineMessage;
use crate::machine::{self, Machine, MachineConfig, MachineSpec};
use crate::snapshot::{self, Snapshot};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A command that did not exit 0.
    #[error("`{command}` exited with {code}")]
    Failed { command: String, code: i32 },

    #[error(transparent)]
    Store(#[from] store::Error),

    #[error(transparent)]
    Machine(#[from] machine::Error),

    #[error(transparent)]
    Snapshot(#[from] snapshot::Error),

    #[error(transparent)]
    Fstree(#[from] fstree::Error),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
}

impl From<tokio::task::JoinError> for Error {
    fn from(e: tokio::task::JoinError) -> Error {
        Error::Io {
            context: "building a layer".into(),
            source: std::io::Error::other(e),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Take the image of `layers` and `image`'s config one `step` further: an `ENV` or a
/// `WORKDIR` into the config, and what a `RUN` wrote onto `layers`, filed in `store` --
/// nothing, for a command that wrote nothing.
///
/// A `RUN` runs through `/bin/sh -c`, in the config's working directory -- made if it is
/// not there -- and in a machine as `spec` says, spawned as `vm` (see [`Machine::start`]).
/// Each line it writes goes to `report`. One that exits with anything but 0 is
/// [`Error::Failed`], and changes nothing.
pub async fn build_step(
    store: &Store,
    image: &mut ImageConfiguration,
    layers: &mut Vec<Digest>,
    step: &Step,
    vm: &[OsString],
    spec: &MachineSpec,
    mut report: impl FnMut(String) + Send,
) -> Result<()> {
    let mut config = image.config().clone().unwrap_or_default();
    let command = match step {
        Step::Env { key, value } => {
            let mut env = config.env().clone().unwrap_or_default();
            env.retain(|var| var.split_once('=').map_or(var.as_str(), |(name, _)| name) != key);
            env.push(format!("{key}={value}"));
            config.set_env(Some(env));
            image.set_config(Some(config));
            return Ok(());
        }
        // Relative to the one before, as a Dockerfile's is.
        Step::Workdir(dir) => {
            let dir = match config.working_dir() {
                Some(at) if !dir.starts_with('/') => format!("{}/{dir}", at.trim_end_matches('/')),
                _ => dir.clone(),
            };
            config.set_working_dir(Some(dir));
            image.set_config(Some(config));
            return Ok(());
        }
        Step::Run(command) => command,
    };

    let rootfs = {
        let (store, layers) = (store.clone(), layers.clone());
        tokio::task::spawn_blocking(move || store.insert_rootfs(&layers)).await??
    };
    let scratch = Scratch::new()?;
    let snapshot = {
        let snapshots = scratch.0.clone();
        tokio::task::spawn_blocking(move || snapshots.create("layer", Snapshot::SIZE)).await??
    };

    // From the root, as the guest's init would fail to go somewhere not there yet. Synced
    // before it is done, since the machine is killed after and what the guest had not put on
    // the snapshot would be lost.
    let workdir = config.working_dir().clone().unwrap_or_else(|| "/".into());
    config.set_working_dir(None);
    let cmd = [
        "/bin/sh",
        "-c",
        r#"mkdir -p "$0" && cd "$0" && /bin/sh -c "$1"; code=$?; sync; exit $code"#,
        workdir.as_str(),
        command,
    ];
    let config = MachineConfig {
        rootfs,
        config,
        snapshot: Some(snapshot.clone()),
        spec: spec.clone(),
        volumes: Vec::new(),
        ports: Vec::new(),
    };
    let vm = vm.to_vec();
    let machine =
        Arc::new(tokio::task::spawn_blocking(move || Machine::start(config, &vm)).await??);
    // Killed when this is dropped before the command has ended -- a build cancelled -- which
    // the thread waiting on the machine below would not see.
    struct Kill(Arc<Machine>);
    impl Drop for Kill {
        fn drop(&mut self) {
            self.0.kill();
        }
    }
    let kill = Kill(Arc::clone(&machine));
    let cmd = cmd.map(str::to_owned).to_vec();
    let (lines, mut said) = tokio::sync::mpsc::unbounded_channel();
    let ran = tokio::task::spawn_blocking(move || {
        machine.exec(cmd)?;
        let id = loop {
            match machine.recv() {
                Some(MachineMessage::Started(started)) => {
                    break started.map_err(machine::Error::NotStarted)?;
                }
                Some(_) => {}
                None => {
                    return Err(machine::Error::Died(
                        "the machine's process ended before the command started".into(),
                    ));
                }
            }
        };
        loop {
            match machine.recv() {
                Some(
                    MachineMessage::Stdout { id: of, line }
                    | MachineMessage::Stderr { id: of, line },
                ) if of == id => {
                    let _ = lines.send(line);
                }
                Some(MachineMessage::Exited { id: of, code }) if of == id => return Ok(code),
                Some(_) => {}
                None => {
                    return Err(machine::Error::Died(
                        "the machine's process ended before the command did".into(),
                    ));
                }
            }
        }
    });
    while let Some(line) = said.recv().await {
        report(line);
    }
    let code = ran.await??;
    drop(kill);
    if code != 0 {
        return Err(Error::Failed {
            command: command.clone(),
            code,
        });
    }

    let store = store.clone();
    let layer = tokio::task::spawn_blocking(move || written(&store, &snapshot)).await??;
    layers.extend(layer);
    Ok(())
}

/// What a machine wrote to `snapshot`, filed as a layer: the overlay's upper directory,
/// without what the overlay said of itself. `None` for a command that wrote nothing.
fn written(store: &Store, snapshot: &Snapshot) -> Result<Option<Digest>> {
    let file = File::open(&snapshot.path).map_err(|e| Error::Io {
        context: format!("opening {}", snapshot.path.display()),
        source: e,
    })?;
    // The guest's init makes it on every boot, so only a snapshot no machine booted over
    // has none.
    let Some(mut upper) = FsTree::from_ext4(file)?.subtree(b"upper")? else {
        return Ok(None);
    };
    if upper.walk().len() == 1 {
        return Ok(None);
    }
    upper.strip_overlay();
    Ok(Some(store.insert_layer(&upper)?))
}

/// The snapshot a layer's machine writes to, in a directory of the temp directory's that
/// goes with it.
struct Scratch(snapshot::Store);

impl Scratch {
    fn new() -> Result<Scratch> {
        static LAYERS: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "virtx-uvm-layer-{}-{}",
            std::process::id(),
            LAYERS.fetch_add(1, Ordering::Relaxed)
        ));
        Ok(Scratch(snapshot::Store::new(dir)?))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.0.root());
    }
}
