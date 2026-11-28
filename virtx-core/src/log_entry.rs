//! A job's log's entries as JSON has them: each numbered, and each of a kind its log's entry enum
//! names, open to kinds a later version of the protocol adds: an entry under a name its enum
//! derives is read as that kind, and one under any other as its `Other`.

use std::cell::Cell;

use serde::{
    Deserialize, Deserializer, Serialize, Serializer, de, forward_to_deserialize_any,
    ser::SerializeMap,
};
use serde_json::{Map, Value};

/// One entry of a job's log, as its event stream carries it: its number in the log, `1` for
/// the first, and the entry. As JSON, the entry with `id` beside its member, as
/// `{"id": 1, "stdout": "hello"}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEntry<E> {
    pub id: u64,
    #[serde(flatten)]
    pub entry: E,
}

#[cfg(feature = "client")]
impl<E: de::DeserializeOwned> LogEntry<E> {
    /// `event`, as a job's log's event stream sends an entry, and whether it is the log's last:
    /// the one with an `end` member.
    pub(crate) fn read(
        event: http_over_x::sse::Event<Value>,
    ) -> Result<(LogEntry<E>, bool), crate::Error> {
        let end = event.data.get("end").is_some();
        let entry =
            serde_json::from_value(event.data).map_err(|e| crate::Error::Client(e.to_string()))?;
        Ok((
            LogEntry {
                id: event.id,
                entry,
            },
            end,
        ))
    }
}

/// The log of the job at `job`, a path under the server's base, after its `after`th entry if
/// given, until its `end`: read on where its stream breaks, and where the server ends it before
/// its `end`, at once after entries came, and after none only while the job is `running`, so a
/// reader asking after an ended log's last entry gets nothing more, and one whose job was
/// removed that job's `404`.
#[cfg(feature = "client")]
pub(crate) fn log<'a, E: de::DeserializeOwned + Send + 'a>(
    client: &'a crate::Client,
    job: String,
    after: Option<u64>,
) -> impl futures_util::Stream<Item = Result<LogEntry<E>, crate::Error>> + Send + 'a {
    use futures_util::StreamExt as _;

    let path = format!("{job}/events");
    let reopen = move |last: Option<&http_over_x::sse::Event<Value>>| {
        let ended = last.map(|last| last.data.get("end").is_some());
        let job = job.clone();
        async move {
            match ended {
                Some(ended) => Ok(!ended),
                None => {
                    let read: Value = client.wire().get(&job, client.headers()).await?;
                    Ok(read["running"].as_bool().unwrap_or(false))
                }
            }
        }
    };
    http_over_x::sse::events(client.wire(), path, client.headers(), after, reopen)
        .map(|event| Ok(LogEntry::read(event?)?.0))
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

/// A log's entry enum: its kinds as serde derives them (`#[serde(remote = "Self")]`, its `Other`
/// skipped), and an `Other` for a kind this version does not know.
pub(crate) trait Entry: Sized {
    /// One of its kinds, as derived.
    fn kind<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error>;

    /// One of a kind this version does not know.
    fn other(entry: UnknownEntry) -> Self;
}

/// `Serialize`, `Deserialize` and [`Entry`] for an entry enum derived with
/// `#[serde(remote = "Self")]`, its `Other(UnknownEntry)` skipped: `Other` written as
/// [`UnknownEntry`] is, and read by [`read_entry`].
macro_rules! open_entry {
    ($name:ident) => {
        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                match self {
                    $name::Other(entry) => entry.serialize(serializer),
                    kind => $name::serialize(kind, serializer),
                }
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                $crate::log_entry::read_entry(deserializer)
            }
        }

        impl $crate::log_entry::Entry for $name {
            fn kind<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                $name::deserialize(deserializer)
            }

            fn other(entry: $crate::UnknownEntry) -> Self {
                $name::Other(entry)
            }
        }
    };
}
pub(crate) use open_entry;

/// An `E` read from an object of one member: as the kind its member names where `E` derives one
/// of that name, failing as that kind does; as its `Other` where it does not.
pub(crate) fn read_entry<'de, D: Deserializer<'de>, E: Entry>(
    deserializer: D,
) -> Result<E, D::Error> {
    let entry = Map::<String, Value>::deserialize(deserializer)?;
    let mut names = entry.keys();
    let (Some(kind), None) = (names.next(), names.next()) else {
        return Err(de::Error::invalid_length(
            entry.len(),
            &"an object of one member",
        ));
    };
    if !kinds::<E>().contains(&kind.as_str()) {
        let kind = kind.clone();
        return Ok(E::other(UnknownEntry { kind }));
    }
    E::kind(Value::Object(entry)).map_err(de::Error::custom)
}

