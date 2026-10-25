use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// One JSON-RPC request: the method it is sent as, its params, and the result it is answered
/// with.
pub trait Request {
    const NAME: &'static str;
    type Args: Serialize + DeserializeOwned;
    type Output: Serialize + DeserializeOwned;
}

/// A request whose server sends `notifications/progress` about it before answering it, each
/// carrying a `content` of the method's own: such as a line of its log, or the next piece of its
/// output. Still a [`Request`]: called as one, its notifications are not read.
pub trait StreamingRequest: Request {
    type Content: Serialize + DeserializeOwned;
}

/// What a multi round-trip request answers, by its `resultType`: what the server needs next,
/// with the `state` the client sends back, or the method's result once it is done.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "resultType", rename_all = "snake_case")]
pub enum RoundTrip<I, C> {
    InputRequired(I),
    Complete(C),
}

/// What a call's stream gives: each of its notifications, then its output last. As CBOR,
/// `{"progress": {"progress": 1, "total": 3, "content": ...}}` or `{"output": ...}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event<R, C> {
    Progress(Progress<C>),
    Output(R),
}

/// One `notifications/progress` about a call: how far it has got, what that will reach when
/// known, and the method's `content`, if it sent one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress<C> {
    pub progress: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<C>,
}
