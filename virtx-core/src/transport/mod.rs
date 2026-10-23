mod client;
pub(crate) mod dispatch;
pub mod error;
pub mod r#impl;
pub(crate) mod message;
#[cfg(test)]
// `connect` is the typed client's, which is `ffi`'s.
#[cfg_attr(not(feature = "ffi"), allow(dead_code))]
pub(crate) mod testing;

pub use client::TransportClient;
pub use dispatch::{Dispatcher, Methods, Notifier};
pub use error::Error;
pub use message::ErrorObject;

pub use crate::method::Event;
