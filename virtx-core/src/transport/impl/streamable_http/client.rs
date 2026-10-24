//! The client's transport: each message a `POST`, and what it answers fed back to the client.

use std::{io, sync::Arc};

use http::{StatusCode, header};
use tokio::sync::mpsc;

use super::{CBOR, CBOR_SEQ, is_media_type};
use crate::transport::{
    error::transport_error,
    r#impl::{BoxFuture, Incoming, Outgoing, frame::Parser},
    message::Received,
};

/// Where every message goes.
struct Endpoint {
    http: reqwest::Client,
    url: reqwest::Url,
}

/// The transport to the endpoint at `url`, an `http` or `https` URL. Nothing is sent until
/// the first message.
pub fn connect(url: &str) -> anyhow::Result<(Sender, Receiver)> {
    let url = reqwest::Url::parse(url)?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "not an http or https URL: {url}"
    );
    let endpoint = Arc::new(Endpoint {
        http: reqwest::Client::new(),
        url,
    });
    let (inbox, received) = mpsc::unbounded_channel();
    let sender = Sender { endpoint, inbox };
    Ok((sender, Receiver(received)))
}

pub struct Sender {
    endpoint: Arc<Endpoint>,
    /// Where each `POST`'s answers go, for [`Receiver`] to hand to the client.
    inbox: mpsc::UnboundedSender<Vec<u8>>,
}

impl Outgoing for Sender {
    /// Start the `POST` and return. Its failure is the request's error, not the transport's:
    /// an `Err` here would end the client, and every call in flight with it.
    fn send(&mut self, message: Vec<u8>) -> BoxFuture<'_, io::Result<()>> {
        let endpoint = Arc::clone(&self.endpoint);
        let inbox = self.inbox.clone();
        tokio::spawn(async move {
            if let Err(reason) = post(&endpoint, &inbox, &message).await
                && let Some(response) = transport_error(&message, &reason)
            {
                let _ = inbox.send(response);
            }
        });
        Box::pin(async { Ok(()) })
    }

    /// Nothing to end: the server keeps nothing between `POST`s.
    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async {})
    }
}

pub struct Receiver(mpsc::UnboundedReceiver<Vec<u8>>);

impl Incoming for Receiver {
    fn receive(&mut self) -> BoxFuture<'_, io::Result<Vec<u8>>> {
        Box::pin(async move {
            self.0
                .recv()
                .await
                .ok_or(io::ErrorKind::UnexpectedEof.into())
        })
    }
}

/// `POST` `message`, and send what the server answers to `inbox`. An `Err` says why the
/// answer did not come.
async fn post(
    endpoint: &Endpoint,
    inbox: &mpsc::UnboundedSender<Vec<u8>>,
    message: &[u8],
) -> Result<(), String> {
    let mut response = endpoint
        .http
        .post(endpoint.url.clone())
        .header(header::CONTENT_TYPE, CBOR)
        .header(header::ACCEPT, format!("{CBOR}, {CBOR_SEQ}"))
        .body(message.to_owned())
        .send()
        .await
        .map_err(|e| format!("{e:#}"))?;

    let status = response.status();
    if status == StatusCode::ACCEPTED {
        return Ok(());
    }
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(format!("the server answered {status}: {body}")
            .trim_end()
            .to_owned());
    }
    let streamed = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| is_media_type(value, CBOR_SEQ));
    if !streamed {
        let body = response.bytes().await.map_err(|e| format!("{e:#}"))?;
        let _ = inbox.send(body.to_vec());
        return Ok(());
    }

    let mut parser = Parser::default();
    let mut answered = false;
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("{e:#}"))? {
        parser.push(&chunk);
        while let Some(message) = parser.next_message() {
            answered |= is_response(&message);
            let _ = inbox.send(message);
        }
    }
    match answered {
        true => Ok(()),
        false => Err("the stream ended before the response".into()),
    }
}

/// Whether `message` is a response, not a notification.
fn is_response(message: &[u8]) -> bool {
    ciborium::from_reader::<Received, _>(message).is_ok_and(|message| message.id.is_some())
}
