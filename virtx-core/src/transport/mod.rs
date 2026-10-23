mod client;
pub(crate) mod dispatch;
pub mod error;
pub mod r#impl;
pub(crate) mod message;
#[cfg(test)]
pub(crate) mod testing;

pub use client::{Event, TransportClient};
pub use dispatch::{Dispatcher, Methods, Notifier};
pub use error::Error;
pub use message::ErrorObject;
