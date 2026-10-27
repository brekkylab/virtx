//! The control channel between a server and each of its machines' processes: `ctlin`, which
//! the server writes [`HostMessage`]s into, and `ctlout`, which it reads [`MachineMessage`]s
//! from. The guest's own stdin, stdout and stderr are the process's, apart from this.
//!
//! Each message is one frame: its length as four big-endian bytes, then the message as JSON.

use std::io::{self, Read, Write};
use std::net::SocketAddr;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// The largest frame either end reads.
const MAX_FRAME: usize = 16 << 20;

/// What the host tells the machine's process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostMessage {
    /// End the machine. Nothing is synced first: the process just exits.
    Stop,
}

/// What the machine's process tells the host, apart from the guest's own output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MachineMessage {
    /// Where on this host's loopback each published guest port is reached.
    Published(Vec<(u16, SocketAddr)>),
    /// A line the guest's console printed: the kernel's, or the init's before it ran the
    /// command.
    Console(String),
    /// What libkrun logged of its own.
    Log { level: String, message: String },
    /// The machine could not be built, or broke once it was, and why. The last thing it says.
    NotBuilt(String),
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
            MachineMessage::Published(vec![(80, "127.0.0.1:49152".parse().unwrap())]),
            MachineMessage::Console("hello".into()),
            MachineMessage::NotBuilt("no".into()),
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
