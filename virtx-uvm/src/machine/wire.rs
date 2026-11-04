//! The control channel between a server and each of its machines' processes: `ctrlin`, which
//! the server writes [`HostMessage`]s into, and `ctrlout`, which it reads [`MachineMessage`]s
//! from -- everything the two say to each other once the process is spawned.
//!
//! The same frames go on between the process and the agent in the guest, over the
//! virtio-console port [`PORT_NAME`]: the process passes the host's on to the agent and the
//! agent's back, so the channel runs end to end. The agent, built apart, includes this file by
//! path.
//!
//! Each message is one frame: its length as four big-endian bytes, then the message as JSON.

use std::io::{self, Read, Write};
use std::net::SocketAddr;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// The largest frame either end reads.
const MAX_FRAME: usize = 16 << 20;

/// The name of the virtio-console port the channel runs on inside the machine.
pub const PORT_NAME: &str = "virtx-ctrl";

/// What the host tells the machine's process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostMessage {
    /// Run `cmd` in the guest, beside whatever else runs there. Answered with
    /// [`Started`](MachineMessage::Started) before the next `Exec` is, so the host that waits for
    /// it before sending another knows which it answers.
    Exec { cmd: Vec<String> },
    /// Send `signal` to the process `id` and everything it started. Numbered as the guest's
    /// Linux numbers it, which is not every host's: `SIGUSR1` is 10 there and 30 on macOS. Not
    /// answered: what the process does with it, its [`Exited`](MachineMessage::Exited) says.
    Signal { id: u32, signal: i32 },
    /// Put what the guest has written on its disks, and end the machine. Not answered: its
    /// process ends.
    Shutdown,
}

/// What the machine's process tells the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MachineMessage {
    /// The machine has booted and the guest takes commands, with where on this host's loopback
    /// each published guest port is reached. Once per machine.
    Ready(Vec<(u16, SocketAddr)>),
    /// The answer to the last [`Exec`](HostMessage::Exec): the guest's pid of the process it
    /// started, the `id` everything said of it after is under, or why it could not be started.
    Started(std::result::Result<u32, String>),
    /// A line the process `id` wrote to its stdout, without its newline.
    Stdout { id: u32, line: String },
    /// A line the process `id` wrote to its stderr, without its newline.
    Stderr { id: u32, line: String },
    /// The process `id` ended with `code`; `128 + signal` for one a signal ended. The last
    /// thing said of it.
    Exited { id: u32, code: i32 },
    /// A record of the machine process's `log`: whatever any Rust code in it -- libkrun, or
    /// this crate -- logs through the `log` facade, which `boot` points at `ctrlout`. Among it
    /// are the guest's console, which libkrun logs under `init_or_kernel`, and why the machine
    /// could not be built or broke, logged as an error as the process's last word -- so a
    /// process that ends before [`Ready`](MachineMessage::Ready) has said why here.
    Log { level: String, message: String },
}

/// Write `message` as one frame.
pub fn send(out: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    let message = serde_json::to_vec(message).map_err(io::Error::other)?;
    let length = u32::try_from(message.len()).map_err(io::Error::other)?;
    out.write_all(&length.to_be_bytes())?;
    out.write_all(&message)?;
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
            MachineMessage::Ready(vec![(80, "127.0.0.1:49152".parse().unwrap())]),
            MachineMessage::Stdout {
                id: 0,
                line: "hello".into(),
            },
            MachineMessage::Log {
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
                recv::<MachineMessage>(&mut input).unwrap().as_ref(),
                Some(message)
            );
        }
        assert_eq!(recv::<MachineMessage>(&mut input).unwrap(), None);
    }
}
