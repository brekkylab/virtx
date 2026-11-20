//! The server's end of a job's log: the entries it keeps, and its event stream.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::{Stream, StreamExt as _};
use http::HeaderMap;
use serde::Serialize;
use tokio::sync::watch;

/// A job's log, kept whole up to `limit` entries before its end, and read by any number of
/// [`events`](Log::events) streams at once.
pub struct Log<E> {
    state: Mutex<State<E>>,
    changed: watch::Sender<()>,
    limit: usize,
    /// What stands, once, for the entries past `limit`.
    truncated: E,
}

struct State<E> {
    entries: Vec<E>,
    truncated: bool,
    ended: bool,
    /// The job was removed: a reader's stream ends where it is, or at the end of one that has
    /// ended.
    removed: bool,
}

impl<E: Clone + Serialize + Send + Sync + 'static> Log<E> {
    /// An empty log that keeps `limit` entries before its end, and `truncated` once past it.
    pub fn new(limit: usize, truncated: E) -> Arc<Log<E>> {
        Arc::new(Log {
            state: Mutex::new(State {
                entries: Vec::new(),
                truncated: false,
                ended: false,
                removed: false,
            }),
            changed: watch::channel(()).0,
            limit,
            truncated,
        })
    }

    /// Keep `entry`, unless the log is past its limit or has ended.
    pub fn push(&self, entry: E) {
        let mut state = self.state.lock().unwrap();
        if state.ended {
            return;
        }
        if state.entries.len() < self.limit {
            state.entries.push(entry);
        } else if !state.truncated {
            state.truncated = true;
            state.entries.push(self.truncated.clone());
        } else {
            return;
        }
        drop(state);
        self.changed.send_replace(());
    }

    /// Keep `end` last, past the limit too, and run `ended` before any reader sees it, so what
    /// it changes of the job is there for a reader that asks once the stream ends. False, and
    /// nothing run, if the log had ended already.
    pub fn end(&self, end: E, ended: impl FnOnce()) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.ended {
            return false;
        }
        state.ended = true;
        state.entries.push(end);
        ended();
        drop(state);
        self.changed.send_replace(());
        true
    }

    /// End every reader's stream where it is, or, once the log has ended, at its end: the job
    /// is gone.
    pub fn remove(&self) {
        self.state.lock().unwrap().removed = true;
        self.changed.send_replace(());
    }

    /// The entries after the one `Last-Event-ID` in `headers` names, or all of them, as the
    /// body of a `text/event-stream`: each as it comes, until the end, or the removal of a job
    /// that has none.
    pub fn events(
        self: Arc<Self>,
        headers: &HeaderMap,
    ) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static + use<E> {
        let after: usize = headers
            .get("last-event-id")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let changed = self.changed.subscribe();
        futures_util::stream::unfold(
            (self, changed, after),
            |(log, mut changed, next)| async move {
                loop {
                    let (entry, ended) = {
                        let state = log.state.lock().unwrap();
                        if state.removed && !state.ended {
                            return None;
                        }
                        (state.entries.get(next).cloned(), state.ended)
                    };
                    if let Some(entry) = entry {
                        let data = serde_json::to_string(&entry).expect("an entry serializes");
                        let event = Bytes::from(format!("id: {}\ndata: {data}\n\n", next + 1));
                        return Some((Ok(event), (log, changed, next + 1)));
                    }
                    if ended || changed.changed().await.is_err() {
                        return None;
                    }
                }
            },
        )
    }

    /// An event of `first`, with no `id`, then every entry from the log's first as
    /// [`events`](Log::events) sends them: the entries keep their numbers in the log, so a
    /// reader cut off reads on from `events` after the last entry it read.
    pub fn events_led_by(
        self: Arc<Self>,
        first: &impl Serialize,
    ) -> impl Stream<Item = Result<Bytes, Infallible>> + Send + 'static {
        let first = serde_json::to_string(first).expect("an entry serializes");
        let first = Bytes::from(format!("data: {first}\n\n"));
        futures_util::stream::iter([Ok(first)]).chain(self.events(&HeaderMap::new()))
    }
}

