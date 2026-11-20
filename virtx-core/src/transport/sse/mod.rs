//! A job's log, which the protocol's Events has a server send as an event stream: each entry
//! numbered, read from its start or from after the entry a client last saw. The client's end is
//! [`client`], the server's [`server`].

#[cfg(feature = "client")]
pub(crate) mod client;
#[cfg(feature = "server")]
pub(crate) mod server;

use std::fmt;

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{self, IgnoredAny, MapAccess, Visitor},
    ser::SerializeMap,
};

/// One entry of a job's log, as its event stream carries it: its number in the log, `1` for
/// the first, and the entry. As JSON, the entry with `id` beside its member, as
/// `{"id": 1, "stdout": "hello"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry<E> {
    pub id: u64,
    #[serde(flatten)]
    pub entry: E,
}

/// A log entry of a kind this version does not know, by the member it was sent as: to be passed
/// over, its `id` still counting where a log is read from. What it carried is not kept, so it is
/// written back as that member alone, as `{"kind": null}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownEntry {
    pub kind: String,
}

impl Serialize for UnknownEntry {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry(&self.kind, &())?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for UnknownEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct OneMember;

        impl<'de> Visitor<'de> for OneMember {
            type Value = UnknownEntry;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object of one member")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<UnknownEntry, A::Error> {
                let Some((kind, IgnoredAny)) = map.next_entry::<String, IgnoredAny>()? else {
                    return Err(de::Error::invalid_length(0, &self));
                };
                if map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {
                    return Err(de::Error::invalid_length(2, &self));
                }
                Ok(UnknownEntry { kind })
            }
        }

        deserializer.deserialize_map(OneMember)
    }
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
