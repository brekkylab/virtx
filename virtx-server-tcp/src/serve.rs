//! The server's routes, for an [`Access`]: the endpoints of `virtx-core`'s `PROTOCOL.md`
//! ([`crate::endpoints`]), each answered from the home its request reaches, the images as an
//! OCI registry, and each sandbox's ports. A server built on this one gives [`router`] an
//! authenticator of its own in the access, and routes of its own beside these; the server a
//! config alone makes is [`configured`], which does both.
//!
//! A sandbox's ports are at `/ports/<capability>` beside the endpoints, each a WebSocket of the
//! subprotocol `virtx.tcp` carrying a TCP connection to the port: the capability, which only
//! making the sandbox answers, is all a connection needs, and gives no more than the one port.

use std::sync::Arc;
use std::time::Duration;

use crate::access::{self, Access};
use crate::config::Config;
use crate::endpoints::{self, Tunnel};
use anyhow::Context as _;
use axum::Extension;
use axum::extract::Path;
use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::middleware::Next;
use axum::response::{IntoResponse as _, Response};
use axum::routing::any;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tower_http::cors::CorsLayer;

/// The WebSocket subprotocol a port's TCP is carried in.
const TCP_SUBPROTOCOL: &str = "virtx.tcp";

/// How much of a port's TCP one message carries at most.
const CHUNK: usize = 64 << 10;

/// What a server's binary is spawned with to be a machine.
pub const MACHINE_SUBCOMMAND: &str = "vm";

/// How often images are pruned.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);

/// This process become a machine, never to come back, if it was spawned to be one: called
/// before any runtime is made.
pub fn become_machine_if_spawned() {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == MACHINE_SUBCOMMAND)
    {
        // Why it could not be is the host's already, told on the control channel; stderr is
        // the guest command's, and nothing of this process's goes there.
        let Err(_) = virtx_uvm::enter_machine();
        std::process::exit(1);
    }
}

/// The server `config` alone makes: its [`Access::configured`] behind [`router`], and beside it
/// where a client signs in for its tokens, answered without any -- the provider and the client
/// to sign in as.
pub async fn configured(config: &Config) -> anyhow::Result<(Arc<Access>, axum::Router)> {
    let access = Arc::new(Access::configured(config).await?);
    let oidc = &config.oidc;
    let advertised = serde_json::json!({
        "oidc": { "issuer": oidc.issuer, "client_id": oidc.client_id },
    });
    let advertised = (
        [(header::CONTENT_TYPE, "application/json")],
        advertised.to_string(),
    );
    let own = axum::Router::new().route(
        "/auth",
        axum::routing::get(move || async move { advertised }),
    );
    let router = router(Arc::clone(&access), own);
    Ok((access, router))
}

/// Everything served, each request checked against `access`: the endpoints, in the home a
/// request reaches, the projects themselves, the images' registry at `/images/oci/v2/`, the
/// ports, and the CORS a browser at one of its origins has -- with `own` beside them, routes
/// that take no home and check for themselves.
pub fn router(access: Arc<Access>, own: axum::Router) -> axum::Router {
    let registry = format!("{}/v2", access::REGISTRY_PATH);
    let cors = cors(access.origins());
    let reading = Arc::clone(&access);
    let images = move |request: axum::extract::Request| {
        let access = Arc::clone(&reading);
        async move {
            match access.home(request.headers()).await {
                Ok((_, home)) => home
                    .registry()
                    .handle(request)
                    .await
                    .map(axum::body::Body::new)
                    .into_response(),
                Err(status) => endpoints::refusal(status).into_response(),
            }
        }
    };
    let checked = Arc::clone(&access);
    let reaching = Arc::clone(&access);
    endpoints::router()
        .layer(axum::middleware::from_fn(
            move |mut request: axum::extract::Request, next: Next| {
                let checked = Arc::clone(&checked);
                async move {
                    match checked.home(request.headers()).await {
                        Ok((_, home)) => {
                            request.extensions_mut().insert(home);
                            next.run(request).await
                        }
                        Err(status) => endpoints::refusal(status).into_response(),
                    }
                }
            },
        ))
        .merge(endpoints::projects().layer(Extension(Arc::clone(&access))))
        .merge(own)
        .route(&registry, any(images.clone()))
        .route(&format!("{registry}/"), any(images.clone()))
        .route(&format!("{registry}/{{*rest}}"), any(images))
        .route(
            "/ports/{capability}",
            any(
                move |Path(capability): Path<String>,
                      headers: HeaderMap,
                      upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>| {
                    let access = Arc::clone(&reaching);
                    async move { port(&access, &capability, &headers, upgrade) }
                },
            ),
        )
        .layer(cors)
}

