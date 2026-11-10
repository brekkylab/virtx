//! How a client reaches a server's endpoints, as `PROTOCOL.md`'s Transport says: HTTP over TCP,
//! or one HTTP/2 connection over the server's stdin and stdout; and what every endpoint's client
//! shares on it: a JSON request and its refusal ([`http`]), a job's log as an event stream
//! ([`sse`], whose server end keeps the log too), and the files of a volume or a sandbox over
//! WebDAV ([`webdav`]).
//!
//! The client end is the `client` feature, the server end the `server` feature; the types
//! [`webdav`]'s requests name are always built.

#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
pub(crate) mod http;
#[cfg(feature = "server")]
mod server;
pub(crate) mod sse;
pub(crate) mod webdav;

#[cfg(feature = "client")]
pub(crate) use client::{Body, full};
#[cfg(feature = "client")]
pub use client::{ForwardError, PortForwarder, TransportClient};
#[cfg(feature = "client")]
pub(crate) use http::{segment, segments};
#[cfg(feature = "server")]
pub use server::serve_stdio;
pub use sse::LogEntry;
#[cfg(feature = "server")]
pub use sse::server::Log;
pub use webdav::{
    DirEntry, FileMoveArgs, FilePathArgs, FileReadArgs, FileRemoveArgs, FileRoot, FileWriteArgs,
    Metadata,
};

#[cfg(all(test, feature = "server"))]
mod tests {
    //! Both ends together: a router over HTTP, served by axum, and over an in-memory HTTP/2
    //! connection as stdio serves one; and what every endpoint shares on either, such as the
    //! credentials sent with each request.

