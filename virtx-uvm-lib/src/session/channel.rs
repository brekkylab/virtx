//! The console channel: the wire between the process that decided to make a machine and the
//! process that became one.
//!
//! A console session is a virtio-console port, and a port is a thing the host end of which is
//! a descriptor -- see [`Session::port`](super::Session::port). This module is where that
//! descriptor comes from, both ends of it, because the two ends have to agree about something
//! neither platform spells the same way.
//!
//! # The two shapes
//!
//! On unix it is one `AF_UNIX` stream: the server binds a path, the boot connects to it, and
//! the single descriptor carries both directions because a socket is bidirectional.
//!
//! On Windows it is **two named pipes**, and the second one is not symmetry for its own sake.
//! libkrun's Windows console does blocking `ReadFile` on the port's input handle from one
//! thread and blocking `WriteFile` on its output handle from another, with no `OVERLAPPED` on
//! either call -- which means both handles have to be synchronous. Two handles onto one
//! synchronous file object serialize against each other, so a read parked waiting for the
//! guest would hold the write that was supposed to wake it: the protocol deadlocks on its
//! first call. Two pipes are two file objects, and the two directions stop being able to
//! block each other.
//!
//! An `AF_UNIX` socket would have been the smaller change -- Windows has had them since 1803
//! -- but a socket is not a handle `ReadFile` will take, and Rust's standard library has no
//! Windows spelling for one anyway.
//!
//! # What a boot is told
//!
//! [`Listener::bind`] answers with the name to hand over, rather than taking one. On unix
//! that is the socket's path, under the directory the caller named; on Windows it is a pipe
//! name, which is not a path at all and is not under anything. Making it the listener's to
//! choose is what keeps the difference here instead of in the command line that carries it.

use std::path::Path;

/// The server's end: bound before a boot is spawned, because a boot connects as its first act
/// and there is nothing to retry against if it is not there yet.
pub struct Listener {
    #[cfg(unix)]
    listener: tokio::net::UnixListener,
    /// The pipe the guest writes down, which is this end's to read.
    #[cfg(windows)]
    incoming: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
    /// The pipe this end writes down, which is the guest's to read.
    #[cfg(windows)]
    outgoing: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
}

/// The half of a connected channel a server reads the guest's answers from.
#[cfg(unix)]
pub type Incoming = tokio::net::unix::OwnedReadHalf;
#[cfg(windows)]
pub type Incoming = tokio::net::windows::named_pipe::NamedPipeServer;

/// The half of a connected channel a server writes its calls down.
#[cfg(unix)]
pub type Outgoing = tokio::net::unix::OwnedWriteHalf;
#[cfg(windows)]
pub type Outgoing = tokio::net::windows::named_pipe::NamedPipeServer;

