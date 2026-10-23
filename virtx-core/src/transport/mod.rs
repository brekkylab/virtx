#[cfg(feature = "client")]
mod client;
#[cfg(feature = "server")]
pub(crate) mod dispatch;
#[cfg(feature = "client")]
pub mod error;
pub mod r#impl;
pub(crate) mod message;
#[cfg(all(test, feature = "server"))]
pub(crate) mod testing;

#[cfg(feature = "client")]
pub use client::TransportClient;
#[cfg(feature = "server")]
pub use dispatch::{Dispatcher, Methods, Notifier};
#[cfg(feature = "client")]
pub use error::Error;
pub use message::ErrorObject;

pub use crate::method::Event;