    use ::http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION};
    use axum::{Json, Router, response::IntoResponse as _, routing::get};
    use hyper::service::Service as _;
    use hyper_util::service::TowerToHyperService;
    use tokio::net::TcpListener;

    use super::{TransportClient, http, serve_stdio};
    use crate::{Auth, Error};

    /// `router` served over HTTP on this machine's loopback, and its base URL.
    async fn serve(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await });
        url
    }

    /// `/user`, answering whom the request's bearer token names, `alice` or `bob`: `401` without
    /// one, and `403` for anyone else.
    fn users() -> Router {
        let who = |headers: HeaderMap| async move {
            let token = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok());
            match token.and_then(|t| t.strip_prefix("Bearer ")) {
                None => Err(StatusCode::UNAUTHORIZED),
                Some(user @ ("alice" | "bob")) => Ok(Json(user.to_owned())),
                Some(_) => Err(StatusCode::FORBIDDEN),
            }
        };
        Router::new().route("/user", get(who))
    }

    #[tokio::test]
    async fn the_auth_given_at_connect_goes_with_every_request() {
        let url = serve(users()).await;
        let alice = Auth::Bearer {
            token: "alice".into(),
        };
        let client = TransportClient::connect(&url, alice, HeaderMap::new()).unwrap();
        for _ in 0..2 {
            assert_eq!(
                http::get::<String>(&client, "/user").await.unwrap(),
                "alice"
            );
        }
    }

    #[tokio::test]
    async fn the_auth_set_later_goes_with_the_requests_after() {
        let url = serve(users()).await;
        let bearer = |token: &str| Auth::Bearer {
            token: token.into(),
        };
        let client = TransportClient::connect(&url, bearer("alice"), HeaderMap::new()).unwrap();
        assert_eq!(
            http::get::<String>(&client, "/user").await.unwrap(),
            "alice"
        );

        client.set_auth(bearer("bob")).unwrap();
        assert_eq!(http::get::<String>(&client, "/user").await.unwrap(), "bob");

        // The old one is gone with the new, not kept under it.
        client.set_auth(Auth::None).unwrap();
        let error = http::get::<String>(&client, "/user").await.unwrap_err();
        assert!(matches!(error, Error::Status(401, _)), "{error:?}");
        client.set_auth(bearer("carol")).unwrap();
        let error = http::get::<String>(&client, "/user").await.unwrap_err();
        assert!(matches!(error, Error::Status(403, _)), "{error:?}");
    }

    #[tokio::test]
    async fn the_headers_given_at_connect_and_set_later_go_with_the_requests_after() {
        // The token in a header of the caller's own, as to a server that reads it from one.
        let url = serve(users()).await;
        let alice =
            HeaderMap::from_iter([(AUTHORIZATION, HeaderValue::from_static("Bearer alice"))]);
        let bob = HeaderMap::from_iter([(AUTHORIZATION, HeaderValue::from_static("Bearer bob"))]);
        let client = TransportClient::connect(&url, Auth::None, alice).unwrap();
        assert_eq!(
            http::get::<String>(&client, "/user").await.unwrap(),
            "alice"
        );

        client.set_headers(bob);
        assert_eq!(http::get::<String>(&client, "/user").await.unwrap(), "bob");
        client.set_headers(HeaderMap::new());
        let error = http::get::<String>(&client, "/user").await.unwrap_err();
        assert!(matches!(error, Error::Status(401, _)), "{error:?}");
    }

    #[tokio::test]
    async fn the_auth_goes_over_an_authorization_header() {
        let url = serve(users()).await;
        let alice = Auth::Bearer {
            token: "alice".into(),
        };
        let bob = HeaderMap::from_iter([(AUTHORIZATION, HeaderValue::from_static("Bearer bob"))]);
        let client = TransportClient::connect(&url, alice, bob).unwrap();
        assert_eq!(
            http::get::<String>(&client, "/user").await.unwrap(),
            "alice"
        );
    }

    #[test]
    fn a_token_does_not_go_in_the_clear_to_another_machine() {
        let token = || Auth::Bearer { token: "t".into() };
        let with_token = |url| TransportClient::connect(url, token(), HeaderMap::new());
        assert!(with_token("http://example.com/").is_err());
        for url in [
            "https://example.com/",
            "http://127.0.0.1:9/",
            "http://localhost:9/",
            "http://[::1]:9/",
        ] {
            assert!(with_token(url).is_ok(), "{url}");
        }
        let client = TransportClient::connect("http://example.com/", Auth::None, HeaderMap::new());
        assert!(client.unwrap().set_auth(token()).is_err());
    }

    #[test]
    fn connect_refuses_anything_but_an_http_url() {
        for url in ["not a url", "file:///tmp/x"] {
            assert!(
                TransportClient::connect(url, Auth::None, HeaderMap::new()).is_err(),
                "{url}"
            );
        }
    }

    #[tokio::test]
    async fn the_endpoints_are_under_the_base_url_with_or_without_its_slash() {
        let router = Router::new().route("/base/ping", get(|| async { Json("pong") }));
        let url = serve(router).await;
        for base in [format!("{url}base"), format!("{url}base/")] {
            let client = TransportClient::connect(&base, Auth::None, HeaderMap::new()).unwrap();
            assert_eq!(http::get::<String>(&client, "/ping").await.unwrap(), "pong");
        }
    }

    #[tokio::test]
    async fn a_request_to_no_server_fails() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let client = TransportClient::connect(&url, Auth::None, HeaderMap::new()).unwrap();
        for _ in 0..2 {
            let error = http::get::<String>(&client, "/ping").await.unwrap_err();
            assert!(matches!(error, Error::Transport(_)), "{error:?}");
        }
    }

    #[tokio::test]
    async fn a_refusal_reads_its_code_and_message_over_either_transport() {
        let router = Router::new().route(
            "/snapshots/{name}",
            get(|| async {
                let body = serde_json::json!({"error": {"code": "not_found", "message": "none"}});
                (StatusCode::NOT_FOUND, Json(body))
            }),
        );
        let tcp =
            TransportClient::connect(&serve(router.clone()).await, Auth::None, HeaderMap::new());
        let (pipe, server) = tokio::io::duplex(64 * 1024);
        let (input, output) = tokio::io::split(server);
        let service = TowerToHyperService::new(router);
        let handle = move |request| {
            let service = service.clone();
            async move { service.call(request).await.unwrap_or_else(|e| match e {}) }
        };
        tokio::spawn(serve_stdio(handle, input, output));

        for client in [tcp.unwrap(), TransportClient::over(pipe)] {
            let error = http::get::<String>(&client, "/snapshots/nothing")
                .await
                .unwrap_err();
            let Error::Status(404, message) = &error else {
                panic!("not a 404: {error:?}");
            };
            assert_eq!(message, "not_found: none");
            client.close().await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_exits_fails_every_request() {
        let client = TransportClient::spawn(tokio::process::Command::new("true")).unwrap();
        for _ in 0..2 {
            assert!(http::get::<String>(&client, "/ping").await.is_err());
        }
        client.close().await;
    }

    #[tokio::test]
    async fn serve_returns_once_its_client_is_gone() {
        let router = Router::new().route("/ping", get(|| async { Json("pong") }));
        let (pipe, server) = tokio::io::duplex(64 * 1024);
        let (input, output) = tokio::io::split(server);
        let service = TowerToHyperService::new(router);
        let handle = move |request| {
            let service = service.clone();
            async move { service.call(request).await.unwrap_or_else(|e| match e {}) }
        };
        let served = tokio::spawn(serve_stdio(handle, input, output));
        let client = TransportClient::over(pipe);
        assert_eq!(http::get::<String>(&client, "/ping").await.unwrap(), "pong");
        client.close().await;
        served.await.unwrap().unwrap();
    }

    /// A job's log whose connection breaks after `upto` entries the first time it is asked for, and
    /// is answered whole after the `Last-Event-ID` the next: each request's `Last-Event-ID` kept.
    fn breaking_log(asked: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>) -> Router {
        use axum::{
            body::Body,
            routing::{get, post},
        };
        let entries = [
            r#"{"stdout":"one"}"#,
            r#"{"stdout":"two"}"#,
            r#"{"end":{"code":0}}"#,
        ];
        let events = move |after: u64, upto: usize, broken: bool| {
            use futures_util::StreamExt as _;
            let parts: Vec<Result<String, std::io::Error>> = entries
                .iter()
                .enumerate()
                .skip(after as usize)
                .take(upto)
                .map(|(i, data)| Ok(format!("id: {}\ndata: {data}\n\n", i + 1)))
                .collect();
            // Broken a while after what it sent, once that has reached the client.
            let gone = futures_util::stream::once(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                Err(std::io::Error::other("the server went away"))
            })
            .take(usize::from(broken));
            Body::from_stream(futures_util::stream::iter(parts).chain(gone))
        };
        let log = {
            let asked = asked.clone();
            move |headers: HeaderMap| async move {
                let after = headers
                    .get("last-event-id")
                    .map(|h| h.to_str().unwrap().to_owned());
                let first = {
                    let mut asked = asked.lock().unwrap();
                    asked.push(after.clone());
                    asked.len() == 1
                };
                let after: u64 = after.map_or(0, |a| a.parse().unwrap());
                let body = if first {
                    events(after, 1, true)
                } else {
                    events(after, 3, false)
                };
                ([("content-type", "text/event-stream")], body)
            }
        };
        let exec = {
            let asked = asked.clone();
            move || async move {
                asked.lock().unwrap().push(Some("exec".into()));
                (
                    [
                        ("content-type", "text/event-stream"),
                        (
                            "virtx-job",
                            r#"{"id":"j","kind":"exec","pid":1,"cmd":["x"],"running":true}"#,
                        ),
                    ],
                    events(0, 1, true),
                )
            }
        };
        Router::new()
            .route("/sandboxes/s/jobs/j/events", get(log))
            .route("/sandboxes/s/jobs/exec/", post(exec))
    }

    #[tokio::test]
    async fn a_job_s_log_whose_connection_breaks_is_read_on_after_its_last_entry() {
        use futures_util::TryStreamExt as _;
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let url = serve(breaking_log(asked.clone())).await;
        let client = TransportClient::connect(&url, Auth::None, HeaderMap::new()).unwrap();
        let read: Vec<_> = crate::sandbox::client::job_log(&client, "s", "j", None)
            .try_collect()
            .await
            .unwrap();
        assert_eq!(read.iter().map(|e| e.id).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(*asked.lock().unwrap(), [None, Some("1".to_owned())]);

        // An exec answered with its log: read on from the job's log as well.
        asked.lock().unwrap().clear();
        asked.lock().unwrap().push(None); // the job's log, asked for next, answered whole
        let args = crate::sandbox::SandboxExecArgs::new(["x"]);
        let read: Vec<_> = crate::sandbox::client::exec_log(&client, "s", &args)
            .try_collect()
            .await
            .unwrap();
        assert_eq!(read.len(), 4, "the job, then its three entries");
        assert_eq!(
            asked.lock().unwrap()[1..],
            [Some("exec".to_owned()), Some("1".to_owned())]
        );
    }

    /// A `429` for a quota the caller is past -- sandboxes open -- is answered at once, as waiting
    /// frees none of it; the rate of requests, which frees up by itself, is waited out and asked
    /// again.
    #[tokio::test]
    async fn a_quota_past_is_answered_at_once_and_the_rate_of_requests_waited_out() {
        use std::sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        };
        let asked = Arc::new(AtomicU32::new(0));
        let router = Router::new()
            .route(
                "/quota",
                axum::routing::get(async || {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(::http::header::RETRY_AFTER, "30")],
                        r#"{"error":{"code":"quota","message":"the organization has 3 sandboxes open","reason":"sandboxes"}}"#,
                    )
                }),
            )
            .route(
                "/rate",
                axum::routing::get(async |axum::extract::State(asked): axum::extract::State<Arc<AtomicU32>>| {
                    if asked.fetch_add(1, Ordering::SeqCst) == 0 {
                        return (
                            StatusCode::TOO_MANY_REQUESTS,
                            [(::http::header::RETRY_AFTER, "1")],
                            r#"{"error":{"code":"quota","message":"past the rate","reason":"requests"}}"#,
                        )
                            .into_response();
                    }
                    "\"answered\"".into_response()
                }),
            )
            .with_state(asked.clone());
        let url = serve(router).await;
        let client = TransportClient::connect(&url, Auth::None, HeaderMap::new()).unwrap();

        let error = http::get::<String>(&client, "/quota").await.unwrap_err();
        assert_eq!(error.code(), Some("quota"), "{error:?}");
        assert!(matches!(error, Error::Status(429, _)), "{error:?}");

        assert_eq!(
            http::get::<String>(&client, "/rate").await.unwrap(),
            "answered"
        );
        assert_eq!(
            asked.load(Ordering::SeqCst),
            2,
            "asked again once the rate allowed"
        );
    }
}
