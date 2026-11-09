//! The control channel between a server and each of its machines' processes: `ctrlin`, which
//! the server writes [`CtrlReq`]s into, and `ctrlout`, which it reads [`CtrlResp`]s
//! from -- everything the two say to each other once the process is spawned.
//!
//! The same frames go on between the process and the agent in the guest, over the
//! virtio-console port [`PORT_NAME`]: the process passes the host's on to the agent and the
//! agent's back, so the channel runs end to end. The agent, built apart, includes this file by
//! path.
//!
//! The I/O channel is two more pipes, `ioin` and `ioout`, which the process wires straight to
//! the virtio-console port [`IO_PORT_NAME`]: the host writes [`IoReq`]s into it and
//! reads a [`IoResp`] for each, in turn, before it sends the next.
//!
//! Each message is one frame: its length as four big-endian bytes, then the message as JSON.
//! The bytes of a file go as a frame of their own ([`send_data`]) after the message they belong
//! to: a [`Write`](IoReq::Write), and a [`Data`](IoResp::Data).

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// The largest frame either end reads.
const MAX_FRAME: usize = 16 << 20;

/// The name of the virtio-console port the channel runs on inside the machine.
pub const PORT_NAME: &str = "virtx-ctrl";

/// The name of the virtio-console port the I/O channel runs on inside the machine.
pub const IO_PORT_NAME: &str = "virtx-io";

/// The most of a file one [`Read`](IoReq::Read) or [`Write`](IoReq::Write)
/// moves.
pub const MAX_DATA: usize = 1 << 20;

/// The vsock port the agent dials guest ports from, for a published port whose program in the
/// guest listened before it was published -- a listen the host was not asked to take, which
/// stays the guest's own (`net`). The host connects, says the port as two big-endian bytes,
/// and is answered one byte, [`DIALED`] or [`NOT_DIALED`]; then it is that port's connection,
/// both ways. Clear of libkrun's own ports (620, 1024-1031, 123).
pub const DIAL_PORT: u32 = 52_000;
/// The agent's answer to a dial that reached the port in the guest.
pub const DIALED: u8 = 0;
/// The agent's answer to a dial nothing in the guest answered.
pub const NOT_DIALED: u8 = 1;

/// What the host tells the machine's process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CtrlReq {
    /// Run `cmd` in the guest, beside whatever else runs there. Answered with
    /// [`Started`](CtrlResp::Started) before the next `Exec` is, so the host that waits for
    /// it before sending another knows which it answers.
    Exec {
        cmd: Vec<String>,
        /// Its stdin a pipe, written with [`Stdin`](CtrlReq::Stdin): empty if not.
        #[serde(default)]
        stdin: bool,
    },
    /// Send `signal` to the process `id` and everything it started. Numbered as the guest's
    /// Linux numbers it, which is not every host's: `SIGUSR1` is 10 there and 30 on macOS. Not
    /// answered: what the process does with it, its [`Exited`](CtrlResp::Exited) says.
    Signal { id: u32, signal: i32 },
    /// Put what the guest has written on its disks, and end the machine. Not answered: its
    /// process ends.
    Shutdown,
    /// Run `cmd` on a terminal of its own, `cols` by `rows`, which is its stdin, stdout and
    /// stderr and its controlling terminal. Answered as [`Exec`](CtrlReq::Exec) is, in the
    /// same turn; what it writes comes back as [`Output`](CtrlResp::Output).
    ExecTty {
        cmd: Vec<String>,
        cols: u16,
        rows: u16,
    },
    /// Write `data` to the terminal of the process `id`, as typed -- or to the stdin of one
    /// [`Exec`](CtrlReq::Exec) started with it, closed after if `eof`. Not answered: a
    /// process that does not read it holds up none but itself.
    Stdin {
        id: u32,
        data: String,
        #[serde(default)]
        eof: bool,
    },
    /// Make the terminal of the process `id` `cols` by `rows`.
    Resize { id: u32, cols: u16, rows: u16 },
    /// Put what the guest has written on its disks, and say so with
    /// [`Synced`](CtrlResp::Synced): before its disks are copied as they are.
    Sync,
    /// Publish guest port `port` as those the machine booted with are, from now on: answered
    /// with [`Published`](CtrlResp::Published). The machine's process does it, not the
    /// guest, which never hears of it.
    Publish { port: u16 },
    /// Whether the process `id`, on a terminal, has another in the terminal's foreground -- a
    /// command a shell runs -- answered with [`Foreground`](CtrlResp::Foreground).
    Foreground { id: u32 },
    /// Mark its disk `mark`, as it is about to be copied: what is written from now on noted
    /// apart ([`crate::lazy::Lazy::mark`]). Answered with [`Marked`](CtrlResp::Marked).
    /// The machine's process does it; the guest never hears of it.
    Mark { mark: u64 },
    /// Make what the copy made at mark `mark` was kept as -- staged beside the disk
    /// ([`crate::lazy::stage_base`]) -- its disk's base ([`crate::lazy::Lazy::rebase`]).
    /// Answered with [`Rebased`](CtrlResp::Rebased).
    Rebase { mark: u64 },
}

