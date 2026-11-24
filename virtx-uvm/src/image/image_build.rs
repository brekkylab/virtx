//! Building an image a [`Step`] at a time, over a base the caller has.
//!
//! A `RUN` is a machine booted over the image so far and an upper disk made empty for it,
//! running the command. What the command wrote is the overlay's upper directory on the
//! upper disk, which is a changeset already -- whiteouts and opaque directories included --
//! and becomes a layer as it is. `ENV` and `WORKDIR` change only the config, which every
//! later `RUN` runs under and the image keeps.

use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use oci_spec::image::{Digest, ImageConfiguration};
use virtx_core::Step;

use super::store::{self, Store};
use crate::fstree::{self, FsTree};
use crate::machine::wire::CtrlResp;
use crate::machine::{self, Machine, MachineConfig, MachineSpec};

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
    Fstree(#[from] fstree::Error),

    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },

    /// Blocking work moved off the runtime that panicked or was cancelled.
    #[error("building a layer: {0}")]
    Join(#[from] tokio::task::JoinError),
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
    let upper = scratch.0.join("layer.ext4");
    let made = upper.clone();
    tokio::task::spawn_blocking(move || machine::create_upper(&made, crate::DEFAULT_DISK_GIB))
        .await??;

    // From the root, as the guest's init would fail to go somewhere not there yet. Synced
    // before it is done, since the machine is killed after and what the guest had not put on
    // the upper disk would be lost.
    let workdir = machine::workdir(&config);
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
        upper: upper.clone(),
        spec: spec.clone(),
        volumes: Vec::new(),
        abin: None,
        ports: Vec::new(),
        sandbox_disk: None,
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
                Some(CtrlResp::Exited { id: of, code, .. }) if of == id => return Ok(code),
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
    let layer = tokio::task::spawn_blocking(move || written(&store, &upper)).await??;
    layers.extend(layer);
    Ok(())
}

/// What a machine wrote to the upper disk `disk`, filed as a layer: the overlay's upper
/// directory, without what the overlay said of itself. `None` for a command that wrote nothing.
fn written(store: &Store, disk: &Path) -> Result<Option<Digest>> {
    let file = File::open(disk).map_err(|e| Error::Io {
        context: format!("opening {}", disk.display()),
        source: e,
    })?;
    // The guest's init makes it on every boot, so only a disk no machine booted over
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

/// The upper disk a layer's machine writes to, in a directory of the temp directory's that
/// goes with it.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Scratch> {
        static LAYERS: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "virtx-uvm-layer-{}-{}",
            std::process::id(),
            LAYERS.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).map_err(|e| Error::Io {
            context: format!("making {}", dir.display()),
            source: e,
        })?;
        Ok(Scratch(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_workdir_is_under_the_one_before_and_an_env_set_again_takes_the_last_value() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("images")).unwrap();
        let mut image = ImageConfiguration::default();
        let mut layers = Vec::new();
        let spec = MachineSpec {
            vcpus: 1,
            ram_mib: 512,
            gpu: false,
            gpu_memory_mib: None,
            network: false,
        };
        let steps = [
            Step::Workdir("/app".into()),
            Step::Workdir("src".into()),
            Step::Env {
                key: "MODE".into(),
                value: "dev".into(),
            },
            Step::Env {
                key: "MODE_X".into(),
                value: "kept".into(),
            },
            Step::Env {
                key: "MODE".into(),
                value: "prod".into(),
            },
        ];
        for step in &steps {
            build_step(&store, &mut image, &mut layers, step, &[], &spec, |_| {})
                .await
                .unwrap();
        }
        let config = image.config().clone().unwrap();
        assert_eq!(config.working_dir().as_deref(), Some("/app/src"));
        // In no order it promises.
        let mut env = config.env().clone().unwrap();
        env.sort();
        assert_eq!(env, ["MODE=prod", "MODE_X=kept"]);
        // Neither boots a machine nor makes a layer.
        assert!(layers.is_empty());

        let absolute = Step::Workdir("/srv".into());
        build_step(
            &store,
            &mut image,
            &mut layers,
            &absolute,
            &[],
            &spec,
            |_| {},
        )
        .await
        .unwrap();
        let config = image.config().clone().unwrap();
        assert_eq!(config.working_dir().as_deref(), Some("/srv"));
    }
}
