//! Server-Sent Events: the stream format a server answers a request with when it has
//! notifications to send before the response.
//!
//! Only what this protocol uses: `data` lines, joined, are an event's message. `event`,
//! `id`, `retry` and comment lines are read and ignored.

use bytes::Bytes;

/// One event carrying `message`. A message is a line of JSON, so one `data` line holds it.
pub fn event(message: &str) -> Bytes {
    Bytes::from(format!("data: {message}\n\n"))
}

/// Cuts a stream of bytes into events' messages, however the bytes arrive.
#[derive(Default)]
pub struct Parser {
    /// Bytes after the last complete line.
    pending: Vec<u8>,
    /// The `data` of the event being read.
    data: Option<String>,
}

impl Parser {
    /// Read `chunk`, and return the messages of the events it completes.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(chunk);
        let mut messages = Vec::new();
        while let Some(end) = self.pending.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches('\n').trim_end_matches('\r');
            if line.is_empty() {
                messages.extend(self.data.take());
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            if field == "data" {
                let value = value.strip_prefix(' ').unwrap_or(value);
                match &mut self.data {
                    Some(data) => {
                        data.push('\n');
                        data.push_str(value);
                    }
                    None => self.data = Some(value.to_owned()),
                }
            }
        }
        messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_is_its_data() {
        let mut parser = Parser::default();
        assert_eq!(parser.feed(&event(r#"{"a":1}"#)), vec![r#"{"a":1}"#]);
    }

    #[test]
    fn an_event_split_across_chunks_comes_out_once_complete() {
        let mut parser = Parser::default();
        assert!(parser.feed(b"data: {\"a\"").is_empty());
        assert!(parser.feed(b":1}\r\n").is_empty());
        assert_eq!(parser.feed(b"\r\ndata: 2\n\n"), vec![r#"{"a":1}"#, "2"]);
    }

    #[test]
    fn other_fields_and_comments_are_ignored() {
        let mut parser = Parser::default();
        let stream = b": keep-alive\n\nid: 4\nevent: message\ndata:x\ndata: y\n\n";
        assert_eq!(parser.feed(stream), vec!["x\ny"]);
    }
}
