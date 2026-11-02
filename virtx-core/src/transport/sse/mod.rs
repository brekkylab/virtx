//! A job's log, which `PROTOCOL.md`'s Events has a server send as an event stream: each entry
//! numbered, read from its start or from after the entry a client last saw. The client's end is
//! [`client`].

#[cfg(feature = "client")]
pub(crate) mod client;

use serde::{Deserialize, Serialize};

/// One entry of a job's log, as its event stream carries it: its number in the log, `1` for
/// the first, and the entry. As JSON, the entry with `id` beside its member, as
/// `{"id": 1, "stdout": "hello"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry<E> {
    pub id: u64,
    #[serde(flatten)]
    pub entry: E,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Line {
        Stdout(String),
        Truncated {},
    }

    #[test]
    fn an_entry_carries_its_id_beside_its_member() {
        let cases = [
            (
                LogEntry {
                    id: 1,
                    entry: Line::Stdout("hi".into()),
                },
                json!({"id": 1, "stdout": "hi"}),
            ),
            (
                LogEntry {
                    id: 2,
                    entry: Line::Truncated {},
                },
                json!({"id": 2, "truncated": {}}),
            ),
        ];
        for (entry, value) in cases {
            assert_eq!(serde_json::to_value(&entry).unwrap(), value);
            assert_eq!(
                serde_json::from_value::<LogEntry<Line>>(value).unwrap(),
                entry
            );
        }
    }
}
