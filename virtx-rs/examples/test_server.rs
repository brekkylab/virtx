//! A server over stdio for the clients' tests to spawn, here and in virtx-py and virtx-node:
//! `cargo build -p virtx-rs --example test_server`.
//!
//! `image.build` logs `pulling <base>`, and fails for the base `missing`; `image.list` answers
//! one image; `image.remove` fails; `sandbox.exec` runs everything as `echo`;
//! `volume.file.write` keeps what it is given by path, and `volume.file.read` reads it back, or
//! the path itself for a path never written.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use bytes::Bytes;
use virtx_core::{
    Dispatcher, ErrorObject, ImageBuild, ImageBuildOutput, ImageList, ImageListOutputItem,
    ImageRemove, Methods, Notifier, SandboxExec, SandboxExecOutput, VolumeFileRead,
    VolumeFileReadOutput, VolumeFileWrite, r#impl::stdio,
};

pub fn methods() -> Methods {
    let files: Arc<Mutex<HashMap<String, Bytes>>> = Arc::default();
    let written = Arc::clone(&files);
    let mut methods = Methods::new();
    methods
        .add::<ImageList, _, _>(|_, _| async {
            Ok(vec![ImageListOutputItem {
                reference: "a:latest".into(),
                digest: "sha256:0".into(),
                size: 1,
            }])
        })
        .add::<ImageBuild, _, _>(|args, extensions| async move {
            let notifier = extensions.get::<Notifier>().expect("a notifier");
            let base = &args.recipe.base;
            notifier.progress(format!("pulling {base}"));
            if base == "missing" {
                return Err(ErrorObject::new(1, format!("no image {base}")));
            }
            Ok(ImageBuildOutput {
                reference: args.reference.unwrap_or_else(|| "built:latest".into()),
                digest: "sha256:0".into(),
            })
        })
        .add::<ImageRemove, _, _>(|_, _| async { Err(ErrorObject::new(1, "no such image")) })
        .add::<SandboxExec, _, _>(|args, extensions| async move {
            let notifier = extensions.get::<Notifier>().expect("a notifier");
            notifier.progress(args.cmd[1..].join(" "));
            Ok(SandboxExecOutput { code: 0 })
        })
        .add::<VolumeFileWrite, _, _>(move |args, _| {
            written.lock().unwrap().insert(args.path, args.data);
            async { Ok(Default::default()) }
        })
        .add::<VolumeFileRead, _, _>(move |args, _| {
            let bytes = files.lock().unwrap().get(&args.path).cloned();
            let bytes = bytes.unwrap_or_else(|| args.path.clone().into());
            let start = (args.offset as usize).min(bytes.len());
            let end = match args.length {
                Some(length) => (start + length as usize).min(bytes.len()),
                None => bytes.len(),
            };
            async move {
                Ok(VolumeFileReadOutput {
                    data: bytes.slice(start..end),
                })
            }
        });
    methods
}

/// Serve [`methods`] on this process's stdin and stdout until stdin ends.
pub fn serve() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime
        .block_on(stdio::serve(
            Dispatcher::new(methods()),
            tokio::io::stdin(),
            tokio::io::stdout(),
        ))
        .unwrap();
}

#[allow(dead_code)]
fn main() {
    serve();
}