/// What the machine's process tells the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CtrlResp {
    /// The machine has booted and the guest takes commands, with where on this host's loopback
    /// each published guest port is reached. Once per machine.
    Ready(Vec<(u16, SocketAddr)>),
    /// The answer to the last [`Exec`](CtrlReq::Exec): the guest's pid of the process it
    /// started, the `id` everything said of it after is under, or why it could not be started.
    Started(std::result::Result<u32, String>),
    /// A line the process `id` wrote to its stdout, without its newline.
    Stdout { id: u32, line: String },
    /// A line the process `id` wrote to its stderr, without its newline.
    Stderr { id: u32, line: String },
    /// What the guest had written is on its disks, as the last [`Sync`](CtrlReq::Sync)
    /// asked.
    Synced,
    /// What the process `id`, on a terminal, wrote there: as it came, control sequences and
    /// all, cut only between whole characters.
    Output { id: u32, data: String },
    /// The process `id` ended with `code`; `128 + signal` for one a signal ended. The last
    /// thing said of it.
    Exited { id: u32, code: i32 },
    /// A record of the machine process's `log`: whatever any Rust code in it -- libkrun, or
    /// this crate -- logs through the `log` facade, which `boot` points at `ctrlout`. Among it
    /// are the guest's console, which libkrun logs under `init_or_kernel`, and why the machine
    /// could not be built or broke, logged as an error as the process's last word -- so a
    /// process that ends before [`Ready`](CtrlResp::Ready) has said why here.
    Log { level: String, message: String },
    /// The answer to the last [`Publish`](CtrlReq::Publish): where on this host's loopback
    /// the port is reached now, or why it could not be published.
    Published(std::result::Result<(u16, SocketAddr), String>),
    /// The answer to a [`Foreground`](CtrlReq::Foreground): whether the process `id` has
    /// another in its terminal's foreground; `false` for one with no terminal, or ended.
    Foreground { id: u32, busy: bool },
    /// The answer to a [`Mark`](CtrlReq::Mark): whether its disk is marked so -- not one
    /// with no disk kept, or why it could not be.
    Marked {
        mark: u64,
        marked: std::result::Result<bool, String>,
    },
    /// The answer to a [`Rebase`](CtrlReq::Rebase): whether its disk was rebased, or why
    /// it could not be.
    Rebased {
        mark: u64,
        rebased: std::result::Result<bool, String>,
    },
}

/// What the host asks of the guest's files on the I/O channel, each answered with a
/// [`IoResp`]: what it says, or [`Failed`](IoResp::Failed). Each path is
/// absolute, as the guest's processes spell it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IoReq {
    /// What `path` is: [`IoResp::Meta`].
    Stat { path: String },
    /// The entries of the directory `path`, each by its name: [`IoResp::Entries`].
    ReadDir { path: String },
    /// Up to `len` bytes of `path` from `offset`, and never more than [`MAX_DATA`]:
    /// [`IoResp::Data`].
    Read { path: String, offset: u64, len: u64 },
    /// The bytes that follow written at `offset` in `path`, made if it is not there; without
    /// an offset the file is those bytes and nothing else. [`IoResp::Done`].
    Write { path: String, offset: Option<u64> },
    /// The directory `path`, whose parent is there. [`IoResp::Done`].
    CreateDir { path: String },
    /// `path` gone: a file, or a directory with everything in it. [`IoResp::Done`].
    Remove { path: String },
    /// `from` moved to `to`, over whatever is there. With `keep`, a file at `to` gives `from`
    /// its mode and owner first, so a file written whole beside it keeps what it was.
    /// [`IoResp::Done`].
    Rename {
        from: String,
        to: String,
        keep: bool,
    },
    /// The file `from` copied to `to`, over whatever is there. [`IoResp::Done`].
    Copy { from: String, to: String },
}