/// Whether a request's `Accept` in `headers` names `text/event-stream` with a `q` above `0`:
/// asks for an event stream, which a range like `*/*` does not.
pub fn accepts_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get_all(http::header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|range| {
            let mut parts = range.split(';').map(str::trim);
            let named = parts
                .next()
                .is_some_and(|kind| kind.eq_ignore_ascii_case("text/event-stream"));
            let q = parts
                .filter_map(|param| param.split_once('='))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("q"))
                .map_or(1.0, |(_, q)| q.trim().parse::<f32>().unwrap_or(0.0));
            named && q > 0.0
        })
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use super::*;

    #[derive(Clone, Serialize)]
    #[serde(rename_all = "snake_case")]
    enum Line {
        Stdout(String),
        Truncated {},
        End {},
    }

    #[test]
    fn an_event_stream_is_accepted_only_where_named_with_a_q_above_0() {
        let accepting = |accept: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(http::header::ACCEPT, accept.parse().unwrap());
            accepts_event_stream(&headers)
        };
        assert!(accepting("text/event-stream"));
        assert!(accepting("application/json, Text/Event-Stream;q=0.5"));
        assert!(!accepting("text/event-stream;q=0"));
        assert!(!accepting("text/event-stream; q=0.0, application/json"));
        assert!(!accepting("*/*"));
        assert!(!accepting("application/json"));
        assert!(!accepts_event_stream(&HeaderMap::new()));
    }

    fn after(id: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("last-event-id", id.parse().unwrap());
        headers
    }

    async fn read(log: &Arc<Log<Line>>, headers: &HeaderMap) -> String {
        let events: Vec<_> = Arc::clone(log).events(headers).collect().await;
        events
            .into_iter()
            .map(|event| String::from_utf8(event.unwrap().to_vec()).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn an_ended_log_is_read_whole_and_from_after_the_last_event_id() {
        let log = Log::new(2, Line::Truncated {});
        for line in ["a", "b", "c", "d"] {
            log.push(Line::Stdout(line.into()));
        }
        assert!(log.end(Line::End {}, || {}));
        assert!(!log.end(Line::End {}, || panic!("ended twice")));
        log.push(Line::Stdout("e".into()));

        assert_eq!(
            read(&log, &HeaderMap::new()).await,
            "id: 1\ndata: {\"stdout\":\"a\"}\n\n\
             id: 2\ndata: {\"stdout\":\"b\"}\n\n\
             id: 3\ndata: {\"truncated\":{}}\n\n\
             id: 4\ndata: {\"end\":{}}\n\n"
        );
        assert_eq!(
            read(&log, &after("3")).await,
            "id: 4\ndata: {\"end\":{}}\n\n"
        );
    }

    #[tokio::test]
    async fn a_reader_gets_entries_as_they_come_until_the_job_is_removed() {
        let log = Log::new(10, Line::Truncated {});
        let mut events = std::pin::pin!(Arc::clone(&log).events(&HeaderMap::new()));
        log.push(Line::Stdout("a".into()));
        let event = events.next().await.unwrap().unwrap();
        assert_eq!(event, "id: 1\ndata: {\"stdout\":\"a\"}\n\n");
        log.remove();
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn a_log_ended_then_removed_is_read_to_its_end() {
        let log = Log::new(10, Line::Truncated {});
        let events = std::pin::pin!(Arc::clone(&log).events(&HeaderMap::new()));
        log.push(Line::Stdout("a".into()));
        assert!(log.end(Line::End {}, || {}));
        log.remove();
        let got: Vec<_> = events.collect().await;
        assert_eq!(
            got.into_iter()
                .map(|event| String::from_utf8(event.unwrap().to_vec()).unwrap())
                .collect::<String>(),
            "id: 1\ndata: {\"stdout\":\"a\"}\n\n\
             id: 2\ndata: {\"end\":{}}\n\n"
        );
        assert_eq!(
            read(&log, &after("1")).await,
            "id: 2\ndata: {\"end\":{}}\n\n"
        );
    }

    #[tokio::test]
    async fn a_log_led_by_an_event_numbers_its_entries_as_the_log_does() {
        let log = Log::new(10, Line::Truncated {});
        for line in ["a", "b"] {
            log.push(Line::Stdout(line.into()));
        }
        assert!(log.end(Line::End {}, || {}));
        let led: Vec<_> = Arc::clone(&log)
            .events_led_by(&serde_json::json!({"job": {"id": "j"}}))
            .collect()
            .await;
        let led: String = led
            .into_iter()
            .map(|event| String::from_utf8(event.unwrap().to_vec()).unwrap())
            .collect();
        assert_eq!(
            led,
            "data: {\"job\":{\"id\":\"j\"}}\n\n\
             id: 1\ndata: {\"stdout\":\"a\"}\n\n\
             id: 2\ndata: {\"stdout\":\"b\"}\n\n\
             id: 3\ndata: {\"end\":{}}\n\n"
        );
        // Cut off after its first entry, it reads on from the log after that entry's id.
        assert_eq!(
            read(&log, &after("1")).await,
            "id: 2\ndata: {\"stdout\":\"b\"}\n\n\
             id: 3\ndata: {\"end\":{}}\n\n"
        );
    }
}