/// The images of every home `access` has, pruned every hour from now on, as
/// `POST /images/prune` does.
pub fn keep_pruned(access: Arc<Access>) {
    tokio::spawn(async move {
        let mut every = tokio::time::interval(PRUNE_EVERY);
        loop {
            every.tick().await;
            for home in access.homes() {
                let pruned = move || home.images().prune(endpoints::PRUNE_GRACE);
                match tokio::task::spawn_blocking(pruned).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => eprintln!("pruning the images: {e}"),
                    Err(e) => eprintln!("pruning the images: {e}"),
                }
            }
        }
    });
}

/// A listener at `listen`, where it says it serves.
pub async fn bind(listen: std::net::SocketAddr) -> anyhow::Result<tokio::net::TcpListener> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening at {listen}"))?;
    eprintln!("serving at http://{}/", listener.local_addr()?);
    Ok(listener)
}

/// `router` served on `listener` until this process is interrupted.
pub async fn run(listener: tokio::net::TcpListener, router: axum::Router) -> anyhow::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// What a browser at one of the config's `origins` may do here: every method the endpoints
/// take, with the headers they read and the ones a client reads back. A preflight is answered
/// here, before any check, as a browser sends it without the token; the request it clears is
/// checked as any other.
fn cors(origins: &[String]) -> CorsLayer {
    let origins: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();
    let method = |name| Method::from_bytes(name).expect("a method name");
    CorsLayer::new()
        .allow_origin(origins)
        .allow_methods([
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::DELETE,
            Method::OPTIONS,
            method(b"PROPFIND"),
            method(b"MKCOL"),
            method(b"MOVE"),
            method(b"COPY"),
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::IF_MATCH,
            header::RANGE,
            HeaderName::from_static(virtx_core::PROJECT_HEADER),
            HeaderName::from_static("depth"),
            HeaderName::from_static("destination"),
            HeaderName::from_static("overwrite"),
        ])
        .expose_headers([
            header::ETAG,
            header::CONTENT_RANGE,
            header::LOCATION,
            HeaderName::from_static("dav"),
        ])
        .max_age(Duration::from_secs(3600))
}

/// A request to the port `capability` opens: `403` from an `Origin` not let in, `426` unless it
/// opens a WebSocket, `400` unless of the subprotocol `virtx.tcp`, and `404` for a capability no
/// sandbox here has -- one never answered, or of a sandbox gone.
fn port(
    access: &Access,
    capability: &str,
    headers: &HeaderMap,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    if let Err(status) = access.origin(headers) {
        return endpoints::refusal(status).into_response();
    }
    let Ok(upgrade) = upgrade else {
        return StatusCode::UPGRADE_REQUIRED.into_response();
    };
    let offered = headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|offered| offered.trim() == TCP_SUBPROTOCOL)
        });
    if !offered {
        return (
            StatusCode::BAD_REQUEST,
            format!("a port is reached over the WebSocket subprotocol {TCP_SUBPROTOCOL}"),
        )
            .into_response();
    }
    let Some(tunnel) = access
        .homes()
        .iter()
        .find_map(|home| home.tunnel(capability))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    upgrade
        .protocols([TCP_SUBPROTOCOL])
        .on_upgrade(move |socket| relay(socket, tunnel))
}

