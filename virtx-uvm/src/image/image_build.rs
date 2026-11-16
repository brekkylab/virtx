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

use oci_spec::image::{Digest, HistoryBuilder, ImageConfiguration};
use virtx_core::{Recipe, Step};

use super::store::{self, Store};
use crate::fstree::{self, FsTree};
use crate::machine::wire::CtrlResp;
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

/// The label a built image keeps the recipe it was built from under, as its JSON.
pub const RECIPE_LABEL: &str = "sh.virtx.recipe";

/// `recipe` kept in the config of `image`, built from it: under [`RECIPE_LABEL`].
pub fn recipe_kept(image: &mut ImageConfiguration, recipe: &Recipe) {
    let mut config = image.config().clone().unwrap_or_default();
    let mut labels = config.labels().clone().unwrap_or_default();
    labels.insert(
        RECIPE_LABEL.into(),
        serde_json::to_string(recipe).expect("a recipe serializes"),
    );
    config.set_labels(Some(labels));
    image.set_config(Some(config));
}

/// The recipe kept in `image`'s config let go of: an image made of it otherwise -- a sandbox
/// committed -- was not built from it.
pub fn recipe_forgotten(image: &mut ImageConfiguration) {
    let Some(mut config) = image.config().clone() else {
        return;
    };
    if let Some(labels) = config.labels_mut() {
        labels.remove(RECIPE_LABEL);
    }
    image.set_config(Some(config));
}

/// What made `image` as it is now -- `created_by`, as a Dockerfile's instruction, or what
/// committed it -- kept in its history, as `docker history` reads it: with whether it added a
/// layer, of the `layers` it has now. Not kept where its history did not already tell each layer
/// apart -- one its base kept none of -- as it would then name the wrong ones.
pub fn history_kept(
    image: &mut ImageConfiguration,
    created_by: String,
    layered: bool,
    layers: usize,
) {
    let mut history = image.history().clone().unwrap_or_default();
    let told = history
        .iter()
        .filter(|h| !h.empty_layer().unwrap_or(false))
        .count();
    if told + usize::from(layered) != layers {
        return;
    }
    let Ok(entry) = HistoryBuilder::default()
        .created(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .created_by(created_by)
        .comment("virtx")
        .empty_layer(!layered)
        .build()
    else {
        return;
    };
    history.push(entry);
    image.set_history(Some(history));
}

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
        snapshot_held: false,
        spec: spec.clone(),
        volumes: Vec::new(),
        abin: None,
        ports: Vec::new(),
        chunks: None,
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
        machine.exec(cmd, false)?;
        let id = loop {
            match machine.recv() {
                Some(CtrlResp::Started(started)) => {
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
                Some(CtrlResp::Stdout { id: of, line } | CtrlResp::Stderr { id: of, line })
                    if of == id =>
                {
                    let _ = lines.send(line);
                }
                Some(CtrlResp::Exited { id: of, code }) if of == id => return Ok(code),
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
    written_layer(store, &snapshot.path)
}

/// What machines wrote to the snapshot disk at `disk` -- a copy of one, or one no machine
/// runs over -- filed in `store` as a layer, as a `RUN` files what it wrote: how a sandbox is
/// committed to an image. `None` for a disk nothing was written to.
pub fn written_layer(store: &Store, disk: &std::path::Path) -> Result<Option<Digest>> {
    let file = File::open(disk).map_err(|e| Error::Io {
        context: format!("opening {}", disk.display()),
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

#[cfg(test)]
mod tests {
    use oci_spec::image::{History, HistoryBuilder};

    use super::*;

    fn layered(created_by: &str) -> History {
        HistoryBuilder::default()
            .created_by(created_by)
            .build()
            .unwrap()
    }

    #[test]
    fn a_built_image_keeps_its_recipe_and_a_committed_one_does_not() {
        let recipe = Recipe::new("alpine:3.20")
            .step("apk add --no-cache jq")
            .step(Step::env("TZ", "UTC"));
        let mut image = ImageConfiguration::default();
        recipe_kept(&mut image, &recipe);
        let kept =
            image.config().as_ref().unwrap().labels().as_ref().unwrap()[RECIPE_LABEL].clone();
        assert_eq!(serde_json::from_str::<Recipe>(&kept).unwrap(), recipe);

        recipe_forgotten(&mut image);
        assert!(
            !image
                .config()
                .as_ref()
                .unwrap()
                .labels()
                .as_ref()
                .unwrap()
                .contains_key(RECIPE_LABEL)
        );
    }

    #[test]
    fn each_step_is_kept_in_the_history_where_it_tells_each_layer_apart() {
        // A base of one layer, its history telling it.
        let mut image = ImageConfiguration::default();
        image.set_history(Some(vec![layered("ADD rootfs /")]));
        history_kept(&mut image, "RUN apk add jq".into(), true, 2);
        history_kept(&mut image, "ENV TZ=UTC".into(), false, 2);
        let history = image.history().clone().unwrap();
        let told: Vec<_> = history
            .iter()
            .map(|h| {
                (
                    h.created_by().clone().unwrap(),
                    h.empty_layer().unwrap_or(false),
                )
            })
            .collect();
        assert_eq!(
            told,
            [
                ("ADD rootfs /".to_string(), false),
                ("RUN apk add jq".to_string(), false),
                ("ENV TZ=UTC".to_string(), true),
            ]
        );

        // A base whose history tells none of its layers: none is told after it either.
        let mut bare = ImageConfiguration::default();
        history_kept(&mut bare, "RUN apk add jq".into(), true, 3);
        assert!(bare.history().is_none());
        // From scratch: every one.
        let mut scratch = ImageConfiguration::default();
        history_kept(&mut scratch, "RUN true".into(), true, 1);
        assert_eq!(scratch.history().as_ref().unwrap().len(), 1);
    }
}
