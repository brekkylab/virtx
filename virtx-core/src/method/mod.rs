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

/// What a call's stream gives: its progress, a line of its log each, then its output last. As
/// CBOR, `{"progress": "..."}` or `{"output": ...}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Event<R> {
    Progress(String),
    Output(R),
}
