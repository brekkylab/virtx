mod image;
mod sandbox;
mod snapshot;
mod volume;

pub use image::*;
pub use sandbox::*;
use serde::{Serialize, de::DeserializeOwned};
pub use snapshot::*;
pub use volume::*;

/// One JSON-RPC request: the method it is sent as, its params, and the result it is answered
/// with.
pub trait Request {
    const NAME: &'static str;
    type Args: Serialize + DeserializeOwned;
    type Output: Serialize + DeserializeOwned;
}