/// Carry bytes between `socket` and the port `tunnel` reaches until either ends: what is
/// read from the port as binary messages, and the binary messages to the port. Closed at once
/// when no machine runs, as a port nothing listens on is.
async fn relay(mut socket: WebSocket, tunnel: Tunnel) {
    if let Ok(mut reached) = tunnel.connect().await {
        let (mut from, mut to) = reached.split();
        let mut buffer = vec![0u8; CHUNK];
        loop {
            tokio::select! {
                read = from.read(&mut buffer) => match read {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let message = Message::Binary(buffer[..n].to_vec().into());
                        if socket.send(message).await.is_err() {
                            break;
                        }
                    }
                },
                received = socket.recv() => match received {
                    Some(Ok(Message::Binary(bytes))) => {
                        if to.write_all(&bytes).await.is_err() {
                            break;
                        }
                    }
                    // Answered by the socket itself.
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    // Text is no TCP's, and anything else is the end.
                    Some(Ok(Message::Text(_) | Message::Close(_)) | Err(_)) | None => break,
                },
            }
        }
    }
    let _ = socket.send(Message::Close(None)).await;
}

#[cfg(test)]
mod tests {
    //! The app, served on the loopback over an identity of the test's own, and called over HTTP
    //! as a client would: each request to an endpoint in a project -- by hand, and through
    //! `virtx-core`'s client given it at connect -- and to the projects themselves. Who a
    //! provider's token is, is `virtx-rs`'s `tests/projects.rs`, against the fixture's.

    use std::path::{Path, PathBuf};

