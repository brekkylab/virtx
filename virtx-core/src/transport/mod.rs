//! How a client reaches a server's endpoints, as `PROTOCOL.md`'s Transport says: HTTP over TCP,
//! or one HTTP/2 connection over the server's stdin and stdout; and what every endpoint's client
//! shares on it: a JSON request and its refusal ([`http`]), a job's log as an event stream
//! ([`sse`], whose server end keeps the log too), and the files of a volume or a sandbox over
//! WebDAV ([`webdav`]); and, on the server's end, a request's body read as its args ([`args`]).
//!
//! The client end is the `client` feature, the server end the `server` feature; the types
//! [`webdav`]'s requests name are always built.

#[cfg(feature = "server")]
mod args;
#[cfg(feature = "client")]
mod client;
#[cfg(feature = "client")]
pub(crate) mod http;
#[cfg(feature = "server")]
mod server;
pub(crate) mod sse;
pub(crate) mod webdav;

#[cfg(feature = "server")]
pub use args::{described_type, from_body};
#[cfg(feature = "client")]
pub(crate) use client::{Body, full};
#[cfg(feature = "client")]
pub use client::{ForwardError, PortForwarder, TransportClient};
#[cfg(feature = "client")]
pub(crate) use http::{resolved, segment, segments};
#[cfg(feature = "server")]
pub use server::serve_stdio;
pub use sse::LogEntry;
#[cfg(feature = "server")]
pub use sse::server::Log;
pub use webdav::{
    DirEntry, FileMoveArgs, FilePathArgs, FileReadArgs, FileRemoveArgs, FileRoot, FileWriteArgs,
    Metadata,
};

/// What a test of a client call shares: a router, served in memory as stdio serves one.
#[cfg(all(test, feature = "server"))]
pub(crate) mod testing {
    use axum::Router;
    use hyper::service::Service as _;
    use hyper_util::service::TowerToHyperService;

    use super::{TransportClient, serve_stdio};

    /// A client over an in-memory pipe to `router`.
    pub(crate) fn piped(router: Router) -> TransportClient {
        let (pipe, server) = tokio::io::duplex(64 * 1024);
        let (input, output) = tokio::io::split(server);
        let service = TowerToHyperService::new(router);
        let handle = move |request| {
            let service = service.clone();
            async move { service.call(request).await.unwrap_or_else(|e| match e {}) }
        };
        tokio::spawn(serve_stdio(handle, input, output));
        TransportClient::over(pipe)
    }
}

#[cfg(all(test, feature = "server"))]
mod tests {
    //! Both ends together: a router over HTTP, served by axum, and over an in-memory HTTP/2
    //! connection as stdio serves one; and what every endpoint shares on either, such as the
    //! credentials sent with each request.

