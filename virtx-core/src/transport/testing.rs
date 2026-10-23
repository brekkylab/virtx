//! The methods the tests serve, and a client of them in memory. `images.build` sends
//! `pulling <base>` as its progress, and fails for the base `missing`.

use std::io;

use tokio::io::{
    AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, Lines, ReadHalf, WriteHalf,
};

use crate::{
    method::{ImageBuild, ImageBuildOutput, Request},
    transport::{
        Dispatcher, ErrorObject, Methods, Notifier, TransportClient,
        r#impl::{BoxFuture, Incoming, Outgoing, stdio, streamable_http::SessionId},
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
    TransportClient::new(Pipe(write), PipeLines(BufReader::new(read).lines()))
}

struct Pipe(WriteHalf<DuplexStream>);

impl Outgoing for Pipe {
    fn send(&mut self, message: String) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move {
            self.0.write_all(message.as_bytes()).await?;
            self.0.write_all(b"\n").await
        })
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

struct PipeLines(Lines<BufReader<ReadHalf<DuplexStream>>>);

impl Incoming for PipeLines {
    fn receive(&mut self) -> BoxFuture<'_, io::Result<String>> {
        Box::pin(async move {
            self.0
                .next_line()
                .await?
                .ok_or(io::ErrorKind::UnexpectedEof.into())
        })
    }
}
