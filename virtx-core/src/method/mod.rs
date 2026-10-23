mod image;
mod snapshot;
mod volume;

pub use image::*;
pub use snapshot::*;
pub use volume::*;

use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};

/// One JSON-RPC request: the method it is sent as, its params, and the result it is answered
/// with.
pub trait Request {
    const NAME: &'static str;
    type Args: Serialize + DeserializeOwned + JsonSchema;
    type Output: Serialize + DeserializeOwned + JsonSchema;
}