    use reqwest::{Method, StatusCode, Url};
    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};
    use tokio_tungstenite::tungstenite;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use virtx_core::{Auth, Client, DirEntry, FileRoot, SandboxCreateArgs};
    use virtx_uvm::{Entry, FsTree, ImageStore};

    use std::sync::Arc;

    use futures_util::future::BoxFuture;
    use virtx_core::DEFAULT_PROJECT;

    use virtx_uvm::{Membership as _, ProjectStore};

    use crate::access::{Access, FileMembership, Identity, TokenIsUser};
    use crate::config::Config;

    /// Every request the tester's, whatever it carries.
    struct Tester;

    impl Identity for Tester {
        fn identify<'a>(&'a self, _token: Option<&'a str>) -> BoxFuture<'a, Option<String>> {
            Box::pin(async { Some("tester".to_owned()) })
        }
    }

    type Socket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    /// The app served on the loopback over an empty home, stopped and removed when this drops.
    struct Served {
        task: tokio::task::JoinHandle<std::io::Result<()>>,
        /// Its base URL, ending in `/`.
        url: String,
        home: PathBuf,
    }

    impl Served {
        /// With every request the tester's, the owner of `demo`, `other` and `default`, from
        /// `https://allowed.example`.
        async fn spawn(name: &str) -> Served {
            Served::spawn_over(name, async |home| {
                let projects = FileMembership::new(ProjectStore::new(home));
                for project in ["demo", "other", DEFAULT_PROJECT] {
                    projects.create(project, "tester").await.unwrap();
                }
                let access = Access::new(
                    vec!["https://allowed.example".to_owned()],
                    Arc::new(Tester),
                    Arc::new(projects),
                    ProjectStore::new(home),
                );
                super::router(Arc::new(access.unwrap()), axum::Router::new())
            })
            .await
        }

        /// What `serving` makes over an empty home of its own.
        async fn spawn_over(
            name: &str,
            serving: impl AsyncFnOnce(&Path) -> axum::Router,
        ) -> Served {
            let home =
                std::env::temp_dir().join(format!("virtx-tcp-app-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(&home).unwrap();
            let router = serving(&home).await;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let task = tokio::spawn(async move { axum::serve(listener, router).await });
            Served { task, url, home }
        }

        /// `method` sent to `path` in `project`: the HTTP status, and the body.
        async fn send(
            &self,
            method: &str,
            path: &str,
            project: Option<&str>,
            body: &'static str,
        ) -> (StatusCode, String) {
            let method = Method::from_bytes(method.as_bytes()).unwrap();
            let mut request = reqwest::Client::new()
                .request(method, format!("{}{path}", self.url))
                .body(body);
            if let Some(project) = project {
                request = request.header("virtx-project", project);
            }
            let response = request.send().await.unwrap();
            let status = response.status();
            (status, response.text().await.unwrap())
        }

        /// A client in `demo`.
        fn connect(&self) -> Client {
            let mut headers = http::HeaderMap::new();
            headers.insert("virtx-project", "demo".parse().unwrap());
            Client::connect(&self.url, Auth::None, headers).unwrap()
        }

        /// File an empty image under `tag` in `demo`'s images.
        fn file_image(&self, tag: &str) {
            let store = ImageStore::new(self.home.join("project/demo/images")).unwrap();
            let layer = store.insert_layer(&FsTree::new()).unwrap();
            let config = serde_json::from_str(
                r#"{"architecture": "arm64", "os": "linux", "rootfs": {"type": "layers", "diff_ids": []}}"#,
            )
            .unwrap();
            let image = store.insert_image(&config, &[layer]).unwrap();
            store.insert_tag(&tag.parse().unwrap(), &image).unwrap();
        }

        /// A port's URL, as a client resolves what making its sandbox answered: against the URL
        /// it called, as a WebSocket's.
        fn port_url(&self, url: &str) -> Url {
            let mut url = Url::parse(&self.url).unwrap().join(url).unwrap();
            url.set_scheme("ws").unwrap();
            url
        }

        /// A WebSocket to `url`, offering `protocol`: the socket, or the status it was refused with.
        async fn open(url: &Url, protocol: Option<&str>) -> Result<Socket, u16> {
            let mut request = url.as_str().into_client_request().unwrap();
            if let Some(protocol) = protocol {
                request
                    .headers_mut()
                    .insert("sec-websocket-protocol", protocol.parse().unwrap());
            }
            match tokio_tungstenite::connect_async(request).await {
                Ok((socket, _)) => Ok(socket),
                Err(tungstenite::Error::Http(response)) => Err(response.status().as_u16()),
                Err(e) => panic!("{e}"),
            }
        }
    }

    impl Drop for Served {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    /// The HTTP status a request was refused with.
    fn status<T: std::fmt::Debug>(answered: Result<T, virtx_core::Error>) -> u16 {
        match answered {
            Err(virtx_core::Error::Status(status, _)) => status,
            other => panic!("expected a status, got {other:?}"),
        }
    }

    fn names(entries: Vec<DirEntry>) -> Vec<String> {
        entries.into_iter().map(|entry| entry.name).collect()
    }

    #[tokio::test]
    async fn a_server_says_where_to_sign_in_and_takes_no_request_without_a_token() {
        let oidc = json!({ "issuer": "https://idp.example", "client_id": "virtx" });
        let served = Served::spawn_over("advertised", async |home| {
            let config = json!({ "listen": "127.0.0.1:0", "home": home, "oidc": oidc });
            super::configured(&Config::parse(config).unwrap())
                .await
                .unwrap()
                .1
        })
        .await;
        let (status, body) = served.send("GET", "auth", None, "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap(),
            json!({ "oidc": oidc })
        );
        let list = served.send("GET", "volumes/", None, "").await;
        assert_eq!(list.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn members_make_read_and_remove_projects_as_their_roles_let_them() {
        let served = Served::spawn_over("projects", async |home| {
            let users = ["alice", "bob", "carol"].map(str::to_owned).into();
            let access = Access::new(
                Vec::new(),
                Arc::new(TokenIsUser(users)),
                Arc::new(FileMembership::new(ProjectStore::new(home))),
                ProjectStore::new(home),
            );
            super::router(Arc::new(access.unwrap()), axum::Router::new())
        })
        .await;
        let send = async |method: &str, path: &str, token: &str, body: Value| {
            let method = Method::from_bytes(method.as_bytes()).unwrap();
            let mut request = reqwest::Client::new()
                .request(method, format!("{}{path}", served.url))
                .bearer_auth(token);
            if !body.is_null() {
                request = request.json(&body);
            }
            let response = request.send().await.unwrap();
            let location = response.headers().get("location").cloned();
            let status = response.status().as_u16();
            let text = response.text().await.unwrap();
            (
                status,
                serde_json::from_str(&text).unwrap_or(Value::Null),
                location,
            )
        };
        let status = async |method, path, token, body| send(method, path, token, body).await.0;
        let none = Value::Null;

        // Whoever signs in makes a project, and owns it: one of a name in use is refused.
        let demo = json!({ "name": "demo" });
        let (made, answer, location) = send("POST", "projects/", "alice", demo.clone()).await;
        assert_eq!(
            (made, &answer),
            (201, &json!({ "name": "demo", "role": "owner" }))
        );
        assert_eq!(location.unwrap(), "/projects/demo");
        assert_eq!(
            status("POST", "projects/", "bob", json!({ "name": "his" })).await,
            201
        );
        assert_eq!(
            status("POST", "projects/", "alice", demo.clone()).await,
            409
        );
        assert_eq!(
            status("POST", "projects/", "alice", json!({ "name": "Demo" })).await,
            400
        );
        assert_eq!(status("POST", "projects/", "nobody", demo).await, 401);
        let listed = async |token| send("GET", "projects/", token, Value::Null).await.1;
        assert_eq!(
            listed("alice").await,
            json!([{ "name": "demo", "role": "owner" }])
        );
        assert_eq!(
            listed("bob").await,
            json!([{ "name": "his", "role": "owner" }])
        );

        // Bob reaches it, and what is in it, once Alice adds him.
        let volumes = async |token: &str| {
            let request = reqwest::Client::new()
                .get(format!("{}volumes/", served.url))
                .bearer_auth(token)
                .header("virtx-project", "demo");
            request.send().await.unwrap().status().as_u16()
        };
        assert_eq!(
            status("GET", "projects/demo", "bob", none.clone()).await,
            403
        );
        assert_eq!(volumes("bob").await, 403);
        let member = json!({ "role": "member" });
        let bob = "projects/demo/members/bob";
        let (added, answer, location) = send("PUT", bob, "alice", member.clone()).await;
        assert_eq!(
            (added, &answer),
            (201, &json!({ "user": "bob", "role": "member" }))
        );
        assert_eq!(location.unwrap(), "/projects/demo/members/bob");
        assert_eq!(status("PUT", bob, "alice", member.clone()).await, 200);
        assert_eq!(
            send("GET", "projects/demo", "bob", none.clone()).await.1,
            json!({ "name": "demo", "role": "member" })
        );
        assert_eq!(volumes("bob").await, 200);
        assert_eq!(
            send("GET", "projects/demo/members/", "bob", none.clone())
                .await
                .1,
            json!([{ "user": "alice", "role": "owner" }, { "user": "bob", "role": "member" }])
        );

        // A member neither changes the members nor removes the project; the last owner stays.
        let (alice, carol) = ("projects/demo/members/alice", "projects/demo/members/carol");
        assert_eq!(status("PUT", carol, "bob", member).await, 403);
        assert_eq!(status("DELETE", alice, "bob", none.clone()).await, 403);
        assert_eq!(
            status("DELETE", "projects/demo", "bob", none.clone()).await,
            403
        );
        assert_eq!(status("DELETE", alice, "alice", none.clone()).await, 409);
        assert_eq!(status("DELETE", carol, "alice", none.clone()).await, 404);

        // Bob leaves; Alice removes the project, and everything in it.
        assert_eq!(status("DELETE", bob, "bob", none.clone()).await, 204);
        assert_eq!(volumes("bob").await, 403);
        assert!(served.home.join("project/demo").is_dir());
        assert_eq!(
            status("DELETE", "projects/demo", "alice", none.clone()).await,
            204
        );
        assert!(!served.home.join("project/demo").exists());
        assert_eq!(status("GET", "projects/demo", "alice", none).await, 403);
        assert_eq!(listed("alice").await, json!([]));
    }

    #[tokio::test]
    async fn a_browser_is_let_in_from_the_config_s_origins_alone() {
        let served = Served::spawn("origins").await;
        let from = async |origin| {
            reqwest::Client::new()
                .get(format!("{}snapshots/", served.url))
                .header("virtx-project", "demo")
                .header("origin", origin)
                .send()
                .await
                .unwrap()
                .status()
        };
        assert_eq!(from("https://allowed.example").await, StatusCode::OK);
        assert_eq!(from("https://evil.example").await, StatusCode::FORBIDDEN);

        // A browser's preflight is answered for the WebDAV methods from
        // an origin let in, and left without its answer from any other.
        let files = format!("{}volumes/data/files/", served.url);
        let preflight = async |origin| {
            reqwest::Client::new()
                .request(Method::OPTIONS, &files)
                .header("origin", origin)
                .header("access-control-request-method", "PROPFIND")
                .header(
                    "access-control-request-headers",
                    "authorization, virtx-project, depth",
                )
                .send()
                .await
                .unwrap()
        };
        let cleared = preflight("https://allowed.example").await;
        assert!(cleared.status().is_success(), "{}", cleared.status());
        let header = |name| {
            cleared
                .headers()
                .get(name)
                .map(|value| value.to_str().unwrap().to_owned())
        };
        assert_eq!(
            header("access-control-allow-origin").as_deref(),
            Some("https://allowed.example")
        );
        let methods = header("access-control-allow-methods").unwrap();
        assert!(
            methods.contains("PROPFIND") && methods.contains("MKCOL"),
            "{methods}"
        );
        let headers = header("access-control-allow-headers")
            .unwrap()
            .to_lowercase();
        assert!(
            headers.contains("depth") && headers.contains("virtx-project"),
            "{headers}"
        );
        let refused = preflight("https://evil.example").await;
        assert!(
            refused
                .headers()
                .get("access-control-allow-origin")
                .is_none()
        );

        // The request it clears carries the origin's answer too.
        served.connect().create_volume(Some("data")).await.unwrap();
        let listed = reqwest::Client::new()
            .request(Method::from_bytes(b"PROPFIND").unwrap(), &files)
            .header("virtx-project", "demo")
            .header("origin", "https://allowed.example")
            .header("depth", "1")
            .send()
            .await
            .unwrap();
        assert_eq!(listed.status(), StatusCode::MULTI_STATUS);
        assert_eq!(
            listed.headers().get("access-control-allow-origin").unwrap(),
            "https://allowed.example"
        );
    }

    /// A project's images are an OCI registry at `/images/oci/v2/`, and no other project's.
    #[tokio::test]
    async fn a_project_s_images_are_a_registry() {
        let served = Served::spawn("registry").await;
        served.file_image("alpine:3.21");
        let base = format!("{}images/oci/v2", served.url);
        let get = async |path: &str, project: &str| {
            reqwest::Client::new()
                .get(format!("{base}/{path}"))
                .header("virtx-project", project)
                .send()
                .await
                .unwrap()
        };
        assert_eq!(get("", "demo").await.status(), StatusCode::OK);

        let manifest = get("alpine/manifests/3.21", "demo").await;
        assert_eq!(manifest.status(), StatusCode::OK);
        let manifest: Value = manifest.json().await.unwrap();
        let layer = &manifest["layers"][0];
        assert_eq!(layer["mediaType"], "application/vnd.erofs.layer.v1");
        let digest = layer["digest"].as_str().unwrap();
        let blob = get(&format!("alpine/blobs/{digest}"), "demo").await;
        assert_eq!(blob.status(), StatusCode::OK);
        assert_eq!(
            format!("sha256:{:x}", Sha256::digest(blob.bytes().await.unwrap())),
            layer["digest"]
        );

        // Another project has none of them, by tag or by digest.
        assert_eq!(get("", "other").await.status(), StatusCode::OK);
        let theirs = get("alpine/manifests/3.21", "other").await;
        assert_eq!(theirs.status(), StatusCode::NOT_FOUND);
        let theirs = get(&format!("alpine/blobs/{digest}"), "other").await;
        assert_eq!(theirs.status(), StatusCode::NOT_FOUND);
    }

    /// The files of a volume and a snapshot through `virtx-core`'s client, given the project at
    /// connect, as a program over the bindings reaches them.
    #[tokio::test]
    async fn the_client_reaches_a_project_with_the_headers_given_at_connect() {
        let served = Served::spawn("client").await;
        let client = served.connect();
        client.create_volume(Some("data")).await.unwrap();
        client.create_snapshot(Some("work"), Some(1)).await.unwrap();
        let volume = FileRoot::Volume("data".into());
        let snapshot = FileRoot::Snapshot("work".into());
        let home = served.home.join("project/demo");

        // A volume's files, on the host's disk.
        client.create_dir(&volume, "dir").await.unwrap();
        client
            .write_file(&volume, "dir/a.txt", "hello world", None)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(home.join("volume/data/files/dir/a.txt")).unwrap(),
            b"hello world"
        );
        let metadata = client.stat_file(&volume, "dir/a.txt").await.unwrap();
        assert!(!metadata.is_dir && metadata.size == 11, "{metadata:?}");
        assert_eq!(
            names(client.read_dir(&volume, "dir").await.unwrap()),
            ["a.txt"]
        );
        assert_eq!(names(client.read_dir(&volume, "").await.unwrap()), ["dir"]);
        let read = client
            .read_file(&volume, "dir/a.txt", 6, Some(3))
            .await
            .unwrap();
        assert_eq!(&read[..], b"wor");
        client
            .rename_file(&volume, "dir/a.txt", "b.txt", false)
            .await
            .unwrap();
        client
            .copy_file(&volume, "b.txt", "c.txt", false)
            .await
            .unwrap();
        assert_eq!(
            status(client.copy_file(&volume, "b.txt", "c.txt", false).await),
            412
        );
        client
            .copy_file(&volume, "dir", "copied", false)
            .await
            .unwrap();
        client.remove_file(&volume, "dir", false).await.unwrap();
        assert_eq!(status(client.stat_file(&volume, "dir").await), 404);
        assert!(client.stat_file(&volume, "copied").await.unwrap().is_dir);
        let read = client.read_file(&volume, "c.txt", 0, None).await.unwrap();
        assert_eq!(&read[..], b"hello world");
        assert_eq!(
            status(client.write_file(&volume, "no/file", "x", None).await),
            409
        );
        let nobody = FileRoot::Volume("nobody".into());
        assert_eq!(status(client.stat_file(&nobody, "").await), 404);

        // A snapshot's files, in its image.
        assert_eq!(
            names(client.read_dir(&snapshot, "").await.unwrap()),
            [] as [&str; 0]
        );
        client.create_dir(&snapshot, "etc").await.unwrap();
        client
            .write_file(&snapshot, "etc/hostname", "guest\n", None)
            .await
            .unwrap();
        assert_eq!(
            status(client.write_file(&snapshot, "no/file", "x", None).await),
            409
        );
        assert_eq!(status(client.create_dir(&snapshot, "etc").await), 405);
        let read = client
            .read_file(&snapshot, "etc/hostname", 1, Some(2))
            .await
            .unwrap();
        assert_eq!(&read[..], b"ue");
        client
            .rename_file(&snapshot, "etc/hostname", "hostname", false)
            .await
            .unwrap();
        client
            .copy_file(&snapshot, "hostname", "etc/copy", false)
            .await
            .unwrap();
        assert_eq!(
            status(
                client
                    .copy_file(&snapshot, "hostname", "etc/copy", false)
                    .await
            ),
            412
        );
        client
            .remove_file(&snapshot, "hostname", false)
            .await
            .unwrap();
        assert_eq!(status(client.stat_file(&snapshot, "hostname").await), 404);
        let image = std::fs::File::open(home.join("snapshot/work.ext4")).unwrap();
        let tree = FsTree::from_ext4(image).unwrap();
        let paths: Vec<_> = tree
            .walk()
            .into_iter()
            .map(|(path, ..)| String::from_utf8(path).unwrap())
            .collect();
        assert_eq!(
            paths,
            ["", "lost+found", "upper", "upper/etc", "upper/etc/copy"]
        );
        assert!(matches!(
            tree.entry(b"upper/etc/copy").unwrap(),
            Some(Entry::File { data, .. }) if data == b"guest\n"
        ));
        let lock = std::fs::File::create(home.join("snapshot/work.ext4.lock")).unwrap();
        lock.try_lock().unwrap();
        assert_eq!(status(client.read_dir(&snapshot, "").await), 423);
        assert_eq!(
            status(client.write_file(&snapshot, "x", "y", None).await),
            423
        );
        drop(lock);
        let nobody = FileRoot::Snapshot("nobody".into());
        assert_eq!(status(client.read_dir(&nobody, "").await), 404);
    }

    #[tokio::test]
    async fn a_project_keeps_its_snapshots_and_volumes() {
        let served = Served::spawn("project").await;
        let client = served.connect();
        let created = client.create_snapshot(Some("work"), Some(1)).await.unwrap();
        assert_eq!(created.name, "work");
        assert!(
            served
                .home
                .join("project/demo/snapshot/work.ext4")
                .is_file()
        );
        assert_eq!(client.list_snapshot().await.unwrap(), [created]);

        client.create_volume(Some("data")).await.unwrap();
        assert!(served.home.join("project/demo/volume/data/files").is_dir());
        assert_eq!(status(client.create_volume(Some("data")).await), 409);

        // A port that is none, and a sandbox of an image nobody filed, are refused before it
        // boots.
        let mut args = SandboxCreateArgs::new("a:b");
        args.ports = vec![0];
        assert_eq!(status(client.create_sandbox(&args).await), 400);
        let args = SandboxCreateArgs::new("a:b");
        assert_eq!(status(client.create_sandbox(&args).await), 404);
    }

    /// A port is a WebSocket of `virtx.tcp` and nothing else, behind a capability no one guesses.
    #[tokio::test]
    async fn a_port_is_a_websocket_behind_its_capability() {
        let served = Served::spawn("ports").await;
        let guessed = served.port_url(&format!("ports/{}", "0".repeat(64)));
        let mut plain = guessed.clone();
        plain.set_scheme("http").unwrap();
        let elsewhere = reqwest::Client::new()
            .get(plain.clone())
            .header("origin", "https://elsewhere.example")
            .send()
            .await
            .unwrap();
        assert_eq!(elsewhere.status(), StatusCode::FORBIDDEN);
        let plain = reqwest::get(plain).await.unwrap();
        assert_eq!(plain.status(), StatusCode::UPGRADE_REQUIRED);
        assert_eq!(Served::open(&guessed, None).await.err(), Some(400));
        assert_eq!(
            Served::open(&guessed, Some("virtx.tcp")).await.err(),
            Some(404)
        );
    }

    /// Two clients writing over one etag at once: the first to write lands, and the other is
    /// refused the etag it read, gone stale under it.
    #[tokio::test]
    async fn of_two_writes_over_one_etag_one_lands() {
        let served = Served::spawn("etag").await;
        let (a, b) = (served.connect(), served.connect());
        a.create_volume(Some("data")).await.unwrap();
        let root = FileRoot::Volume("data".into());
        let (from_a, from_b) = (vec![b'a'; 1 << 20], vec![b'b'; 1 << 20]);
        for round in 0..20 {
            a.write_file(&root, "read", format!("round {round}"), None)
                .await
                .unwrap();
            let etag = b.stat_file(&root, "read").await.unwrap().etag.unwrap();
            let (by_a, by_b) = tokio::join!(
                a.write_file(&root, "read", from_a.clone(), Some(&etag)),
                b.write_file(&root, "read", from_b.clone(), Some(&etag)),
            );
            let landed = match (by_a, by_b) {
                (Ok(()), Err(virtx_core::Error::Status(412, _))) => &from_a,
                (Err(virtx_core::Error::Status(412, _)), Ok(())) => &from_b,
                (by_a, by_b) => panic!("round {round}, over one etag: {by_a:?} and {by_b:?}"),
            };
            let read = a.read_file(&root, "read", 0, None).await.unwrap();
            assert!(read == *landed, "round {round}: not the write that landed");
        }
    }
}
