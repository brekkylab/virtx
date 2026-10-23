//! The errors a transport answers in the server's place.

/// The response to `request`, a message the client sent, saying it failed with `reason`;
/// `None` if `request` was not one to answer (a notification, or not JSON).
pub(crate) fn transport_error(request: &str, reason: &str) -> Option<String> {
    let request: serde_json::Value = serde_json::from_str(request).ok()?;
    let id = request.get("id")?;
    // -32000: JSON-RPC's range for implementation-defined errors. Never on the wire.
    let response = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32000, "message": reason },
    });
    Some(response.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_error_answers_a_request_by_its_id() {
        let response = transport_error(r#"{"jsonrpc":"2.0","id":7,"method":"m"}"#, "gone");
        let response: serde_json::Value = serde_json::from_str(&response.unwrap()).unwrap();
        assert_eq!(response["id"], 7);
        assert_eq!(response["error"]["code"], -32000);
        assert_eq!(response["error"]["message"], "gone");
    }

    #[test]
    fn transport_error_answers_nothing_for_a_notification() {
        assert_eq!(
            transport_error(r#"{"jsonrpc":"2.0","method":"m"}"#, "gone"),
            None
        );
        assert_eq!(transport_error("not json", "gone"), None);
    }
}
