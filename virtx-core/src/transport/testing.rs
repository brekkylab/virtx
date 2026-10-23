//! The methods the tests serve, and a client of them in memory. `image.build` sends
//! `pulling <base>` as its progress, and fails for the base `missing`. `volume.file.read` reads
//! the path itself, as though each file held its own path. `sandbox.exec` runs everything as
//! `echo`.

use std::io;

use tokio::io::{AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};

use crate::{
    method::{
        ImageBuild, ImageBuildOutput, Request, SandboxExec, SandboxExecOutput, VolumeFileRead,
        VolumeFileReadOutput,
    },
    transport::{
        Dispatcher, ErrorObject, Methods, Notifier, TransportClient,
        r#impl::{
            BoxFuture, Incoming, Outgoing,
            frame::{Reader, frame},
            stdio,
            streamable_http::SessionId,
        },
    },
};

/// `echo`: answers the text it is given.
pub struct Echo;

impl Request for Echo {
    const NAME: &'static str = "echo";
    type Args = (String,);
    type Output = String;
}

/// `count`: sends 1 up to the number it is given as its progress, then answers it.
pub struct Count;

impl Request for Count {
    const NAME: &'static str = "count";
    type Args = (u32,);
    type Output = u32;
}

/// `session`: answers the id of the Streamable HTTP session the request came in.
pub struct Session;

impl Request for Session {
    const NAME: &'static str = "session";
    type Args = [(); 0];
    type Output = String;
}

pub fn methods() -> Methods {
    let mut methods = Methods::new();
    methods
        .add::<Echo, _, _>(|(text,), _| async move { Ok(text) })
        .add::<Count, _, _>(|(to,), extensions| async move {
            let notifier = extensions.get::<Notifier>().expect("a notifier");
            for i in 1..=to {
                notifier.progress(i.to_string());
                tokio::task::yield_now().await;
            }
            Ok(to)
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
        .add::<SandboxExec, _, _>(|args, _| async move {
            let stdout = format!("{}\n", args.cmd[1..].join(" "));
            Ok(SandboxExecOutput {
                code: 0,
                stdout: stdout.into(),
                stderr: Default::default(),
                truncated: false,
            })
        })
        .add::<VolumeFileRead, _, _>(|args, _| async move {
            let bytes = args.path.into_bytes();
            let start = (args.offset as usize).min(bytes.len());
            let end = match args.length {
                Some(length) => (start + length as usize).min(bytes.len()),
                None => bytes.len(),
            };
            Ok(VolumeFileReadOutput {
                data: bytes[start..end].to_vec().into(),
            })
        })
        .add::<Session, _, _>(|_, extensions| async move {
            Ok(extensions.get::<SessionId>().expect("a session").0.clone())
        });
    methods
}

/// A client of [`methods`], served on this runtime over an in-memory pipe.
pub fn connect() -> TransportClient {
    let (client, server) = tokio::io::duplex(64 * 1024);
    let (input, output) = tokio::io::split(server);
    tokio::spawn(stdio::serve(Dispatcher::new(methods()), input, output));
    let (read, write) = tokio::io::split(client);
    TransportClient::new(Pipe(write), PipeFrames(Reader::new(read)))
}

struct Pipe(WriteHalf<DuplexStream>);

impl Outgoing for Pipe {
    fn send(&mut self, message: Vec<u8>) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move { self.0.write_all(&frame(&message)).await })
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

struct PipeFrames(Reader<ReadHalf<DuplexStream>>);

impl Incoming for PipeFrames {
    fn receive(&mut self) -> BoxFuture<'_, io::Result<Vec<u8>>> {
        Box::pin(async move {
            self.0
                .next()
                .await?
                .ok_or(io::ErrorKind::UnexpectedEof.into())
        })
    }
}
