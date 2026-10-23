//! The transport client: JSON-RPC requests over either transport, which only opening and
//! closing tell apart. It knows no method; a typed client is built on it.

use std::{
    collections::HashMap,
    pin::pin,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use super::{
    Error,
    r#impl::{Incoming, Outgoing},
    message::{PROGRESS, Progress, Received, Request},
};

/// What a call's stream gives: its progress, a line of its log each, then its output last.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event<R> {
    Progress(String),
    Output(R),
}

/// A client of one server, over stdio or Streamable HTTP.
///
/// It carries JSON-RPC 2.0 as the spec defines. One task reads what the server sends, in
/// order, so a request's progress reaches its call before its response does.
pub struct TransportClient {
    outgoing: tokio::sync::Mutex<Box<dyn Outgoing>>,
    state: Arc<Mutex<State>>,
    next_id: AtomicU64,
    reader: JoinHandle<()>,
}

#[derive(Default)]
struct State {
    /// The calls waiting on a response, by request id.
    calls: HashMap<u64, Call>,
    /// Why the session ended; `None` while it runs.
    ended: Option<String>,
}

/// Where a call's response and progress go.
struct Call {
    answer: oneshot::Sender<Result<Box<RawValue>, Error>>,
    progress: mpsc::UnboundedSender<String>,
}

impl State {
    /// End the session for `reason`, failing every call waiting. Only the first reason is
    /// kept.
    fn end(&mut self, reason: String) {
        for (_, call) in self.calls.drain() {
            let _ = call.answer.send(Err(Error::Transport(reason.clone())));
        }
        self.ended.get_or_insert(reason);
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl TransportClient {
    /// A client over `outgoing` and `incoming`, such as the two [`stdio::spawn`] or
    /// [`streamable_http::connect`] returns. Must be called inside a tokio runtime, which reads
    /// what the server sends after it returns.
    pub fn new(outgoing: impl Outgoing + 'static, incoming: impl Incoming + 'static) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let reader = tokio::spawn(read(Box::new(incoming), Arc::clone(&state)));
        TransportClient {
            outgoing: tokio::sync::Mutex::new(Box::new(outgoing)),
            state,
            next_id: AtomicU64::new(1),
            reader,
        }
    }

    /// Call `method` with `params`, a value that serializes to a JSON object or array, and
    /// wait for its result. Its progress is not read.
    pub async fn request<R: DeserializeOwned + Send>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<R, Error> {
        let events = self.request_with_progress::<R>(method, params);
        let mut events = pin!(events);
        while let Some(event) = events.next().await {
            if let Event::Output(output) = event? {
                return Ok(output);
            }
        }
        unreachable!("a call's events end with its output or its error")
    }

    /// Call `method` with `params`, as in [`request`](Self::request): the progress the server
    /// sends about it, then its result last, or the error it failed with. Dropping the stream
    /// drops the call.
    pub fn request_with_progress<'a, R>(
        &'a self,
        method: &'a str,
        params: impl Serialize,
    ) -> impl Stream<Item = Result<Event<R>, Error>> + Send + 'a
    where
        R: DeserializeOwned + Send + 'a,
    {
        let params = serde_json::value::to_raw_value(&params);
        async_stream::try_stream! {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let message = serde_json::to_string(&Request {
                jsonrpc: "2.0",
                id: Some(id),
                method,
                params: &params?,
            })?;
            let (answer, mut answered) = oneshot::channel();
            let (progress, mut progressed) = mpsc::unbounded_channel();
            let ended = {
                let mut state = lock(&self.state);
                let ended = state.ended.clone();
                if ended.is_none() {
                    state.calls.insert(id, Call { answer, progress });
                }
                ended
            };
            if let Some(reason) = ended {
                Err(Error::Transport(reason))?;
            }
            // Forget the call however this ends: answered, failed or dropped.
            let _waiting = Waiting {
                id,
                state: &self.state,
            };
            self.send(message).await?;

            let result = loop {
                tokio::select! {
                    biased;
                    Some(line) = progressed.recv() => yield Event::Progress(line),
                    result = &mut answered => break result,
                }
            };
            // Progress is handed over before the response it precedes: what is left is queued.
            while let Ok(line) = progressed.try_recv() {
                yield Event::Progress(line);
            }
            let result = result.map_err(|_| Error::Transport("the session ended".into()))??;
            yield Event::Output(serde_json::from_str(result.get())?);
        }
    }

    /// False once the session has ended: the server is gone, and every call fails.
    pub fn is_connected(&self) -> bool {
        lock(&self.state).ended.is_none()
    }

    /// End the session. Over stdio, close the server's stdin and kill it if it has not exited
    /// within [`EXIT_GRACE`](stdio::EXIT_GRACE); over Streamable HTTP, `DELETE` the session
    /// if one was opened.
    pub async fn close(self) {
        self.outgoing.into_inner().close().await;
        self.reader.abort();
    }

    /// Send `message`. Failing to means the server is gone, which ends the session.
    async fn send(&self, message: String) -> Result<(), Error> {
        let sent = self.outgoing.lock().await.send(message).await;
        sent.map_err(|e| {
            let reason = format!("sending to the server: {e}");
            lock(&self.state).end(reason.clone());
            Error::Transport(reason)
        })
    }
}

/// A call waiting on its response; dropping it forgets the call.
struct Waiting<'a> {
    id: u64,
    state: &'a Mutex<State>,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        lock(self.state).calls.remove(&self.id);
    }
}

/// Read what the server sends until the transport ends, handing each response and each
/// progress value to its call. A message that does not read is skipped, not an end.
async fn read(mut incoming: Box<dyn Incoming>, state: Arc<Mutex<State>>) {
    let reason = loop {
        let message = match incoming.receive().await {
            Ok(message) => message,
            Err(e) => break format!("the server is gone: {e}"),
        };
        let Ok(received) = serde_json::from_str::<Received>(&message) else {
            continue;
        };
        let mut state = lock(&state);
        match (received.id, received.method) {
            (None, Some(method)) if method == PROGRESS => {
                let Some(Progress {
                    request_id,
                    message,
                }) = received
                    .params
                    .and_then(|params| serde_json::from_str::<Progress>(params.get()).ok())
                else {
                    continue;
                };
                if let Some(call) = request_id.as_u64().and_then(|id| state.calls.get(&id)) {
                    let _ = call.progress.send(message);
                }
            }
            (Some(id), None) => {
                let Some(call) = id.as_u64().and_then(|id| state.calls.remove(&id)) else {
                    continue;
                };
                let result = match (received.result, received.error) {
                    (_, Some(error)) => Err(Error::Call(error)),
                    (Some(result), None) => Ok(result),
                    (None, None) => Err(Error::Transport(
                        "a response with neither result nor error".into(),
                    )),
                };
                let _ = call.answer.send(result);
            }
            // Another notification, or a request from the server: this protocol has neither.
            _ => {}
        }
    };
    lock(&state).end(reason);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_is_tagged_by_what_it_is() {
        let cases = [
            (
                Event::Progress("pulling".into()),
                r#"{"progress":"pulling"}"#,
            ),
            (Event::Output(2), r#"{"output":2}"#),
        ];
        for (event, json) in cases {
            assert_eq!(serde_json::to_string(&event).unwrap(), json);
            assert_eq!(serde_json::from_str::<Event<u32>>(json).unwrap(), event);
        }
    }
}
