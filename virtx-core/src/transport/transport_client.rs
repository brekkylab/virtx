//! The transport client: JSON-RPC requests and notifications over either transport, which only
//! opening and closing tell apart. It knows no method; a typed client is built on it.

use std::time::Duration;

use jsonrpsee::core::{
    client::{
        Client as Rpc, ClientBuilder, ClientT, Error, Subscription, SubscriptionClientT,
        TransportReceiverT, TransportSenderT,
    },
    traits::ToRpcParams,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;

use super::Transport;

/// How long a client waits for a response before the call fails. Longer than jsonrpsee's
/// minute, since a call may run a command.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// A client of one server, over stdio or Streamable HTTP.
///
/// It carries JSON-RPC 2.0 as the spec defines.
pub struct TransportClient {
    rpc: Rpc,
    transport: Transport,
}

impl TransportClient {
    /// A client over `sender` and `receiver`, ending its session through `transport`: the
    /// three a [`Transport::spawn`] or [`Transport::connect`] returns. Must be called inside a
    /// tokio runtime, which drives the client's tasks after it returns.
    pub fn new<S, R>(sender: S, receiver: R, transport: Transport) -> TransportClient
    where
        S: TransportSenderT + Send,
        R: TransportReceiverT + Send,
    {
        let rpc = ClientBuilder::default()
            .request_timeout(REQUEST_TIMEOUT)
            .build_with_tokio(sender, receiver);
        TransportClient { rpc, transport }
    }

    /// Call `method` with `params`, a value that serializes to a JSON object or array, and
    /// wait for its result.
    pub async fn request<R: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
    ) -> Result<R, Error> {
        self.rpc.request(method, Params::new(params)?).await
    }

    /// Send the notification `method` with `params`, as in [`request`](Self::request): no
    /// answer comes back.
    pub async fn notify(&self, method: &str, params: impl Serialize) -> Result<(), Error> {
        self.rpc.notification(method, Params::new(params)?).await
    }

    /// The notifications named `method` the server sends from now on.
    pub async fn notifications<N: DeserializeOwned>(
        &self,
        method: &str,
    ) -> Result<Subscription<N>, Error> {
        self.rpc.subscribe_to_method(method).await
    }

    /// False once the transport has ended: the server is gone, and every call fails.
    pub fn is_connected(&self) -> bool {
        self.rpc.is_connected()
    }

    /// End the session. Over stdio, close the server's stdin and kill it if it has not exited
    /// within [`EXIT_GRACE`](super::transport::stdio::EXIT_GRACE); over Streamable HTTP,
    /// `DELETE` the session if one was opened.
    pub async fn close(self) {
        let TransportClient { rpc, transport } = self;
        // Over stdio, dropping the client closes the server's stdin.
        drop(rpc);
        transport.close().await;
    }
}

/// `params`, serialized before the call, so it need not be `Send` across the await.
struct Params(Box<RawValue>);

impl Params {
    fn new(params: impl Serialize) -> Result<Params, Error> {
        Ok(Params(serde_json::value::to_raw_value(&params)?))
    }
}

impl ToRpcParams for Params {
    fn to_rpc_params(self) -> Result<Option<Box<RawValue>>, serde_json::Error> {
        Ok(Some(self.0))
    }
}
