//! How a client reaches a server's endpoints, as `PROTOCOL.md`'s Transport says: HTTP over TCP,
//! or one HTTP/2 connection over the server's stdin and stdout; and what every endpoint's client
//! shares on it: a JSON request and its refusal ([`http`]), a job's log as an event stream
//! ([`sse`], whose server end keeps the log too), and the files of a volume or a snapshot over
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
pub use client::{PortForwarder, TransportClient};
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

    use ::http::{HeaderMap, HeaderValue, StatusCode, header::AUTHORIZATION};
    use axum::{Json, Router, routing::get};
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
                assert!(matches!(error, Error::Path(_)), "{path}: {error:?}");
            }
            let error = files::rename(client, &data, "a", "../../other/files/a", true)
                .await
                .unwrap_err();
            assert!(matches!(error, Error::Path(_)), "{error:?}");
            let error = files::create_dir_all(client, &data, "../a/b")
                .await
                .unwrap_err();
            assert!(matches!(error, Error::Path(_)), "{error:?}");
            // A name is not a path: `..` there is not resolved.
            let error = files::remove(client, &FileRoot::Volume("..".into()), "a", false)
                .await
                .unwrap_err();
            assert!(matches!(error, Error::Path(_)), "{error:?}");
            let error = volumes::remove(client, "..").await.unwrap_err();
            assert!(matches!(error, Error::Path(_)), "{error:?}");
            // A mount's path that resolves to the root would go to the list of mounts.
            for path in [".", "a/.."] {
                let error = volumes::remove_mount(client, "data", path)
                    .await
                    .unwrap_err();
                assert!(matches!(error, Error::Path(_)), "{path}: {error:?}");
            }
            // A path not built from segments, such as one a registry answers: `\` separates
            // segments in an `http` URL.
            let error = http::get::<String>(client, "/a\\..\\..\\b")
                .await
                .unwrap_err();
            assert!(matches!(error, Error::Path(_)), "{error:?}");
            assert_eq!(*seen.lock().unwrap(), [] as [&str; 0]);
        }
    }
}
