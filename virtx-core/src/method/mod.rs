mod image;
mod sandbox;
mod snapshot;
mod volume;

pub use image::*;
pub use sandbox::*;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
pub use snapshot::*;
pub use volume::*;

/// One JSON-RPC request: the method it is sent as, its params, and the result it is answered
/// with.
pub trait Request {
    const NAME: &'static str;
    type Args: Serialize + DeserializeOwned;
    type Output: Serialize + DeserializeOwned;
}

/// What a multi round-trip request answers, by its `resultType`: what the server needs next,
/// with the `state` the client sends back, or the method's result once it is done.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "resultType", rename_all = "snake_case")]
pub enum RoundTrip<I, C> {
    InputRequired(I),
    Complete(C),
}

/// What a call's stream gives: its progress, a line of its log each, then its output last. As
/// CBOR, `{"progress": "..."}` or `{"output": ...}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event<R> {
    Progress(String),
    Output(R),
}
