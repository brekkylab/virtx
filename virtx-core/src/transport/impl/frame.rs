//! How messages follow one another in a byte stream: the server's stdin and stdout, and a
//! streamed HTTP answer.
//!
//! Each message is a frame: the self-described CBOR tag (`d9 d9 f7`) over a byte string with a
//! four-byte length (`5a` and the length, big-endian), whose bytes are the message. A stream
//! of frames is a CBOR sequence, and its fixed eight-byte header lets a reader find the next
//! frame after bytes that are not one, as a server's stray output is skipped.

use std::{collections::VecDeque, io};

use tokio::io::{AsyncRead, AsyncReadExt};

/// What every frame starts with: the tag, and the byte string's head.
const MAGIC: [u8; 4] = [0xd9, 0xd9, 0xf7, 0x5a];

/// The magic and the length.
const HEADER: usize = MAGIC.len() + 4;

/// `message` as a frame.
pub fn frame(message: &[u8]) -> Vec<u8> {
    let len = u32::try_from(message.len()).expect("a message under 4 GiB");
    let mut frame = Vec::with_capacity(HEADER + message.len());
    frame.extend_from_slice(&MAGIC);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(message);
    frame
}

/// Cuts bytes into messages, however the bytes arrive.
#[derive(Default)]
pub struct Parser {
    /// Bytes not yet read as a message.
    pending: Vec<u8>,
}

impl Parser {
    pub fn push(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    /// The next whole message, skipping any bytes before its frame; `None` until one is.
    pub fn next_message(&mut self) -> Option<Vec<u8>> {
        let Some(start) = self.pending.windows(MAGIC.len()).position(|w| w == MAGIC) else {
            // Keep what may be the start of a magic split across chunks.
            let keep = (1..MAGIC.len())
                .rev()
                .find(|&n| self.pending.ends_with(&MAGIC[..n]))
                .unwrap_or(0);
            self.pending.drain(..self.pending.len() - keep);
            return None;
        };
        self.pending.drain(..start);
        let len = self.pending.get(MAGIC.len()..HEADER)?;
        let len = u32::from_be_bytes(len.try_into().expect("four bytes")) as usize;
        let message = self.pending.get(HEADER..HEADER + len)?.to_vec();
        self.pending.drain(..HEADER + len);
        Some(message)
    }
}

/// The messages read from `R`.
pub struct Reader<R> {
    input: R,
    parser: Parser,
    chunk: Vec<u8>,
    ready: VecDeque<Vec<u8>>,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    pub fn new(input: R) -> Reader<R> {
        Reader {
            input,
            parser: Parser::default(),
            chunk: vec![0; 64 * 1024],
            ready: VecDeque::new(),
        }
    }

    /// The next message; `None` once the input ends.
    pub async fn next(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(message) = self.ready.pop_front() {
                return Ok(Some(message));
            }
            let read = self.input.read(&mut self.chunk).await?;
            if read == 0 {
                return Ok(None);
            }
            self.parser.push(&self.chunk[..read]);
            while let Some(message) = self.parser.next_message() {
                self.ready.push_back(message);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(parser: &mut Parser) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| parser.next_message()).collect()
    }

    #[test]
    fn a_frame_is_its_message() {
        let mut parser = Parser::default();
        parser.push(&frame(b"hi"));
        assert_eq!(messages(&mut parser), [b"hi"]);
    }

    #[test]
    fn a_frame_split_across_chunks_comes_out_once_complete() {
        let mut stream = frame(b"one");
        stream.extend(frame(b""));
        stream.extend(frame(b"two"));
        let mut parser = Parser::default();
        let mut read = Vec::new();
        for byte in stream {
            parser.push(&[byte]);
            read.extend(messages(&mut parser));
        }
        assert_eq!(read, [&b"one"[..], b"", b"two"]);
    }

    #[test]
    fn bytes_outside_a_frame_are_skipped() {
        let mut parser = Parser::default();
        parser.push(b"running 1 test\n\xd9");
        assert!(messages(&mut parser).is_empty());
        parser.push(&frame(b"hi")[1..]);
        parser.push(b"test result: ok\n");
        parser.push(&frame(b"bye"));
        assert_eq!(messages(&mut parser), [&b"hi"[..], b"bye"]);
    }

    #[test]
    fn a_frame_is_a_cbor_byte_string() {
        let value: ciborium::Value = ciborium::from_reader(&frame(b"hi")[..]).unwrap();
        let ciborium::Value::Tag(55799, inner) = value else {
            panic!("{value:?}");
        };
        assert_eq!(*inner, ciborium::Value::Bytes(b"hi".to_vec()));
    }
}
