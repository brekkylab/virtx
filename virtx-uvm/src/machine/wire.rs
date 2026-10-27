//! The wire between a server and the guest inside each of its machines.
//!
//! One file for both ends: the guest, built apart, includes it by path, so it names nothing
//! outside itself.
//!
//! The guest sends [`HANDSHAKE`] as soon as it has the port open; after that the host sends
//! calls and the guest answers them, each message one frame: its length as four big-endian
//! bytes, then the message as JSON. Every call carries an id the host picks and every answer
//! names it, so calls run side by side.

use std::io;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// What the guest sends first, before any frame: a virtio-console port discards what the host
/// writes while nothing in the guest has it open, so the host waits for this.
pub const HANDSHAKE: &[u8; 8] = b"VIRTXUVM";

/// The name of the virtio-console port the channel runs on.
pub const PORT_NAME: &str = "virtx-console";

/// The largest frame either end reads.
const MAX_FRAME: usize = 16 << 20;

/// A call, host to guest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToGuest {
    /// Run `cmd` in the guest's working directory, with no stdin, killing it and everything it
    /// started after `timeout_ms`. Answered with its output a line at a time, then how it ended.
    Exec {
        id: u64,
        cmd: Vec<String>,
        timeout_ms: Option<u64>,
    },
    /// Put everything written on the disks, before the machine is killed.
    Sync { id: u64 },
    /// Kill the command the `Exec` named `id` runs, and everything it started. Not answered:
    /// whatever that `Exec` still says is the host's to ignore.
    Kill { id: u64 },
}

/// What the guest sends.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FromGuest {
    /// One line a command wrote, to stdout or stderr, without its newline.
    Line { id: u64, line: String },
    /// The command ended with `code`; `128 + signal` for one a signal ended.
    Exited { id: u64, code: i32 },
    /// The command ran past its timeout, and was killed.
    TimedOut { id: u64 },
    /// The command could not be started, or not waited for.
    Failed { id: u64, message: String },
    /// The disks have what was written.
    Synced { id: u64 },
}

/// Write `message` as one frame.
pub async fn send<W: AsyncWrite + Unpin>(out: &mut W, message: &impl Serialize) -> io::Result<()> {
    let message = serde_json::to_vec(message).map_err(io::Error::other)?;
    let length = u32::try_from(message.len()).map_err(io::Error::other)?;
    out.write_all(&length.to_be_bytes()).await?;
    out.write_all(&message).await?;
    out.flush().await
}

/// The next frame's message; `None` at the end of the channel, between frames.
pub async fn recv<R: AsyncRead + Unpin, M: DeserializeOwned>(
    input: &mut R,
) -> io::Result<Option<M>> {
    let mut length = [0; 4];
    match input.read_exact(&mut length).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a frame of {length} bytes is past the {MAX_FRAME} this reads"),
        ));
    }
    let mut message = vec![0; length];
    input.read_exact(&mut message).await?;
    serde_json::from_slice(&message)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_message_comes_out_as_it_went_in() {
        let sent = [
            ToGuest::Exec {
                id: 1,
                cmd: vec!["echo".into(), "hi".into()],
                timeout_ms: Some(5),
            },
            ToGuest::Sync { id: 2 },
        ];
        let mut wire = Vec::new();
        for message in &sent {
            send(&mut wire, message).await.unwrap();
        }
        let mut input = &wire[..];
        for message in &sent {
            assert_eq!(
                recv::<_, ToGuest>(&mut input).await.unwrap().as_ref(),
                Some(message)
            );
        }
        assert_eq!(recv::<_, ToGuest>(&mut input).await.unwrap(), None);
    }
}