    use ::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header::AUTHORIZATION};
    use axum::{Json, Router, routing::get};
    use hyper::service::Service as _;
    use hyper_util::service::TowerToHyperService;
    use tokio::net::TcpListener;

    use super::{TransportClient, http, serve_stdio, testing::piped};
    use crate::{Auth, Error, PROJECT_HEADER};

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
            "/volumes/{name}",
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
            let error = http::get::<String>(&client, "/volumes/nothing")
                .await
                .unwrap_err();
            let Error::Status(404, message) = &error else {
                panic!("not a 404: {error:?}");
            };
            assert_eq!(message, "not_found: none");
            client.close().await;
        }
    }

    #[tokio::test]
    async fn the_project_set_goes_with_every_request_over_either_transport() {
        let router = Router::new().route(
            "/project",
            get(|headers: HeaderMap| async move {
                let named = headers.get(PROJECT_HEADER).map(|v| v.to_str().unwrap());
                Json(named.map(str::to_owned))
            }),
        );
        let given = HeaderMap::from_iter([(
            HeaderName::from_static(PROJECT_HEADER),
            HeaderValue::from_static("given"),
        )]);
        let tcp = TransportClient::connect(&serve(router.clone()).await, Auth::None, given);
        let named = async |client: &TransportClient| {
            http::get::<Option<String>>(client, "/project")
                .await
                .unwrap()
        };

        for (client, given) in [(tcp.unwrap(), Some("given")), (piped(router), None)] {
            assert_eq!(named(&client).await.as_deref(), given);
            // Set, it is named in place of one among the headers given; unset, that one is, or
            // none where none was given.
            client.set_project(Some("demo")).unwrap();
            assert_eq!(named(&client).await.as_deref(), Some("demo"));
            assert!(client.set_project(Some("a\nb")).is_err());
            assert_eq!(named(&client).await.as_deref(), Some("demo"));
            client.set_project(None).unwrap();
            assert_eq!(named(&client).await.as_deref(), given);
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

    /// An exec answered with its log, whose connection breaks after its first entry, and the
    /// job's log, answered whole after the `Last-Event-ID` it is sent -- broken too the first time
    /// it is asked for, unless `asked` holds something: each request's `Last-Event-ID` kept.
    fn breaking_exec(asked: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>) -> Router {
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

    /// An exec answered with its log, whose connection breaks, is read on from the job's log
    /// after its last entry.
    #[tokio::test]
    async fn an_exec_answered_with_its_log_is_read_on_after_its_last_entry() {
        use futures_util::TryStreamExt as _;
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        asked.lock().unwrap().push(None); // the job's log, asked for next, answered whole
        let url = serve(breaking_exec(asked.clone())).await;
        let client = TransportClient::connect(&url, Auth::None, HeaderMap::new()).unwrap();
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

    #[tokio::test]
    async fn a_file_s_path_goes_resolved_and_one_above_its_root_goes_nowhere() {
        use std::sync::{Arc, Mutex};

        use super::{FileRoot, testing::piped, webdav::client as files};
        use crate::volume::client as volumes;

        // Every path that reaches the server, over either transport.
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let router = {
            let seen = Arc::clone(&seen);
            Router::new().fallback(move |uri: ::http::Uri| {
                let seen = Arc::clone(&seen);
                async move {
                    seen.lock().unwrap().push(uri.path().to_owned());
                    StatusCode::NO_CONTENT
                }
            })
        };
        let url = serve(router.clone()).await;
        let tcp = TransportClient::connect(&url, Auth::None, HeaderMap::new()).unwrap();
        let stdio = piped(router);
        let data = FileRoot::Volume("data".into());

        for client in [&tcp, &stdio] {
            for (path, sent) in [
                (".", "/volumes/data/files/"),
                ("./a/./b", "/volumes/data/files/a/b"),
                ("dir/../x", "/volumes/data/files/x"),
                ("a.b/..c/.d", "/volumes/data/files/a.b/..c/.d"),
            ] {
                files::remove(client, &data, path, false).await.unwrap();
                assert_eq!(std::mem::take(&mut *seen.lock().unwrap()), [sent], "{path}");
            }

            // Over HTTP, the first would go as `DELETE /sandboxes/abc`.
            for path in ["../../../sandboxes/abc", "a/../../b", ".."] {
                let error = files::remove(client, &data, path, false).await.unwrap_err();
                assert!(matches!(error, Error::Client(_)), "{path}: {error:?}");
            }
            let error = files::rename(client, &data, "a", "../../other/files/a", true)
                .await
                .unwrap_err();
            assert!(matches!(error, Error::Client(_)), "{error:?}");
            let error = files::create_dir_all(client, &data, "../a/b")
                .await
                .unwrap_err();
            assert!(matches!(error, Error::Client(_)), "{error:?}");
            // A name is not a path: `..` there is not resolved.
            let error = files::remove(client, &FileRoot::Volume("..".into()), "a", false)
                .await
                .unwrap_err();
            assert!(matches!(error, Error::Client(_)), "{error:?}");
            let error = volumes::remove(client, "..").await.unwrap_err();
            assert!(matches!(error, Error::Client(_)), "{error:?}");
            // A mount's path that resolves to the root would go to the list of mounts.
            for path in [".", "a/.."] {
                let error = volumes::remove_mount(client, "data", path)
                    .await
                    .unwrap_err();
                assert!(matches!(error, Error::Client(_)), "{path}: {error:?}");
            }
            // A path not built from segments, such as one a registry answers: `\` separates
            // segments in an `http` URL.
            let error = http::get::<String>(client, "/a\\..\\..\\b")
                .await
                .unwrap_err();
            assert!(matches!(error, Error::Client(_)), "{error:?}");
            assert_eq!(*seen.lock().unwrap(), [] as [&str; 0]);
        }
    }

    /// Counts each request to a route, and answers the `n`th (from `0`) with `answer(n)`.
    fn counted(
        asked: &std::sync::Arc<std::sync::atomic::AtomicU32>,
        answer: impl Fn(u32) -> axum::response::Response + Clone + Send + Sync + 'static,
    ) -> axum::routing::MethodRouter {
        let asked = asked.clone();
        axum::routing::any(move || {
            let n = asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let answer = answer.clone();
            async move { answer(n) }
        })
    }

    /// A refusal in the protocol's shape: `status`, `error` its members, `Retry-After` if any.
    fn turned_away(
        status: StatusCode,
        error: serde_json::Value,
        retry_after: Option<&str>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse as _;
        let mut response = (status, Json(serde_json::json!({ "error": error }))).into_response();
        if let Some(seconds) = retry_after {
            response.headers_mut().insert(
                ::http::header::RETRY_AFTER,
                HeaderValue::from_str(seconds).unwrap(),
            );
        }
        response
    }

    /// A request the server turns away for now -- a proxy's `503` page, the rate of requests
    /// past -- is sent again after its `Retry-After`, or a wait of its own, until it is answered.
    #[tokio::test]
    async fn a_request_turned_away_for_now_is_sent_again_until_it_is_answered() {
        use axum::response::IntoResponse as _;
        use std::sync::{Arc, atomic::Ordering};
        let (read, written) = (Arc::default(), Arc::default());
        let router = Router::new()
            .route(
                "/read",
                counted(&read, |n| match n {
                    0 => (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [(::http::header::CONTENT_TYPE, "text/html")],
                        "<html>503</html>",
                    )
                        .into_response(),
                    1 => turned_away(
                        StatusCode::TOO_MANY_REQUESTS,
                        serde_json::json!({"code": "quota", "message": "slow down", "reason": "requests"}),
                        Some("1"),
                    ),
                    _ => Json("read").into_response(),
                }),
            )
            .route(
                "/written",
                counted(&written, |n| match n {
                    0 => turned_away(
                        StatusCode::SERVICE_UNAVAILABLE,
                        serde_json::json!({"code": "unavailable", "message": "not now"}),
                        Some("1"),
                    ),
                    _ => Json("written").into_response(),
                }),
            );
        let client = piped(router);
        assert_eq!(http::get::<String>(&client, "/read").await.unwrap(), "read");
        assert_eq!(read.load(Ordering::SeqCst), 3);
        // A `PUT` makes nothing a second sending makes again.
        let body = serde_json::json!({});
        assert_eq!(
            http::put::<String>(&client, "/written", &body)
                .await
                .unwrap(),
            "written"
        );
        assert_eq!(written.load(Ordering::SeqCst), 2);
    }

    /// A `429` for a quota the caller is past -- sandboxes open -- is answered at once, as
    /// waiting frees none of it, with its code; the rate of requests, which frees up by itself,
    /// is waited out.
    #[tokio::test]
    async fn a_quota_past_is_answered_at_once() {
        use std::sync::{Arc, atomic::Ordering};
        let asked = Arc::default();
        let router = Router::new().route(
            "/quota",
            counted(&asked, |_| {
                turned_away(
                    StatusCode::TOO_MANY_REQUESTS,
                    serde_json::json!({"code": "quota", "message": "3 sandboxes open", "reason": "sandboxes"}),
                    Some("30"),
                )
            }),
        );
        let client = piped(router);
        let error = http::get::<String>(&client, "/quota").await.unwrap_err();
        assert!(matches!(error, Error::Status(429, _)), "{error:?}");
        assert_eq!(error.code(), Some("quota"));
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    /// A `503` to a request that makes something is sent again only where it says nothing was
    /// made -- `capacity` -- since any other may have come after the making began, and a second
    /// sending would make a second of it.
    #[tokio::test]
    async fn a_making_turned_away_is_sent_again_only_where_nothing_was_made() {
        use axum::response::IntoResponse as _;
        use std::sync::{Arc, atomic::Ordering};
        let (made, room) = (Arc::default(), Arc::default());
        let router = Router::new()
            .route(
                "/made",
                counted(&made, |_| {
                    turned_away(
                        StatusCode::SERVICE_UNAVAILABLE,
                        serde_json::json!({"code": "unavailable", "message": "its machine went away"}),
                        Some("1"),
                    )
                }),
            )
            .route(
                "/room",
                counted(&room, |n| match n {
                    0 => turned_away(
                        StatusCode::SERVICE_UNAVAILABLE,
                        serde_json::json!({"code": "capacity", "message": "no room yet"}),
                        Some("1"),
                    ),
                    _ => Json("made").into_response(),
                }),
            );
        let client = piped(router);
        let body = serde_json::json!({});
        let error = http::post::<String>(&client, "/made", &body)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Status(503, _)), "{error:?}");
        assert_eq!(made.load(Ordering::SeqCst), 1, "made once at most");
        assert_eq!(
            http::post::<String>(&client, "/room", &body).await.unwrap(),
            "made"
        );
        assert_eq!(room.load(Ordering::SeqCst), 2, "nothing made: sent again");
    }

    /// The time a request is held back counts the server's answers as well as the waits between
    /// them: a server that holds each refusal 12 s is asked three times in 30 s, not once a
    /// second for 30 s of waits.
    #[tokio::test(start_paused = true)]
    async fn a_request_is_held_back_its_answers_time_and_its_waits_together() {
        use std::sync::{Arc, atomic::Ordering};
        let asked = Arc::<std::sync::atomic::AtomicU32>::default();
        let held = asked.clone();
        let router = Router::new().route(
            "/slow",
            get(async move || {
                held.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_secs(12)).await;
                turned_away(
                    StatusCode::SERVICE_UNAVAILABLE,
                    serde_json::json!({"code": "capacity", "message": "no room yet"}),
                    Some("1"),
                )
            }),
        );
        let client = piped(router);
        let error = http::get::<String>(&client, "/slow").await.unwrap_err();
        assert!(matches!(error, Error::Status(503, _)), "{error:?}");
        assert_eq!(asked.load(Ordering::SeqCst), 3);
    }

    /// A log at `/log` whose connection breaks after its first two entries, then is answered by a
    /// proxy's own `502` page for the moment its server is gone, then is answered after the
    /// `Last-Event-ID` it is sent: each request's `Last-Event-ID` kept in `asked`.
    fn breaking_log(asked: std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>) -> Router {
        use axum::response::IntoResponse as _;
        use futures_util::StreamExt as _;
        let events = |lines: &[(u64, &str)]| -> String {
            lines
                .iter()
                .map(|(id, data)| format!("id: {id}\ndata: {data}\n\n"))
                .collect()
        };
        let log = move |headers: HeaderMap| async move {
            let after = headers
                .get("last-event-id")
                .map(|v| v.to_str().unwrap().to_owned());
            let n = {
                let mut asked = asked.lock().unwrap();
                asked.push(after);
                asked.len()
            };
            let sse = [(::http::header::CONTENT_TYPE, "text/event-stream")];
            match n {
                1 => {
                    let sent = bytes::Bytes::from(events(&[
                        (1, r#"{"stdout":"a"}"#),
                        (2, r#"{"stdout":"b"}"#),
                    ]));
                    // Cut a moment after what was sent, once that has reached the client.
                    let cut = futures_util::stream::once(async {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        Err(std::io::Error::other("cut"))
                    });
                    let body = futures_util::stream::iter([Ok(sent)]).chain(cut);
                    (sse, axum::body::Body::from_stream(body)).into_response()
                }
                2 => (
                    StatusCode::BAD_GATEWAY,
                    [(::http::header::CONTENT_TYPE, "text/html")],
                    "<html><body><h1>502 Bad Gateway</h1></body></html>",
                )
                    .into_response(),
                _ => (
                    sse,
                    events(&[(3, r#"{"stdout":"c"}"#), (4, r#"{"end":{"code":0}}"#)]),
                )
                    .into_response(),
            }
        };
        Router::new().route("/log", get(log))
    }

    /// A log whose connection breaks is read on after its last entry, past a proxy's page for
    /// the moment its server is gone: over TCP, where a connection is made anew. Over stdio, the
    /// break is the caller's at once, as the server's process is gone with it.
    #[tokio::test]
    async fn a_log_whose_connection_breaks_is_read_on_after_its_last_entry() {
        use futures_util::StreamExt as _;
        use std::sync::{Arc, Mutex};
        let read = |client: TransportClient| async move {
            super::sse::client::log::<crate::SandboxJobEntry>(&client, "/log".into(), None)
                .map(|entry| entry.map(|entry| entry.id))
                .collect::<Vec<_>>()
                .await
        };

        let asked = Arc::new(Mutex::new(Vec::new()));
        let url = serve(breaking_log(asked.clone())).await;
        let client = TransportClient::connect(&url, Auth::None, HeaderMap::new()).unwrap();
        let ids: Vec<u64> = read(client)
            .await
            .into_iter()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ids, [1, 2, 3, 4]);
        assert_eq!(
            *asked.lock().unwrap(),
            [None, Some("2".to_owned()), Some("2".to_owned())]
        );

        let asked = Arc::new(Mutex::new(Vec::new()));
        let read = read(piped(breaking_log(asked.clone()))).await;
        assert!(
            matches!(read[..], [Ok(1), Ok(2), Err(Error::Transport(_))]),
            "{read:?}"
        );
        assert_eq!(*asked.lock().unwrap(), [None]);
    }

    /// A sandbox is found by its name in `?name=`, encoded; of what a server answers, only one
    /// named so is taken.
    #[tokio::test]
    async fn a_sandbox_is_found_by_its_name_encoded() {
        use axum::extract::RawQuery;
        use serde_json::json;

        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let list = {
            let asked = asked.clone();
            move |RawQuery(query): RawQuery| async move {
                asked.lock().unwrap().push(query);
                // Every sandbox, as a server that reads no `name` answers.
                Json(json!([
                    {"id": "s1", "image": "sha256:00", "running": true},
                    {"id": "s2", "name": "a b&c", "image": "sha256:00", "running": true},
                ]))
            }
        };
        let url = serve(Router::new().route("/sandboxes/", get(list))).await;
        let client = TransportClient::connect(&url, Auth::None, HeaderMap::new()).unwrap();

        let found = crate::sandbox::client::find(&client, "a b&c")
            .await
            .unwrap();
        assert_eq!(found.map(|s| s.id).as_deref(), Some("s2"));
        let found = crate::sandbox::client::find(&client, "s1").await.unwrap();
        assert_eq!(found, None);
        assert_eq!(
            *asked.lock().unwrap(),
            [
                Some("name=a%20b%26c".to_owned()),
                Some("name=s1".to_owned())
            ]
        );
    }
}