impl Listener {
    /// Bind a channel in `dir`, and answer with it beside the name a boot has to be told to
    /// reach it.
    ///
    /// `dir` has to exist. It is where the channel goes on a host whose channels are files,
    /// and is ignored on one whose channels are not -- a Windows pipe lives in a namespace of
    /// its own, so what makes the name unique there is this process and a counter rather than
    /// the directory a caller picked.
    pub fn bind(dir: &Path) -> std::io::Result<(Listener, std::ffi::OsString)> {
        #[cfg(unix)]
        {
            let at = dir.join("port.sock");
            let listener = tokio::net::UnixListener::bind(&at)?;
            Ok((Listener { listener }, at.into_os_string()))
        }

        #[cfg(windows)]
        {
            use std::sync::atomic::{AtomicU64, Ordering};
            use tokio::net::windows::named_pipe::ServerOptions;

            let _ = dir;

            // A counter beside the pid because a pipe is not a file: nothing removes it and
            // nothing collides with it on disk, so the only thing keeping two channels in one
            // process apart is the name. `first_pipe_instance` then refuses to create a
            // second server under a name already taken, which turns a collision into an error
            // here rather than a boot that connects to the wrong machine.
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let base = format!(
                r"\\.\pipe\virtx-uvm-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );

            let incoming = ServerOptions::new()
                .first_pipe_instance(true)
                .create(from_guest(&base))?;
            let outgoing = ServerOptions::new()
                .first_pipe_instance(true)
                .create(to_guest(&base))?;

            Ok((
                Listener {
                    incoming: Some(incoming),
                    outgoing: Some(outgoing),
                },
                base.into(),
            ))
        }
    }

    /// Wait for the boot to connect, and answer with the two halves of the channel it opened.
    ///
    /// Cancel-safe, so a caller may poll this beside a timer and a look at the child it is
    /// waiting for -- which is the only way to tell a boot that could not start from one that
    /// is merely slow.
    pub async fn accept(&mut self) -> std::io::Result<(Incoming, Outgoing)> {
        #[cfg(unix)]
        {
            let (stream, _) = self.listener.accept().await?;
            Ok(stream.into_split())
        }

        #[cfg(windows)]
        {
            // Both, and in whichever order they arrive: the boot opens two pipes and the
            // order it opens them in is not something this end should have to know.
            let (incoming, outgoing) = match (&self.incoming, &self.outgoing) {
                (Some(incoming), Some(outgoing)) => (incoming, outgoing),
                _ => {
                    return Err(std::io::Error::other(
                        "the console channel has already been accepted",
                    ));
                }
            };
            tokio::try_join!(incoming.connect(), outgoing.connect())?;
            Ok((
                self.incoming.take().expect("just borrowed"),
                self.outgoing.take().expect("just borrowed"),
            ))
        }
    }
}

/// The boot's end: the descriptors a [`Session`](super::Session) hands the console device.
///
/// Held for as long as the machine runs, which is the rest of the process -- libkrun borrows
/// what is behind it rather than taking it, so dropping this while a guest is up takes the
/// console session down under it.
pub struct Channel {
    #[cfg(unix)]
    stream: std::os::unix::net::UnixStream,
    /// What the guest reads: this end of the pipe the server writes down.
    #[cfg(windows)]
    input: std::fs::File,
    /// What the guest writes: this end of the pipe the server reads.
    #[cfg(windows)]
    output: std::fs::File,
}

/// What [`Session::port`](super::Session::port) keeps: the channel's descriptors as plain
/// numbers, so that a `Session` stays something a caller can move between threads.
///
/// One on unix, where a socket is both directions; two on Windows, where it is not.
#[cfg(unix)]
pub(crate) type Raw = std::os::fd::RawFd;
#[cfg(windows)]
pub(crate) type Raw = (isize, isize);

impl Channel {
    /// Connect to what a server bound and named -- the `--channel` a boot is started with.
    pub fn connect(named: &Path) -> std::io::Result<Channel> {
        #[cfg(unix)]
        {
            Ok(Channel {
                stream: std::os::unix::net::UnixStream::connect(named)?,
            })
        }

        #[cfg(windows)]
        {
            let base = named.to_str().ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "a pipe name has to be utf-8",
                )
            })?;

            // Read and write on each, although each carries one direction: a pipe created
            // duplex refuses a client that asks for less than it offers.
            let open = |name: String| {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(name)
            };
            Ok(Channel {
                output: open(from_guest(base))?,
                input: open(to_guest(base))?,
            })
        }
    }

    /// The descriptors behind this, for the one caller that has to name them to libkrun.
    pub(crate) fn raw(&self) -> Raw {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;
            self.stream.as_raw_fd()
        }

        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            (
                self.input.as_raw_handle() as isize,
                self.output.as_raw_handle() as isize,
            )
        }
    }
}

/// The pipe the guest writes down. Named from the guest's side, like the handles a console
/// port is built from, so that the two ends read the same way round.
#[cfg(windows)]
fn from_guest(base: &str) -> String {
    format!("{base}-from-guest")
}

/// The pipe the guest reads.
#[cfg(windows)]
fn to_guest(base: &str) -> String {
    format!("{base}-to-guest")
}
