//! Why a call fails, and the errors a transport answers in the server's place.

use std::fmt;

use ciborium::Value;

use super::message::{ErrorObject, response};

/// Why a call failed.
#[derive(Debug)]
pub enum Error {
    /// The server answered with an error.
    Call(ErrorObject),
    /// The request did not reach the server, or its response did not come back: the transport
    /// failed, or the session ended.
    Transport(String),
    /// The params did not encode, or the result did not read as the type asked for.
    Cbor(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Call(error) => write!(f, "{error}"),
            Error::Transport(reason) => f.write_str(reason),
            Error::Cbor(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for Error {}

/// The response to `request`, a message the client sent, saying it failed with `reason`;
/// `None` if `request` was not one to answer (a notification, or not CBOR).
pub(crate) fn transport_error(request: &[u8], reason: &str) -> Option<Vec<u8>> {
    let Value::Map(members) = ciborium::from_reader(request).ok()? else {
        return None;
    };
    let (_, id) = members
        .iter()
        .find(|(key, _)| key.as_text() == Some("id"))?;
    // -32000: JSON-RPC's range for implementation-defined errors. Never on the wire.
    Some(response(id, Err(ErrorObject::new(-32000, reason))))
}

#[cfg(test)]
mod tests {
    use ciborium::cbor;

    use super::*;
    use crate::transport::message::Received;

    fn encode(message: Value) -> Vec<u8> {
        let mut bytes = Vec::new();
        ciborium::into_writer(&message, &mut bytes).unwrap();
        bytes
    }

    #[test]
    fn transport_error_answers_a_request_by_its_id() {
        let request = encode(cbor!({"jsonrpc" => "2.0", "id" => 7, "method" => "m"}).unwrap());
        let response = transport_error(&request, "gone").unwrap();
        let response: Received = ciborium::from_reader(&response[..]).unwrap();
        assert_eq!(response.id, Some(Value::from(7)));
        let error = response.error.unwrap();
        assert_eq!((error.code, error.message.as_str()), (-32000, "gone"));
    }

    #[test]
    fn transport_error_answers_nothing_for_a_notification() {
        let notification = encode(cbor!({"jsonrpc" => "2.0", "method" => "m"}).unwrap());
        assert_eq!(transport_error(&notification, "gone"), None);
        assert_eq!(transport_error(b"not cbor", "gone"), None);
    }
}