/// What the agent answers a [`IoReq`] with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IoResp {
    Meta(IoMeta),
    Entries(Vec<(String, IoMeta)>),
    /// What was read, in the frame that follows.
    Data,
    Done,
    /// Why it did not do what it was asked.
    Failed(IoFailure),
}

/// What a file or a directory is. Nothing else is shown: a link, a device or a pipe is not
/// there as far as the I/O channel says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IoMeta {
    pub is_dir: bool,
    pub len: u64,
    /// When it was last modified, since the Unix epoch.
    pub modified: Option<Duration>,
}

/// Why the guest did not do what it was asked.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum IoFailure {
    NotFound,
    Exists,
    /// What a path leads through is not a directory.
    NotADirectory,
    /// A file was asked for and a directory is there.
    IsADirectory,
    /// The guest will not have it: its permissions, a read-only mount, or a path the channel
    /// does not show.
    Forbidden,
    Full,
    Other(String),
}

/// Write `message` as one frame.
pub fn send(out: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    send_all(out, std::slice::from_ref(message))
}

/// `messages`, a frame each, written at once: one write for them all, where a write is a
/// machine's exit to its host.
pub fn send_all<M: Serialize>(out: &mut impl Write, messages: &[M]) -> io::Result<()> {
    let mut frames = Vec::new();
    for message in messages {
        let message = serde_json::to_vec(message).map_err(io::Error::other)?;
        let length = u32::try_from(message.len()).map_err(io::Error::other)?;
        frames.extend_from_slice(&length.to_be_bytes());
        frames.extend_from_slice(&message);
    }
    out.write_all(&frames)?;
    out.flush()
}

/// The next frame's message; `None` at the end of the channel, between frames.
pub fn recv<M: DeserializeOwned>(input: &mut impl Read) -> io::Result<Option<M>> {
    let mut length = [0; 4];
    match input.read_exact(&mut length) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut message = vec![0; frame_length(length)?];
    input.read_exact(&mut message)?;
    decode(&message).map(Some)
}

/// Write `data` as one frame, as it is.
pub fn send_data(out: &mut impl Write, data: &[u8]) -> io::Result<()> {
    let length = u32::try_from(data.len()).map_err(io::Error::other)?;
    out.write_all(&length.to_be_bytes())?;
    out.write_all(data)?;
    out.flush()
}

/// The next frame, as it is: the bytes [`send_data`] wrote.
pub fn recv_data(input: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut length = [0; 4];
    input.read_exact(&mut length)?;
    let mut data = vec![0; frame_length(length)?];
    input.read_exact(&mut data)?;
    Ok(data)
}

fn frame_length(length: [u8; 4]) -> io::Result<usize> {
    let length = u32::from_be_bytes(length) as usize;
    match length <= MAX_FRAME {
        true => Ok(length),
        false => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a frame of {length} bytes is past the {MAX_FRAME} this reads"),
        )),
    }
}

fn decode<M: DeserializeOwned>(message: &[u8]) -> io::Result<M> {
    serde_json::from_slice(message).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_comes_out_as_it_went_in() {
        let sent = [
            CtrlResp::Ready(vec![(80, "127.0.0.1:49152".parse().unwrap())]),
            CtrlResp::Stdout {
                id: 0,
                line: "hello".into(),
            },
            CtrlResp::Log {
                level: "ERROR".into(),
                message: "no".into(),
            },
        ];
        let mut wire = Vec::new();
        for message in &sent {
            send(&mut wire, message).unwrap();
        }
        let mut input = &wire[..];
        for message in &sent {
            assert_eq!(
                recv::<CtrlResp>(&mut input).unwrap().as_ref(),
                Some(message)
            );
        }
        assert_eq!(recv::<CtrlResp>(&mut input).unwrap(), None);
    }

    #[test]
    fn a_file_s_bytes_follow_their_message_as_they_are() {
        let write = IoReq::Write {
            path: "/a b".into(),
            offset: Some(3),
        };
        let mut wire = Vec::new();
        send(&mut wire, &write).unwrap();
        send_data(&mut wire, b"\0\xffbytes").unwrap();
        let mut input = &wire[..];
        assert_eq!(recv::<IoReq>(&mut input).unwrap(), Some(write));
        assert_eq!(recv_data(&mut input).unwrap(), b"\0\xffbytes");
        assert!(recv_data(&mut input).is_err());
    }
}
