//! The transport client: JSON-RPC requests and notifications over either transport, which only
//! opening and closing tell apart. It knows no method; a typed client is built on it.

use std::{
    collections::HashMap,
    marker::PhantomData,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde::{Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use super::{
    Error, Transport,
    r#impl::{Incoming, Outgoing},
    message::{Received, Request},
};

/// How long a client waits for a response before the call fails. Long, since a call may run
/// a command.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[derive(Default)]
struct State {
    /// The calls waiting on a response, by request id.
    calls: HashMap<u64, oneshot::Sender<Result<Box<RawValue>, Error>>>,
    /// Where the notifications of each method go.
    subscribers: HashMap<String, Vec<mpsc::UnboundedSender<Box<RawValue>>>>,
    /// Why the session ended; `None` while it runs.
    ended: Option<String>,
}

impl State {
    /// End the session for `reason`, failing every call waiting and ending every
    /// [`Notifications`]. Only the first reason is kept.
    fn end(&mut self, reason: String) {
        for (_, call) in self.calls.drain() {
            let _ = call.send(Err(Error::Transport(reason.clone())));
        }
        self.subscribers.clear();
        self.ended.get_or_insert(reason);
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A client of one server, over stdio or Streamable HTTP.
///
/// It carries JSON-RPC 2.0 as the spec defines. One task reads what the server sends, in
/// order, so the notifications sent about a request reach their [`Notifications`] before its
/// response reaches the call.
pub struct TransportClient {
    outgoing: tokio::sync::Mutex<Box<dyn Outgoing>>,
    state: Arc<Mutex<State>>,
    next_id: AtomicU64,
    reader: JoinHandle<()>,
    transport: Transport,
}

impl TransportClient {
    /// A client over `outgoing` and `incoming`, ending its session through `transport`: the
    /// three a [`Transport::spawn`] or [`Transport::connect`] returns. Must be called inside a
    /// tokio runtime, which reads what the server sends after it returns.
    pub fn new(
        outgoing: impl Outgoing + 'static,
        incoming: impl Incoming + 'static,
        transport: Transport,
    ) -> TransportClient {
        let state = Arc::new(Mutex::new(State::default()));
        let reader = tokio::spawn(read(Box::new(incoming), Arc::clone(&state)));
        TransportClient {
            outgoing: tokio::sync::Mutex::new(Box::new(outgoing)),
            state,
            next_id: AtomicU64::new(1),
            reader,
            transport,
        }
    }

    /// Call `method` with `params`, a value that serializes to a JSON object or array, and
    /// wait for its result.
    pub async fn request<R: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<R, Error> {
        let params = serde_json::value::to_raw_value(&params)?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let message = serde_json::to_string(&Request {
            jsonrpc: "2.0",
            id: Some(id),
            method,
            params: &params,
        })?;
        let (answer, answered) = oneshot::channel();
        {
            let mut state = lock(&self.state);
            if let Some(reason) = &state.ended {
                return Err(Error::Transport(reason.clone()));
            }
            state.calls.insert(id, answer);
        }
        // Forget the call however this ends: answered, failed, timed out or dropped.
        let _waiting = Waiting {
            id,
            state: &self.state,
        };
        self.send(message).await?;
        let result = match tokio::time::timeout(REQUEST_TIMEOUT, answered).await {
            Ok(Ok(result)) => result?,
            Ok(Err(_)) => unreachable!("a call is answered before it is forgotten"),
            Err(_) => return Err(Error::Timeout),
        };
        Ok(serde_json::from_str(result.get())?)
    }

    /// Send the notification `method` with `params`, as in [`request`](Self::request): no
    /// answer comes back.
    pub async fn notify(&self, method: &str, params: impl Serialize) -> Result<(), Error> {
        let params = serde_json::value::to_raw_value(&params)?;
        let message = serde_json::to_string(&Request {
            jsonrpc: "2.0",
            id: None,
            method,
            params: &params,
        })?;
        self.send(message).await
    }

    /// The notifications named `method` the server sends from now on, until the session ends.
    pub fn notifications<N: DeserializeOwned>(&self, method: &str) -> Notifications<N> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let mut state = lock(&self.state);
        if state.ended.is_none() {
            let subscribers = state.subscribers.entry(method.to_owned()).or_default();
            subscribers.retain(|subscriber| !subscriber.is_closed());
            subscribers.push(sender);
        }
        Notifications {
            receiver,
            notification: PhantomData,
        }
    }

    /// False once the session has ended: the server is gone, and every call fails.
    pub fn is_connected(&self) -> bool {
        lock(&self.state).ended.is_none()
    }

    /// End the session. Over stdio, close the server's stdin and kill it if it has not exited
    /// within [`EXIT_GRACE`](super::r#impl::stdio::EXIT_GRACE); over Streamable HTTP,
    /// `DELETE` the session if one was opened.
    pub async fn close(self) {
        let TransportClient {
            outgoing,
            reader,
            transport,
            ..
        } = self;
        // Over stdio, dropping the sender closes the server's stdin.
        drop(outgoing);
        transport.close().await;
        reader.abort();
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

/// Read what the server sends until the transport ends, handing each response to its call
/// and each notification to its method's [`Notifications`]. A message that does not read is
/// skipped, not an end.
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
            (None, Some(method)) => {
                let Some(subscribers) = state.subscribers.get_mut(&method) else {
                    continue;
                };
                let params = received.params.unwrap_or_else(null);
                subscribers.retain(|subscriber| subscriber.send(params.clone()).is_ok());
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
                let _ = call.send(result);
            }
            // A request from the server, which this protocol does not have.
            _ => {}
        }
    };
    lock(&state).end(reason);
}

fn null() -> Box<RawValue> {
    serde_json::value::to_raw_value(&()).expect("null serializes")
}

/// The notifications of one method, as [`TransportClient::notifications`] returns them.
pub struct Notifications<N> {
    receiver: mpsc::UnboundedReceiver<Box<RawValue>>,
    notification: PhantomData<fn() -> N>,
}

impl<N: DeserializeOwned> Notifications<N> {
    /// The next notification's params, read as `N`; `None` once the session has ended and
    /// every one received is taken.
    pub async fn next(&mut self) -> Option<Result<N, serde_json::Error>> {
        let params = self.receiver.recv().await?;
        Some(serde_json::from_str(params.get()))
    }
}