/// The kinds `E` derives, as serde names them: what its derived reading hands
/// `deserialize_enum`, asked of it once and then stopped.
fn kinds<E: Entry>() -> &'static [&'static str] {
    struct Asked<'a>(&'a Cell<&'static [&'static str]>);

    impl<'de> Deserializer<'de> for Asked<'_> {
        type Error = de::value::Error;

        fn deserialize_any<V: de::Visitor<'de>>(self, _: V) -> Result<V::Value, Self::Error> {
            Err(de::Error::custom("only its kinds are asked"))
        }

        fn deserialize_enum<V: de::Visitor<'de>>(
            self,
            _: &'static str,
            variants: &'static [&'static str],
            _: V,
        ) -> Result<V::Value, Self::Error> {
            self.0.set(variants);
            Err(de::Error::custom("only its kinds are asked"))
        }

        forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string bytes byte_buf
            option unit unit_struct newtype_struct seq tuple tuple_struct map struct identifier
            ignored_any
        }
    }

    let kinds = Cell::new(&[][..]);
    let _ = E::kind(Asked(&kinds));
    kinds.get()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{ImageJobEntry, SandboxJobEntry};

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

    #[test]
    fn an_entry_enum_s_kinds_are_what_it_derives_and_not_its_other() {
        let sandbox = kinds::<SandboxJobEntry>();
        assert!(
            sandbox.contains(&"end") && !sandbox.contains(&"other"),
            "{sandbox:?}"
        );
        let image = kinds::<ImageJobEntry>();
        assert!(
            image.contains(&"log") && !image.contains(&"other"),
            "{image:?}"
        );
    }

    #[test]
    fn a_known_kind_that_does_not_read_says_why() {
        let refused = serde_json::from_value::<SandboxJobEntry>(json!({"stdout": 5})).unwrap_err();
        assert_eq!(
            refused.to_string(),
            "invalid type: integer `5`, expected a string"
        );
    }

    #[test]
    fn an_entry_is_an_object_of_one_member() {
        let two = json!({"id": 1, "stdout": "a", "stderr": "b"});
        assert!(serde_json::from_value::<LogEntry<SandboxJobEntry>>(two).is_err());
    }

    /// A log whose stream the server ends before its `end` is read on: after its last entry,
    /// where entries came, so one cut on the way reads to its end and one whose job was removed
    /// ends with that job's `404`; and where none came past where it was asked, only while its
    /// job runs, so a reader asking after an ended log's last entry gets nothing more, at once.
    #[cfg(feature = "server")]
    #[tokio::test(start_paused = true)]
    async fn a_log_ended_before_its_end_is_read_on_while_its_job_is_there() {
        use std::sync::{Arc, Mutex};

        use axum::{Json, Router, response::IntoResponse as _, routing::get};
        use futures_util::StreamExt as _;
        use http::{HeaderMap, StatusCode};

        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let noted = asked.clone();
        let events = move |axum::extract::Path(job): axum::extract::Path<String>,
                           headers: HeaderMap| {
            let after = headers
                .get("last-event-id")
                .map_or("-".to_owned(), |v| v.to_str().unwrap().to_owned());
            let opened = {
                let mut asked = noted.lock().unwrap();
                asked.push(format!("{job} after {after}"));
                asked.iter().filter(|a| a.starts_with(&job)).count()
            };
            let sse = [(::http::header::CONTENT_TYPE, "text/event-stream")];
            let lines = |lines: &[(u64, &str)]| -> String {
                lines
                    .iter()
                    .map(|(id, data)| format!("id: {id}\ndata: {data}\n\n"))
                    .collect()
            };
            let (a, b) = ((1, r#"{"stdout":"a"}"#), (2, r#"{"stdout":"b"}"#));
            let end = (3, r#"{"end":{"code":0}}"#);
            async move {
                match (job.as_str(), opened) {
                    ("cut", 1) => (sse, lines(&[a, b])).into_response(),
                    ("cut", _) => (sse, lines(&[end])).into_response(),
                    ("removed", 1) => (sse, lines(&[a])).into_response(),
                    ("waiting", 1) | ("ended", _) => (sse, "").into_response(),
                    ("waiting", _) => (sse, lines(&[a, b, end])).into_response(),
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
            }
        };
        let job = |axum::extract::Path(job): axum::extract::Path<String>| async move {
            match job.as_str() {
                "removed" => StatusCode::NOT_FOUND.into_response(),
                _ => Json(serde_json::json!({"id": job, "running": job == "waiting"}))
                    .into_response(),
            }
        };
        let router = Router::new()
            .route("/jobs/{job}", get(job))
            .route("/jobs/{job}/events", get(events));
        let client = crate::Client::over(http_over_x::Client::embed(router));
        let read = |job: &str, after: Option<u64>| {
            let job = format!("/jobs/{job}");
            let client = &client;
            async move {
                log::<crate::SandboxJobEntry>(client, job, after)
                    .map(|entry| entry.map(|entry| entry.id))
                    .collect::<Vec<_>>()
                    .await
            }
        };

        let cut = read("cut", None).await;
        assert!(matches!(cut[..], [Ok(1), Ok(2), Ok(3)]), "{cut:?}");
        let removed = read("removed", None).await;
        assert!(
            matches!(removed[..], [Ok(1), Err(crate::Error::Status(404, _, _))]),
            "{removed:?}"
        );
        let waiting = read("waiting", None).await;
        assert!(matches!(waiting[..], [Ok(1), Ok(2), Ok(3)]), "{waiting:?}");
        let ended = read("ended", Some(3)).await;
        assert!(ended.is_empty(), "{ended:?}");
        assert_eq!(
            *asked.lock().unwrap(),
            [
                "cut after -",
                "cut after 2",
                "removed after -",
                "removed after 1",
                "waiting after -",
                "waiting after -",
                "ended after 3",
            ]
        );
    }
}
