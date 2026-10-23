//! The client's transport: each message a `POST`, and what it answers fed back to the client.

use std::{io, sync::Arc};

use http::{HeaderValue, StatusCode, header};
use tokio::sync::{Mutex, mpsc};

use super::{EVENT_STREAM, SESSION_HEADER, sse};
use crate::transport::{
    error::transport_error,
    r#impl::{BoxFuture, Incoming, Outgoing},
};

/// Where every message goes, and the session they go in.
struct Endpoint {
    http: reqwest::Client,
    url: reqwest::Url,
    /// `None` until the first answer names the session. Held across the first `POST`, so no
    /// second message goes out before there is a session to put it in.
    session: Mutex<Option<HeaderValue>>,
}

/// The transport to the endpoint at `url`, an `http` or `https` URL. Nothing is sent until
/// the first message, which opens the session.
pub fn connect(url: &str) -> anyhow::Result<(Sender, Receiver)> {
    let url = reqwest::Url::parse(url)?;
    anyhow::ensure!(
        matches!(url.scheme(), "http" | "https"),
        "not an http or https URL: {url}"
    );
    let endpoint = Arc::new(Endpoint {
        http: reqwest::Client::new(),
        url,
        session: Mutex::new(None),
    });
    let (inbox, received) = mpsc::unbounded_channel();
    let sender = Sender { endpoint, inbox };
    Ok((sender, Receiver(received)))
}

pub struct Sender {
    endpoint: Arc<Endpoint>,
    /// Where each `POST`'s answers go, for [`Receiver`] to hand to the client.
    inbox: mpsc::UnboundedSender<String>,
}

impl Outgoing for Sender {
    /// Start the `POST` and return. Its failure is the request's error, not the transport's:
    /// an `Err` here would end the client, and every call in flight with it.
    fn send(&mut self, message: String) -> BoxFuture<'_, io::Result<()>> {
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

    /// End the session with a `DELETE`, if one was opened.
    fn close(self: Box<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            let endpoint = &self.endpoint;
            if let Some(id) = endpoint.session.lock().await.take() {
                let _ = endpoint
                    .http
                    .delete(endpoint.url.clone())
                    .header(SESSION_HEADER, id)
                    .send()
                    .await;
            }
        })
    }
}

pub struct Receiver(mpsc::UnboundedReceiver<String>);

impl Incoming for Receiver {
    fn receive(&mut self) -> BoxFuture<'_, io::Result<String>> {
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
    inbox: &mpsc::UnboundedSender<String>,
    message: &str,
) -> Result<(), String> {
    let mut request = endpoint
        .http
        .post(endpoint.url.clone())
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, format!("application/json, {EVENT_STREAM}"))
        .body(message.to_owned());
    let mut session = Some(endpoint.session.lock().await);
    if let Some(id) = session.as_deref().cloned().flatten() {
        request = request.header(SESSION_HEADER, id);
        session = None;
    }

    let mut response = request.send().await.map_err(|e| format!("{e:#}"))?;
    if let Some(mut session) = session
        && let Some(id) = response.headers().get(SESSION_HEADER)
    {
        *session = Some(id.clone());
    }

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
        .is_some_and(|value| value.starts_with(EVENT_STREAM));
    if !streamed {
        let body = response.text().await.map_err(|e| format!("{e:#}"))?;
        let _ = inbox.send(body);
        return Ok(());
    }

    let mut parser = sse::Parser::default();
    let mut answered = false;
    while let Some(chunk) = response.chunk().await.map_err(|e| format!("{e:#}"))? {
        for message in parser.feed(&chunk) {
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
fn is_response(message: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(message)
        .is_ok_and(|message| message.get("id").is_some())
}
