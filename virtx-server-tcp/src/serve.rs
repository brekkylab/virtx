//! The server's routes, for an [`Access`]: the endpoints of the protocol ([`crate::endpoints`]),
//! each answered from the home its request reaches, the images as an OCI registry, and each
//! sandbox's ports. A server built on this one gives [`router`] an authenticator of its own in the
//! access, and routes of its own beside these; the server a config alone makes is [`configured`],
//! which does both.
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
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
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
/// to sign in as -- and the projects themselves ([`projects`]).
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
    let own = axum::Router::new()
        .route(
            "/auth",
            axum::routing::get(move || async move { advertised }),
        )
        .merge(projects(&access));
    let router = router(Arc::clone(&access), own);
    Ok((access, router))
}

/// The projects themselves, and their members, as `access`'s membership answers them: for a
/// server to merge into the routes it gives [`router`], where it serves them. One that keeps its
/// projects elsewhere, and serves them there, merges none.
pub fn projects(access: &Arc<Access>) -> axum::Router {
    endpoints::projects().layer(Extension(Arc::clone(access)))
}

/// Everything served in a project, each request checked against `access`: the endpoints, in the
/// home a request reaches, the images' registry at `/images/oci/v2/`, the ports, and the CORS a
/// browser at one of its origins has -- with `own` beside them, routes that take no home and
/// check for themselves, such as the projects themselves ([`projects`]).
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

/// `router` served on `listener` until this process is asked to stop.
pub async fn run(listener: tokio::net::TcpListener, router: axum::Router) -> anyhow::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(interrupted())
        .await?;
    Ok(())
}

/// Once this process is asked to stop: by ctrl-c; on Unix by `SIGTERM` too, as a service
/// manager, `docker stop` or a plain `kill` asks; on Windows by ctrl-break, the one console
/// event a process group of its own can be sent, and by its console's closing, a logoff or a
/// shutdown, each of which Windows gives a few seconds before it ends the process.
async fn interrupted() {
    #[cfg(unix)]
    if let Ok(mut terminate) =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
        return;
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows;
        if let (Ok(mut ctrl_break), Ok(mut close), Ok(mut logoff), Ok(mut shutdown)) = (
            windows::ctrl_break(),
            windows::ctrl_close(),
            windows::ctrl_logoff(),
            windows::ctrl_shutdown(),
        ) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = ctrl_break.recv() => {}
                _ = close.recv() => {}
                _ = logoff.recv() => {}
                _ = shutdown.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
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
            Method::PATCH,
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
            header::IF_NONE_MATCH,
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
/// when the port is not reached, such as when no machine runs, saying why as the close's reason.
async fn relay(mut socket: WebSocket, tunnel: Tunnel) {
    let mut reached = match tunnel.connect().await {
        Ok(reached) => reached,
        Err(e) => {
            let frame = CloseFrame {
                code: close_code::ERROR,
                reason: close_reason(&e.to_string()).into(),
            };
            let _ = socket.send(Message::Close(Some(frame))).await;
            return;
        }
    };
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
    let _ = socket.send(Message::Close(None)).await;
}

/// `said` as a close's reason, which a WebSocket holds 123 bytes of: cut at a character's end.
fn close_reason(said: &str) -> String {
    let mut end = said.len().min(123);
    while !said.is_char_boundary(end) {
        end -= 1;
    }
    said[..end].to_owned()
}

#[cfg(test)]
mod tests {
    //! The app, served on the loopback over an identity of the test's own, and called over HTTP
    //! as a client would: each request to an endpoint in a project -- by hand, and through
    //! `virtx-core`'s client given it at connect -- and to the projects themselves. Who a
    //! provider's token is, is `virtx-rs`'s `tests/projects.rs`, against the fixture's.

    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    use reqwest::{Method, StatusCode, Url};
    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};
    use tokio::io::AsyncReadExt as _;
    use tokio_tungstenite::tungstenite;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use virtx_core::VolumeListOutputItem;
    use virtx_core::{
        Auth, Client, DirEntry, FileRoot, ImageJob, InboundRule, PortProtocol, PortSpec, Recipe,
        SandboxCreateArgs,
    };
    use virtx_uvm::backend::{Held, Refusal, Stored, Wanted};
    use virtx_uvm::{
        FsTree, ImageStore, ProjectId, PublicRegistries, Reference, RegistryProxy, StorageBackend,
        VolumeStore,
    };

    use std::sync::Arc;

    use futures_util::StreamExt as _;
    use futures_util::future::BoxFuture;

    use virtx_uvm::ProjectStore;

    use crate::access::{Access, Homes, Identity, Principal, TokenIsUser};
    use crate::config::Config;
    use crate::project::{Membership as _, TcpMembership};

    /// Every request the tester's, whatever it carries.
    struct Tester;

    impl Identity for Tester {
        fn identify<'a>(&'a self, _token: Option<&'a str>) -> BoxFuture<'a, Option<Principal>> {
            Box::pin(async { Some(Principal::new("tester")) })
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
        /// With every request the tester's, the owner of `demo` and `other`, from
        /// `https://allowed.example`, and the projects served as the binary serves them.
        async fn spawn(name: &str) -> Served {
            Served::spawn_through(name, None).await
        }

        /// As [`Served::spawn`], its image jobs connecting to registries through `registries`
        /// where given.
        async fn spawn_through(name: &str, registries: Option<Arc<dyn RegistryProxy>>) -> Served {
            Served::spawn_over(name, async |home| {
                let projects = TcpMembership::new(home);
                let tester = Principal::new("tester");
                for project in ["demo", "other"] {
                    projects.create(&tester, project).await.unwrap();
                }
                let store = ProjectStore::new(home);
                let mut access = Access::new(
                    vec!["https://allowed.example".to_owned()],
                    Arc::new(Tester),
                    Arc::new(projects),
                    store.clone(),
                    Arc::new(store),
                );
                if let Some(registries) = registries {
                    access = access.through(registries);
                }
                let access = Arc::new(access);
                super::router(Arc::clone(&access), super::projects(&access))
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

        /// The directory of the tester's project `name`.
        async fn project(&self, name: &str) -> PathBuf {
            let tester = Principal::new("tester");
            let projects = TcpMembership::new(&self.home);
            let id = projects.resolve(&tester, name).await.unwrap().unwrap();
            ProjectStore::new(&self.home).path(&id)
        }

        /// File an empty image under `tag` in `demo`'s images.
        async fn file_image(&self, tag: &str) {
            let store = ImageStore::new(self.project("demo").await.join("images")).unwrap();
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
            Err(virtx_core::Error::Status(status, _, _)) => status,
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
            let store = ProjectStore::new(home);
            let access = Access::new(
                Vec::new(),
                Arc::new(TokenIsUser(users)),
                Arc::new(TcpMembership::new(home)),
                store.clone(),
                Arc::new(store),
            );
            let access = Arc::new(access);
            super::router(Arc::clone(&access), super::projects(&access))
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

        // Whoever signs in makes a project, and owns it: one of a name in use of theirs is
        // refused, and of another's is another project.
        let demo = json!({ "name": "demo" });
        let (made, answer, location) = send("POST", "projects/", "alice", demo.clone()).await;
        let id = answer["id"].as_str().unwrap().to_owned();
        assert_eq!(
            (made, &answer),
            (
                201,
                &json!({ "id": id, "name": "demo", "owner": "alice", "role": "owner" })
            )
        );
        assert_eq!(location.unwrap(), format!("/projects/{id}"));
        assert_eq!(
            status("POST", "projects/", "alice", demo.clone()).await,
            409
        );
        let (made, his, _) = send("POST", "projects/", "bob", demo.clone()).await;
        assert_eq!(made, 201);
        assert_ne!(his["id"], answer["id"]);
        for name in ["default", "Demo", "0f6a3b8e-1c2d-4e5f-8a9b-0c1d2e3f4a5b"] {
            let made = status("POST", "projects/", "alice", json!({ "name": name })).await;
            assert_eq!(made, 400, "{name}");
        }
        assert_eq!(status("POST", "projects/", "nobody", demo).await, 401);
        // Each lists their own, made for them, and theirs by name.
        let listed = async |token| {
            let listed = send("GET", "projects/", token, Value::Null).await.1;
            let listed = listed.as_array().unwrap().iter();
            listed
                .map(|project| (project["name"].clone(), project["owner"].clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            listed("alice").await,
            [
                (json!("default"), json!("alice")),
                (json!("demo"), json!("alice"))
            ]
        );
        assert_eq!(
            listed("bob").await,
            [
                (json!("default"), json!("bob")),
                (json!("demo"), json!("bob"))
            ]
        );

        // Bob reaches it by its id, and what is in it, once Alice adds him; by its name, he
        // names his own.
        let project = format!("projects/{id}");
        let volumes = async |token: &str| {
            let request = reqwest::Client::new()
                .get(format!("{}volumes/", served.url))
                .bearer_auth(token)
                .header("virtx-project", &id);
            request.send().await.unwrap().status().as_u16()
        };
        assert_eq!(status("GET", &project, "bob", none.clone()).await, 403);
        assert_eq!(volumes("bob").await, 403);
        let member = json!({ "role": "member" });
        let bob = format!("{project}/members/bob");
        let (added, answer, location) = send("PUT", &bob, "alice", member.clone()).await;
        assert_eq!(
            (added, &answer),
            (201, &json!({ "user": "bob", "role": "member" }))
        );
        assert_eq!(location.unwrap(), format!("/{bob}"));
        assert_eq!(status("PUT", &bob, "alice", member.clone()).await, 200);
        assert_eq!(
            send("GET", &project, "bob", none.clone()).await.1,
            json!({ "id": id, "name": "demo", "owner": "alice", "role": "member" })
        );
        assert_eq!(
            send("GET", "projects/demo", "bob", none.clone()).await.1["id"],
            his["id"]
        );
        assert_eq!(volumes("bob").await, 200);
        assert_eq!(
            send("GET", &format!("{project}/members/"), "bob", none.clone())
                .await
                .1,
            json!([{ "user": "alice", "role": "owner" }, { "user": "bob", "role": "member" }])
        );

        // A member neither changes the members nor removes the project; its owner stays one.
        let (alice, carol) = (
            format!("{project}/members/alice"),
            format!("{project}/members/carol"),
        );
        assert_eq!(status("PUT", &carol, "bob", member).await, 403);
        assert_eq!(status("DELETE", &alice, "bob", none.clone()).await, 403);
        assert_eq!(status("DELETE", &project, "bob", none.clone()).await, 403);
        assert_eq!(status("DELETE", &alice, "alice", none.clone()).await, 409);
        let demoted = json!({ "role": "member" });
        assert_eq!(status("PUT", &alice, "alice", demoted).await, 409);
        assert_eq!(status("DELETE", &carol, "alice", none.clone()).await, 404);

        // Its owner takes a member out.
        let member = json!({ "role": "member" });
        assert_eq!(status("PUT", &carol, "alice", member).await, 201);
        assert_eq!(volumes("carol").await, 200);
        assert_eq!(status("DELETE", &carol, "alice", none.clone()).await, 204);
        assert_eq!(volumes("carol").await, 403);

        // Bob leaves; Alice removes the project, and everything in it.
        assert_eq!(status("DELETE", &bob, "bob", none.clone()).await, 204);
        assert_eq!(volumes("bob").await, 403);
        let dir = served.home.join("project").join(&id);
        assert!(dir.is_dir());
        assert_eq!(
            status("DELETE", "projects/demo", "alice", none.clone()).await,
            204
        );
        assert!(!dir.exists());
        assert_eq!(status("GET", &project, "alice", none).await, 403);
        assert_eq!(listed("alice").await, [(json!("default"), json!("alice"))]);
    }

    /// A server built on this one serves the projects only where it merges them: one that keeps
    /// them elsewhere answers none of its own.
    #[tokio::test]
    async fn the_projects_are_served_only_where_a_server_merges_them() {
        let served = Served::spawn_over("unmerged", async |home| {
            let projects = TcpMembership::new(home);
            projects
                .create(&Principal::new("tester"), "demo")
                .await
                .unwrap();
            let store = ProjectStore::new(home);
            let access = Access::new(
                Vec::new(),
                Arc::new(Tester),
                Arc::new(projects),
                store.clone(),
                Arc::new(store),
            );
            super::router(Arc::new(access), axum::Router::new())
        })
        .await;
        let (status, body) = served.send("GET", "projects/", None, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        let (status, _) = served.send("GET", "projects/demo", None, "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Its endpoints in a project answer as before.
        let (status, _) = served.send("GET", "volumes/", Some("demo"), "").await;
        assert_eq!(status, StatusCode::OK);

        let served = Served::spawn_over("merged", async |home| {
            let projects = TcpMembership::new(home);
            projects
                .create(&Principal::new("tester"), "demo")
                .await
                .unwrap();
            let store = ProjectStore::new(home);
            let access = Access::new(
                Vec::new(),
                Arc::new(Tester),
                Arc::new(projects),
                store.clone(),
                Arc::new(store),
            );
            let access = Arc::new(access);
            super::router(Arc::clone(&access), super::projects(&access))
        })
        .await;
        let (status, body) = served.send("GET", "projects/", None, "").await;
        assert_eq!(status, StatusCode::OK);
        let listed: Vec<Value> = serde_json::from_str(&body).unwrap();
        assert!(
            listed.iter().any(|project| project["name"] == "demo"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn a_browser_is_let_in_from_the_config_s_origins_alone() {
        let served = Served::spawn("origins").await;
        let from = async |origin| {
            reqwest::Client::new()
                .get(format!("{}volumes/", served.url))
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
            methods.contains("PROPFIND") && methods.contains("MKCOL") && methods.contains("PATCH"),
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
        served.file_image("alpine:3.21").await;
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

    /// What `job` failed with, once it has ended.
    async fn failed(client: &Client, job: ImageJob) -> String {
        // A guard against a hang, not a measure of anything.
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                let job = client.get_image_job(&job.id).await.unwrap();
                if !job.running {
                    break job.error.expect("the job did not fail");
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the job went on")
    }

    /// A server's image jobs are held to registries on public addresses: a pull, a push and a
    /// build's base that name one on this host's loopback each fail saying so, and it is
    /// connected to not at all.
    #[tokio::test]
    async fn an_image_job_does_not_connect_to_a_registry_on_this_host() {
        let served = Served::spawn("registries").await;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let host = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let client = served.connect();
        let unreachable = |reference: &str| {
            format!(
                "{reference}: {host} is not reachable from this server: \
                 it is not a public address"
            )
        };

        let pulled = format!("{host}/team/app:1");
        let job = client.pull_image(&pulled, None).await.unwrap();
        assert_eq!(failed(&client, job).await, unreachable(&pulled));

        let pushed = format!("{host}/team/app:2");
        served.file_image(&pushed).await;
        let job = client.push_image(&pushed, None).await.unwrap();
        assert_eq!(failed(&client, job).await, unreachable(&pushed));

        let base = format!("{host}/team/base:1");
        let job = client
            .build_image(Recipe::new(&base), "built:1")
            .await
            .unwrap();
        assert_eq!(failed(&client, job).await, unreachable(&base));

        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "the registry was connected to"
        );
    }

    /// Two servers in one process, each through its own registry proxy: a pull from a registry
    /// on this host's loopback fails on the server whose proxy names none, without a
    /// connection, and reaches it on the server whose proxy names it, which still fails a pull
    /// from a registry beside it that it does not name.
    #[tokio::test]
    async fn each_server_reaches_the_private_registries_its_own_proxy_names() {
        let [named, other] = [(); 2].map(|_| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            listener
        });
        let [named_host, other_host] =
            [&named, &other].map(|listener| listener.local_addr().unwrap().to_string());
        let naming = PublicRegistries::new([&named_host]).unwrap();
        let naming = Served::spawn_through("naming", Some(Arc::new(naming))).await;
        let public = Served::spawn("public").await;
        let unreachable = |reference: &str, host: &str| {
            format!(
                "{reference}: {host} is not reachable from this server: it is not a public address"
            )
        };
        let not_connected = |listener: &std::net::TcpListener| {
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock,
                "the registry was connected to"
            );
        };

        let pulled = format!("{named_host}/team/app:1");
        let client = public.connect();
        let job = client.pull_image(&pulled, None).await.unwrap();
        assert_eq!(
            failed(&client, job).await,
            unreachable(&pulled, &named_host)
        );
        not_connected(&named);

        let client = naming.connect();
        client.pull_image(&pulled, None).await.unwrap();
        let named = tokio::net::TcpListener::from_std(named).unwrap();
        // A guard against a hang, not a measure of anything.
        let first = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            let (mut stream, _) = named.accept().await.unwrap();
            stream.read_u8().await.unwrap()
        })
        .await
        .expect("the registry named was not connected to");
        // A TLS handshake's record: the job reached the listener itself.
        assert_eq!(first, 0x16);

        let beside = format!("{other_host}/team/app:1");
        let job = client.pull_image(&beside, None).await.unwrap();
        assert_eq!(
            failed(&client, job).await,
            unreachable(&beside, &other_host)
        );
        not_connected(&other);
    }

    /// A server given Docker Hub's mirror: a pull and a build's base of Docker Hub's image
    /// each connect to the mirror first.
    #[tokio::test]
    async fn docker_hub_s_images_are_pulled_from_the_mirror_a_server_is_given() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let mirror = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = mirror.local_addr().unwrap().to_string();
        // The mirror: each connection counted, and closed once it has said anything.
        let reached = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&reached);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = mirror.accept().await {
                if stream.read_u8().await.is_ok() {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        let registries = PublicRegistries::new([&host]).unwrap();
        let served = Served::spawn_over("mirror", async |home| {
            let projects = TcpMembership::new(home);
            projects
                .create(&Principal::new("tester"), "demo")
                .await
                .unwrap();
            let store = ProjectStore::new(home);
            let access = Access::new(
                Vec::new(),
                Arc::new(Tester),
                Arc::new(projects),
                store.clone(),
                Arc::new(store),
            )
            .through(Arc::new(registries))
            .mirroring(Some(&host));
            super::router(Arc::new(access), axum::Router::new())
        })
        .await;
        let client = served.connect();
        // A name no one has, so that each job fails once it has been to the mirror and to
        // Docker Hub.
        let missing = "virtx-test-no-such/image:1";

        let job = client.pull_image(missing, None).await.unwrap();
        failed(&client, job).await;
        assert!(
            reached.swap(0, Ordering::SeqCst) > 0,
            "the pull did not reach the mirror"
        );

        let job = client
            .build_image(Recipe::new(missing), "built:1")
            .await
            .unwrap();
        failed(&client, job).await;
        assert!(
            reached.load(Ordering::SeqCst) > 0,
            "the build did not reach the mirror"
        );
    }

    /// Needs a network: `cargo test -p virtx-server-tcp -- --ignored`. Homes made sharing their
    /// anonymous pulls: a pull in one, and a build's base in another, ask where they are shared
    /// for the image before pulling it, and tell it of the image pulled.
    #[tokio::test]
    #[ignore]
    async fn a_home_s_anonymous_pulls_are_shared_where_it_was_made_to_share_them() {
        use virtx_uvm::{Digest, PublicImages};

        #[derive(Default)]
        struct Told {
            asked: std::sync::Mutex<Vec<Digest>>,
            pulled: std::sync::Mutex<Vec<Digest>>,
        }
        impl PublicImages for Told {
            fn fetch<'a>(
                &'a self,
                _: &'a ImageStore,
                origin: &'a Digest,
            ) -> BoxFuture<'a, anyhow::Result<Option<Digest>>> {
                self.asked.lock().unwrap().push(origin.clone());
                Box::pin(async { Ok(None) })
            }
            fn pulled(&self, origin: &Digest, _: &[u8], _: &[u8], _: &Digest) {
                self.pulled.lock().unwrap().push(origin.clone());
            }
        }
        struct Sharing(ProjectStore, Arc<Told>);
        impl crate::access::Homes for Sharing {
            fn make<'a>(
                &'a self,
                id: &'a virtx_uvm::ProjectId,
            ) -> BoxFuture<'a, anyhow::Result<Option<crate::endpoints::Home>>> {
                Box::pin(async move {
                    let home = self.0.make(id).await?;
                    let public: Arc<dyn PublicImages> = self.1.clone();
                    Ok(home.map(|home| home.sharing(Some(public))))
                })
            }
        }

        let told = Arc::new(Told::default());
        let shared = Arc::clone(&told);
        let served = Served::spawn_over("sharing", async |home| {
            let projects = TcpMembership::new(home);
            for project in ["demo", "other"] {
                projects
                    .create(&Principal::new("tester"), project)
                    .await
                    .unwrap();
            }
            let store = ProjectStore::new(home);
            let access = Access::new(
                Vec::new(),
                Arc::new(Tester),
                Arc::new(projects),
                store.clone(),
                Arc::new(Sharing(store, shared)),
            );
            super::router(Arc::new(access), axum::Router::new())
        })
        .await;
        let ended = async |client: &Client, job: ImageJob| {
            // A guard against a hang, not a measure of anything.
            tokio::time::timeout(std::time::Duration::from_secs(300), async {
                loop {
                    let job = client.get_image_job(&job.id).await.unwrap();
                    if !job.running {
                        break job.error;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            })
            .await
            .expect("the job went on")
        };
        let alpine = "mirror.gcr.io/library/alpine:3.21";

        let client = served.connect();
        let job = client.pull_image(alpine, None).await.unwrap();
        assert_eq!(ended(&client, job).await, None);
        let pulled = told.pulled.lock().unwrap().clone();
        assert_eq!(pulled.len(), 1);
        assert_eq!(*told.asked.lock().unwrap(), pulled);

        let mut headers = http::HeaderMap::new();
        headers.insert("virtx-project", "other".parse().unwrap());
        let other = Client::connect(&served.url, Auth::None, headers).unwrap();
        let job = other
            .build_image(Recipe::new(alpine), "built:1")
            .await
            .unwrap();
        assert_eq!(ended(&other, job).await, None);
        assert_eq!(
            *told.asked.lock().unwrap(),
            [pulled[0].clone(), pulled[0].clone()]
        );
        assert_eq!(told.pulled.lock().unwrap().len(), 2);
    }

    /// A refusal made before a handler runs is in the protocol's envelope too, where a client reads
    /// its code: a body over the server's limit is `413` `too_large`, naming the limit, and a path
    /// whose segment does not read as one is `400` `invalid`.
    #[tokio::test]
    async fn a_body_over_the_limit_and_a_path_that_does_not_read_are_refused_as_any_refusal_is() {
        let served = Served::spawn("envelope").await;
        let send = async |request: reqwest::RequestBuilder| {
            let response = request
                .header("virtx-project", "demo")
                .send()
                .await
                .unwrap();
            let status = response.status();
            let kind = response.headers()[http::header::CONTENT_TYPE].clone();
            let body: serde_json::Value = response.json().await.unwrap();
            (status, kind, body["error"].clone())
        };
        let client = reqwest::Client::new();

        let over = client
            .post(format!("{}sandboxes/", served.url))
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(vec![b' '; 3 << 20]);
        let (status, kind, error) = send(over).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(kind.to_str().unwrap(), "application/json");
        assert_eq!(error["code"], "too_large", "{error}");
        assert!(
            error["message"].as_str().unwrap().contains("2 MiB"),
            "{error}"
        );

        let unreadable = client.get(format!("{}sandboxes/%FF", served.url));
        let (status, kind, error) = send(unreadable).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(kind.to_str().unwrap(), "application/json");
        assert_eq!(error["code"], "invalid", "{error}");
    }

    /// A body or a path that does not read is refused `400` `invalid` in the server's words:
    /// the member or segment, what it takes, and what was sent.
    #[tokio::test]
    async fn a_body_or_a_path_that_does_not_read_is_refused_naming_what_it_takes() {
        let served = Served::spawn("unread").await;
        let image = r#""image": {"type": "tag", "tag": "alpine"}"#;
        let ports = format!(r#"{{{image}, "ports": ["5900"]}}"#);
        for (method, path, body, said) in [
            (
                "POST",
                "sandboxes/nobody/jobs/exec/",
                "not json",
                "the body: JSON, which it stops being at line 1, column 2",
            ),
            (
                "POST",
                "sandboxes/nobody/jobs/exec/",
                "[]",
                "the body: an object, not a list",
            ),
            (
                "POST",
                "sandboxes/nobody/jobs/exec/",
                "",
                "the body: a JSON object, and none was sent",
            ),
            (
                "POST",
                "sandboxes/nobody/jobs/exec/",
                r#"{"cmd": "ls"}"#,
                r#"cmd: a list, not "ls""#,
            ),
            (
                "POST",
                "sandboxes/nobody/jobs/j/signal",
                r#"{"signl": "TERM"}"#,
                "signl: not a member; the members are `signal`",
            ),
            (
                "PATCH",
                "sandboxes/nobody",
                r#"{"cpus": 2}"#,
                "cpus: not a member; the members are `vcpus`, `memory_mib`, `disk_gib`",
            ),
            (
                "POST",
                "images/jobs/pull/",
                "x",
                "the body: JSON, which it stops being at line 1, column 1",
            ),
            (
                "POST",
                "sandboxes/",
                &ports,
                r#"ports[0]: a whole number from 0 to 65535, or {"port", "label"?, "protocol"?, "inbound"?}, not "5900""#,
            ),
            (
                "GET",
                "sandboxes/%FF",
                "",
                "id: UTF-8 once its escapes are read, and it is not",
            ),
        ] {
            let response = reqwest::Client::new()
                .request(
                    Method::from_bytes(method.as_bytes()).unwrap(),
                    format!("{}{path}", served.url),
                )
                .header("virtx-project", "demo")
                .header(http::header::CONTENT_TYPE, "application/json")
                .body(body.to_owned())
                .send()
                .await
                .unwrap();
            let how = format!("{method} {path} {body}");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{how}");
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["error"]["code"], "invalid", "{how}");
            assert_eq!(body["error"]["message"], said, "{how}");
        }
    }

    /// Each route that takes a JSON body holds it to the limit: one a byte over is refused `413`
    /// `too_large`, one at the limit is not, each in the envelope. The body is refused before
    /// the handler looks anything up, so no sandbox, volume or job has to be there.
    #[tokio::test]
    async fn every_route_that_takes_a_body_holds_it_to_the_limit() {
        let served = Served::spawn("limit").await;
        let client = reqwest::Client::new();
        let limit = 2 << 20;
        for (method, path) in [
            (Method::POST, "images/jobs/pull/"),
            (Method::POST, "images/jobs/push/"),
            (Method::POST, "images/jobs/build/"),
            (Method::POST, "volumes/"),
            (Method::PUT, "volumes/nobody/mounts/m"),
            (Method::POST, "sandboxes/"),
            (Method::POST, "sandboxes/nobody/jobs/exec/"),
            (Method::POST, "sandboxes/nobody/jobs/j/signal"),
            (Method::PATCH, "sandboxes/nobody"),
        ] {
            for (size, status, code) in [
                (limit + 1, StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
                (limit, StatusCode::BAD_REQUEST, "invalid"),
            ] {
                let response = client
                    .request(method.clone(), format!("{}{path}", served.url))
                    .header("virtx-project", "demo")
                    .header(http::header::CONTENT_TYPE, "application/json")
                    .body(vec![b' '; size])
                    .send()
                    .await
                    .unwrap();
                let how = format!("{method} {path}, {size} bytes");
                assert_eq!(response.status(), status, "{how}");
                let kind = response.headers()[http::header::CONTENT_TYPE].clone();
                assert_eq!(kind, "application/json", "{how}");
                let body: Value = response.json().await.unwrap();
                assert_eq!(body["error"]["code"], code, "{how}");
            }
        }
    }

    /// A body is read to its end before it is refused, over a socket as a client sends it: with
    /// part of a body over the limit sent, nothing is answered; with the rest, `413`. A body that
    /// breaks off is `400` `invalid`.
    #[tokio::test]
    async fn a_body_over_the_limit_is_read_to_its_end_before_it_is_refused() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let served = Served::spawn("read-to-end").await;
        let at = served
            .url
            .trim_start_matches("http://")
            .trim_end_matches('/');
        let head = |length: usize| {
            format!(
                "POST /sandboxes/ HTTP/1.1\r\nhost: virtx\r\nvirtx-project: demo\r\n\
                 content-type: application/json\r\ncontent-length: {length}\r\n\r\n"
            )
        };

        let whole = 3 << 20;
        let first = (2 << 20) + (64 << 10);
        let mut socket = tokio::net::TcpStream::connect(at).await.unwrap();
        socket.write_all(head(whole).as_bytes()).await.unwrap();
        socket.write_all(&vec![b' '; first]).await.unwrap();
        let mut answer = [0u8; 64];
        let early = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            socket.read(&mut answer),
        );
        assert!(
            early.await.is_err(),
            "answered while the body was still owed"
        );
        socket.write_all(&vec![b' '; whole - first]).await.unwrap();
        let read = socket.read(&mut answer).await.unwrap();
        let line = String::from_utf8_lossy(&answer[..read]);
        assert!(line.starts_with("HTTP/1.1 413"), "{line}");

        let mut socket = tokio::net::TcpStream::connect(at).await.unwrap();
        socket.write_all(head(100).as_bytes()).await.unwrap();
        socket.write_all(b"{\"cmd\": [").await.unwrap();
        socket.shutdown().await.unwrap();
        let mut answer = Vec::new();
        socket.read_to_end(&mut answer).await.unwrap();
        let answer = String::from_utf8_lossy(&answer);
        assert!(answer.starts_with("HTTP/1.1 400"), "{answer}");
        assert!(answer.contains("\"invalid\""), "{answer}");
    }

    /// The files of a volume through `virtx-core`'s client, given the project at connect, as a
    /// program over the bindings reaches them.
    #[tokio::test]
    async fn the_client_reaches_a_project_with_the_headers_given_at_connect() {
        let served = Served::spawn("client").await;
        let client = served.connect();
        client.create_volume(Some("data")).await.unwrap();
        let volume = FileRoot::Volume("data".into());
        let home = served.project("demo").await;

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
    }

    #[tokio::test]
    async fn a_project_keeps_its_volumes() {
        let served = Served::spawn("project").await;
        let client = served.connect();
        let demo = served.project("demo").await;
        client.create_volume(Some("data")).await.unwrap();
        assert!(demo.join("volume/data/files").is_dir());
        assert_eq!(status(client.create_volume(Some("data")).await), 409);

        // A port that is none, and a sandbox of an image nobody filed, are refused before it
        // boots.
        let args = SandboxCreateArgs::new("a:b").port(0);
        assert_eq!(status(client.create_sandbox(&args).await), 400);
        // So are a port asked for over HTTP, which a port's tunnel here does not serve, and one
        // with inbound rules, which this server keeps none of.
        let web = PortSpec::new(5173).protocol(PortProtocol::Http);
        let args = SandboxCreateArgs::new("a:b").port(web);
        let refused = client.create_sandbox(&args).await.unwrap_err();
        assert_eq!(refused.status(), Some(400), "{refused}");
        assert!(refused.to_string().contains("HTTP"), "{refused}");
        let ssh = PortSpec::new(22).inbound([InboundRule::new("10.0.0.0/8")]);
        let args = SandboxCreateArgs::new("a:b").port(ssh);
        assert_eq!(status(client.create_sandbox(&args).await), 400);
        let args = SandboxCreateArgs::new("a:b");
        assert_eq!(status(client.create_sandbox(&args).await), 404);
    }

    /// What a server before this one kept of a project's sandboxes is known again as the project
    /// is first reached: each sandbox kept, parked or exited as it was, behind the port URLs it
    /// was made with and answering its ports without them, with its jobs, one that ran then
    /// ended `restarted`; what no sandbox keeps is deleted. No machine boots for it.
    #[tokio::test]
    async fn a_project_s_sandboxes_are_kept_across_a_restart() {
        let served = Served::spawn("kept").await;
        served.file_image("kept:1").await;
        let demo = served.project("demo").await;
        let images = ImageStore::new(demo.join("images")).unwrap();
        let digest = images.get_tag(&"kept:1".parse().unwrap()).unwrap();
        let digest = digest.unwrap().to_string();
        let dir = demo.join("sandbox");
        std::fs::create_dir_all(&dir).unwrap();
        let capability = "c".repeat(64);
        let jobs = json!([
            { "id": "ran", "kind": "exec", "pid": 7, "cmd": ["sleep", "60"], "running": true },
            { "id": "done", "kind": "exec", "pid": 8, "cmd": ["true"], "running": false, "code": 0 },
        ]);
        let web = json!({ "port": 8080, "label": "web" });
        let kept = [
            (
                "kept",
                json!({ capability.clone(): 8080 }),
                false,
                jobs,
                web.clone(),
            ),
            ("ended", json!({}), true, json!([]), json!(8080)),
        ];
        for (id, keys, exited, jobs, port) in kept {
            std::fs::write(dir.join(format!("{id}.ext4")), b"").unwrap();
            let image = json!({ "type": "digest", "digest": digest });
            let args = json!({ "image": image, "ports": [port] });
            let record = json!({ "args": args, "keys": keys, "exited": exited, "jobs": jobs });
            std::fs::write(dir.join(format!("{id}.json")), record.to_string()).unwrap();
        }
        // What a making cut short before its record leaves, and a record whose disk is gone.
        std::fs::write(dir.join("cut.ext4"), b"").unwrap();
        std::fs::write(dir.join("gone.json"), b"{}").unwrap();

        let (status, listed) = served.send("GET", "sandboxes/", Some("demo"), "").await;
        assert_eq!(status, StatusCode::OK);
        let mut listed: Vec<Value> = serde_json::from_str(&listed).unwrap();
        listed.sort_by_key(|sandbox| sandbox["id"].as_str().unwrap().to_owned());
        assert_eq!(
            listed,
            [
                json!({ "id": "ended", "image": digest, "running": false, "ports": [{ "port": 8080 }] }),
                json!({ "id": "kept", "image": digest, "running": true, "ports": [web] }),
            ]
        );
        let (status, got) = served.send("GET", "sandboxes/kept", Some("demo"), "").await;
        assert_eq!(status, StatusCode::OK);
        let got: Value = serde_json::from_str(&got).unwrap();
        assert_eq!(got, listed[1]);
        let mut left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        let kept_files =
            |id: &str| [".ext4", ".ext4.lock", ".json"].map(|end| format!("{id}{end}"));
        assert_eq!(left, [kept_files("ended"), kept_files("kept")].concat());

        let (status, jobs) = served
            .send("GET", "sandboxes/kept/jobs/", Some("demo"), "")
            .await;
        assert_eq!(status, StatusCode::OK);
        let jobs: Value = serde_json::from_str(&jobs).unwrap();
        assert_eq!(
            jobs,
            json!([
                { "id": "done", "kind": "exec", "pid": 8, "cmd": ["true"], "running": false, "code": 0 },
                {
                    "id": "ran", "kind": "exec", "pid": 7, "cmd": ["sleep", "60"],
                    "running": false, "code": 137, "reason": "restarted",
                },
            ])
        );
        let (status, log) = served
            .send("GET", "sandboxes/kept/jobs/ran/events", Some("demo"), "")
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            log,
            "id: 1\ndata: {\"end\":{\"code\":137,\"reason\":\"restarted\"}}\n\n"
        );

        // Its port's URL reaches it as it did: parked, no machine runs to reach the port in,
        // which the close says.
        let port = served.port_url(&format!("ports/{capability}"));
        let mut socket = Served::open(&port, Some("virtx.tcp")).await.unwrap();
        let Some(Ok(tungstenite::Message::Close(Some(frame)))) = socket.next().await else {
            panic!("the WebSocket was not closed with a reason");
        };
        assert!(frame.reason.contains("no machine runs"), "{}", frame.reason);

        let (status, _) = served
            .send("DELETE", "sandboxes/kept", Some("demo"), "")
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(
            kept_files("kept")
                .iter()
                .all(|file| !dir.join(file).exists())
        );
        let (status, _) = served.send("GET", "sandboxes/kept", Some("demo"), "").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            Served::open(&port, Some("virtx.tcp")).await.err(),
            Some(404)
        );
    }

    /// A sandbox a server before this one kept that this one cannot know again -- its record
    /// not read, or its image gone -- is listed, not running, with why in `error`: a record
    /// that does not read is told by the failure's kind, not by its path. Every other request
    /// about it is refused `409` `ended`, saying why the same way, and it keeps its project from
    /// being removed until its `DELETE` removes it, its disk and record with it.
    #[tokio::test]
    async fn a_sandbox_not_known_again_is_listed_with_why_until_its_delete() {
        let served = Served::spawn("unknown").await;
        let demo = served.project("demo").await;
        let dir = demo.join("sandbox");
        std::fs::create_dir_all(&dir).unwrap();
        let gone = format!("sha256:{}", "0".repeat(64));
        let image = json!({ "type": "digest", "digest": gone });
        let record = json!({ "args": { "image": image } }).to_string();
        for (id, record) in [("unread", "not json".to_owned()), ("lost", record)] {
            std::fs::write(dir.join(format!("{id}.ext4")), b"").unwrap();
            std::fs::write(dir.join(format!("{id}.json")), record).unwrap();
        }

        let (status, listed) = served.send("GET", "sandboxes/", Some("demo"), "").await;
        assert_eq!(status, StatusCode::OK);
        let mut listed: Vec<Value> = serde_json::from_str(&listed).unwrap();
        listed.sort_by_key(|sandbox| sandbox["id"].as_str().unwrap().to_owned());
        let errors: Vec<_> = listed
            .iter_mut()
            .map(|sandbox| sandbox.as_object_mut().unwrap().remove("error"))
            .collect();
        assert_eq!(
            listed,
            [
                json!({ "id": "lost", "image": gone, "running": false }),
                json!({ "id": "unread", "image": "", "running": false }),
            ]
        );
        let said = |why: &str| {
            Some(json!(format!(
                "not known again since its server restarted: {why}"
            )))
        };
        let unread = "the server's files: invalid data";
        assert_eq!(
            errors,
            [said(&format!("no image here is {gone}")), said(unread)]
        );
        let (status, got) = served
            .send("GET", "sandboxes/unread", Some("demo"), "")
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&got).unwrap()["running"],
            false
        );

        let exec = r#"{"cmd": ["true"]}"#;
        let used = [
            ("POST", "sandboxes/unread/jobs/exec/", exec),
            ("GET", "sandboxes/unread/jobs/", ""),
            ("GET", "sandboxes/unread/files/", ""),
            ("PATCH", "sandboxes/unread", r#"{"vcpus": 2}"#),
        ];
        for (method, path, body) in used {
            let (status, refused) = served.send(method, path, Some("demo"), body).await;
            assert_eq!(status, StatusCode::CONFLICT, "{method} {path}");
            let refused: Value = serde_json::from_str(&refused).unwrap();
            assert_eq!(refused["error"]["code"], "ended", "{method} {path}");
            let message = refused["error"]["message"].as_str().unwrap();
            assert!(message.ends_with(&format!(": {unread}")), "{message}");
        }
        let (status, _) = served.send("DELETE", "projects/demo", None, "").await;
        assert_eq!(status, StatusCode::CONFLICT, "its sandboxes keep it");

        for id in ["unread", "lost"] {
            let path = format!("sandboxes/{id}");
            let (status, _) = served.send("DELETE", &path, Some("demo"), "").await;
            assert_eq!(status, StatusCode::NO_CONTENT, "{id}");
        }
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
        let (status, _) = served
            .send("GET", "sandboxes/unread", Some("demo"), "")
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let resize = r#"{"vcpus": 2}"#;
        let (status, _) = served
            .send("PATCH", "sandboxes/unread", Some("demo"), resize)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = served.send("DELETE", "projects/demo", None, "").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
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

    /// Two clients writing over one etag at once: the one whose body has all arrived first
    /// lands, and the other is refused the etag it read, gone stale under it.
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
                (Ok(()), Err(virtx_core::Error::Status(412, _, _))) => &from_a,
                (Err(virtx_core::Error::Status(412, _, _)), Ok(())) => &from_b,
                (by_a, by_b) => panic!("round {round}, over one etag: {by_a:?} and {by_b:?}"),
            };
            let read = a.read_file(&root, "read", 0, None).await.unwrap();
            assert!(read == *landed, "round {round}: not the write that landed");
        }
    }

    /// A storage backend that keeps a home's volumes in a store of its own beside the home, as
    /// a server beside this one would find them: each brought here as it is read or written, and
    /// kept there again once a write lets go of it. It refuses what `held` names as held
    /// elsewhere, says what it removed, and keeps nothing of a sandbox's disk but files the empty
    /// image here as it is wanted. A volume it is removing fails where a sandbox could mount it
    /// here meanwhile. A tag told to it is said in `tagged`, and fails while `down`; while
    /// `paused`, it is said once a permit of `resumed` is given, `tagging` set as it waits.
    struct Beyond {
        kept: VolumeStore,
        here: VolumeStore,
        images: ImageStore,
        held: Mutex<HashSet<String>>,
        removed: Mutex<Vec<String>>,
        tagged: Mutex<Vec<String>>,
        down: std::sync::atomic::AtomicBool,
        paused: std::sync::atomic::AtomicBool,
        tagging: std::sync::atomic::AtomicBool,
        resumed: tokio::sync::Semaphore,
    }

    impl Beyond {
        fn refuse(&self, name: &str) -> Result<(), Refusal> {
            match self.held.lock().unwrap().contains(name) {
                true => Err(Refusal::InUse(format!("{name} is held elsewhere"))),
                false => Ok(()),
            }
        }
    }

    /// Volume `name`'s files in `from` copied over those in `to`, made there if it is not.
    fn copy_volume(from: &VolumeStore, to: &VolumeStore, name: &str) {
        let files = to.open(name).or_else(|_| to.create(name)).unwrap().files();
        for file in std::fs::read_dir(from.open(name).unwrap().files()).unwrap() {
            let file = file.unwrap();
            std::fs::copy(file.path(), files.join(file.file_name())).unwrap();
        }
    }

    /// Kept beyond again as it drops: a volume's write let go of.
    struct KeptAgain(VolumeStore, VolumeStore, String);

    impl Drop for KeptAgain {
        fn drop(&mut self) {
            copy_volume(&self.0, &self.1, &self.2);
        }
    }

    impl StorageBackend for Beyond {
        fn made<'a>(&'a self, what: Stored<'a>, _: u64) -> BoxFuture<'a, Result<(), Refusal>> {
            Box::pin(async move {
                let Stored::Volume(name) = what else {
                    return Ok(());
                };
                if self.kept.open(name).is_ok() {
                    return Err(Refusal::Exists(format!("{name} is kept already")));
                }
                self.kept
                    .create(name)
                    .map_err(|e| Refusal::Failed(e.into()))?;
                Ok(())
            })
        }

        fn take<'a>(&'a self, what: Stored<'a>) -> BoxFuture<'a, Result<Held, Refusal>> {
            Box::pin(async move {
                let Stored::Volume(name) = what else {
                    return Ok(Box::new(()) as Held);
                };
                self.refuse(name)?;
                self.bring_up(what).await?;
                let (here, kept) = (self.here.clone(), self.kept.clone());
                Ok(Box::new(KeptAgain(here, kept, name.to_owned())) as Held)
            })
        }

        fn bring_up<'a>(&'a self, what: Stored<'a>) -> BoxFuture<'a, Result<(), Refusal>> {
            Box::pin(async move {
                let Stored::Volume(name) = what else {
                    return Ok(());
                };
                if self.kept.open(name).is_err() {
                    return Err(Refusal::Unknown(format!("no volume {name} is kept")));
                }
                copy_volume(&self.kept, &self.here, name);
                Ok(())
            })
        }

        fn remove<'a>(&'a self, what: Stored<'a>) -> BoxFuture<'a, Result<(), Refusal>> {
            Box::pin(async move {
                let (Stored::Volume(name) | Stored::Disk(name)) = what;
                self.refuse(name)?;
                if let Stored::Volume(name) = what {
                    if self
                        .here
                        .open(name)
                        .is_ok_and(|volume| volume.hold().is_ok())
                    {
                        let mounted = anyhow::anyhow!("{name} could be mounted as it was removed");
                        return Err(Refusal::Failed(mounted));
                    }
                    self.kept
                        .remove(name)
                        .map_err(|_| Refusal::Unknown(format!("no volume {name} is kept")))?;
                }
                self.removed.lock().unwrap().push(name.to_owned());
                Ok(())
            })
        }

        fn volumes(&self) -> BoxFuture<'_, anyhow::Result<Option<Vec<VolumeListOutputItem>>>> {
            Box::pin(async move {
                let mut listed: Vec<_> = (self.kept.list()?.into_iter())
                    .map(|name| VolumeListOutputItem {
                        size: self.kept.open(&name).unwrap().size(),
                        name,
                    })
                    .collect();
                listed.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(Some(listed))
            })
        }

        fn want<'a>(&'a self, wanted: Wanted<'a>) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                if let Wanted::Image(_) = wanted {
                    empty_image(&self.images);
                }
                Ok(())
            })
        }

        fn tagged<'a>(
            &'a self,
            tag: &'a Reference,
            image: &'a virtx_uvm::Digest,
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                if self.paused.load(std::sync::atomic::Ordering::SeqCst) {
                    self.tagging
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    self.resumed.acquire().await?.forget();
                }
                let down = self.down.load(std::sync::atomic::Ordering::SeqCst);
                anyhow::ensure!(!down, "the backend is down");
                self.tagged.lock().unwrap().push(format!("{tag} {image}"));
                Ok(())
            })
        }
    }

    /// File an empty image in `images`: the same digest in every store.
    fn empty_image(images: &ImageStore) -> virtx_uvm::Digest {
        let layer = images.insert_layer(&FsTree::new()).unwrap();
        let config = serde_json::from_str(
            r#"{"architecture": "arm64", "os": "linux", "rootfs": {"type": "layers", "diff_ids": []}}"#,
        )
        .unwrap();
        images.insert_image(&config, &[layer]).unwrap()
    }

    /// Each home in the project's directory, backed by a [`Beyond`] in its `beyond/`.
    #[derive(Default)]
    struct BackedHomes {
        store: OnceLock<ProjectStore>,
        made: Mutex<Vec<(Arc<Beyond>, PathBuf)>>,
    }

    impl Homes for BackedHomes {
        fn make<'a>(
            &'a self,
            id: &'a ProjectId,
        ) -> BoxFuture<'a, anyhow::Result<Option<crate::endpoints::Home>>> {
            Box::pin(async move {
                let store = self.store.get().unwrap();
                let home = Homes::make(store, id).await?.unwrap();
                let beyond = Arc::new(Beyond {
                    kept: VolumeStore::new(home.root.join("beyond"))?,
                    here: home.volumes().clone(),
                    images: home.images().clone(),
                    held: Mutex::default(),
                    removed: Mutex::default(),
                    tagged: Mutex::default(),
                    down: Default::default(),
                    paused: Default::default(),
                    tagging: Default::default(),
                    resumed: tokio::sync::Semaphore::new(0),
                });
                let root = home.root.clone();
                self.made.lock().unwrap().push((Arc::clone(&beyond), root));
                Ok(Some(home.backed_by(beyond)))
            })
        }
    }

    /// The status and the error's code `refused` is answered with.
    async fn refused_with(refused: crate::endpoints::Refused) -> (StatusCode, String) {
        let response = axum::response::IntoResponse::into_response(refused);
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await;
        let body: Value = serde_json::from_slice(&body.unwrap()).unwrap();
        (status, body["error"]["code"].as_str().unwrap().to_owned())
    }

    /// A home backed beyond this machine answers its volumes from there, each as one here is
    /// answered: made there, refused where one of its name is kept already; listed and read from
    /// there; brought here to be read, held there while it is written and kept there again
    /// after; removed there first. What the backend holds elsewhere is refused `409` `in_use`,
    /// and over WebDAV `423`, with nothing changed.
    #[tokio::test]
    async fn a_backed_home_s_volumes_are_kept_beyond_it() {
        let homes = Arc::new(BackedHomes::default());
        let made = Arc::clone(&homes);
        let served = Served::spawn_over("backed", async move |home| {
            let projects = TcpMembership::new(home);
            projects
                .create(&Principal::new("tester"), "demo")
                .await
                .unwrap();
            let store = ProjectStore::new(home);
            made.store.set(store.clone()).unwrap();
            let access = Access::new(
                Vec::new(),
                Arc::new(Tester),
                Arc::new(projects),
                store,
                made,
            );
            super::router(Arc::new(access), axum::Router::new())
        })
        .await;
        let client = served.connect();
        client.create_volume(Some("data")).await.unwrap();
        let (beyond, root) = homes.made.lock().unwrap()[0].clone();
        assert!(beyond.kept.open("data").is_ok(), "made there too");

        let away = beyond.kept.create("away").unwrap().files();
        std::fs::write(away.join("note"), "hi").unwrap();
        assert!(beyond.here.open("away").is_err());
        let (status, listed) = served.send("GET", "volumes/", Some("demo"), "").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&listed).unwrap(),
            json!([{ "name": "away", "size": 2 }, { "name": "data", "size": 0 }])
        );
        let got = client.get_volume("away").await.unwrap();
        assert_eq!((got.name.as_str(), got.size), ("away", 2));
        assert_eq!(self::status(client.get_volume("none").await), 404);

        // Read, it is brought here; written, kept there once the write is done.
        let (status, read) = served
            .send("GET", "volumes/away/files/note", Some("demo"), "")
            .await;
        assert_eq!((status, read.as_str()), (StatusCode::OK, "hi"));
        assert!(root.join("volume/away/files/note").is_file());
        let (status, _) = served
            .send("PUT", "volumes/away/files/note", Some("demo"), "there")
            .await;
        assert!(status.is_success(), "{status}");
        assert_eq!(std::fs::read_to_string(away.join("note")).unwrap(), "there");

        // Held elsewhere: a write refused `423`, a removal `409` `in_use`, and nothing changed.
        beyond.held.lock().unwrap().insert("away".into());
        let (status, _) = served
            .send("PUT", "volumes/away/files/note", Some("demo"), "no")
            .await;
        assert_eq!(status, StatusCode::LOCKED);
        let (status, refused) = served
            .send("DELETE", "volumes/away", Some("demo"), "")
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let refused: Value = serde_json::from_str(&refused).unwrap();
        assert_eq!(refused["error"]["code"], "in_use");
        assert_eq!(std::fs::read_to_string(away.join("note")).unwrap(), "there");
        assert!(beyond.here.open("away").is_ok());
        beyond.held.lock().unwrap().clear();

        // One of a name kept already is refused, and nothing is left of it here.
        beyond.kept.create("taken").unwrap();
        assert_eq!(self::status(client.create_volume(Some("taken")).await), 409);
        assert!(beyond.here.open("taken").is_err());

        client.remove_volume("away").await.unwrap();
        assert!(beyond.kept.open("away").is_err());
        assert!(beyond.here.open("away").is_err());
        assert_eq!(*beyond.removed.lock().unwrap(), ["away"]);
        assert_eq!(self::status(client.remove_volume("away").await), 404);
    }

    /// A build in a backed home keeps the tag it writes beyond this machine before it ends; one
    /// whose tag the backend cannot keep fails, and leaves the tag here as it was.
    #[tokio::test]
    async fn a_backed_home_s_build_keeps_its_tag_beyond_it() {
        let homes = Arc::new(BackedHomes::default());
        let made = Arc::clone(&homes);
        let served = Served::spawn_over("backed-build", async move |home| {
            let projects = TcpMembership::new(home);
            projects
                .create(&Principal::new("tester"), "demo")
                .await
                .unwrap();
            let store = ProjectStore::new(home);
            made.store.set(store.clone()).unwrap();
            let access = Access::new(
                Vec::new(),
                Arc::new(Tester),
                Arc::new(projects),
                store,
                made,
            );
            super::router(Arc::new(access), axum::Router::new())
        })
        .await;
        let client = served.connect();
        let ended = async |reference: &str| {
            let job = client
                .build_image(Recipe::new("scratch"), reference)
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(60), async {
                loop {
                    let job = client.get_image_job(&job.id).await.unwrap();
                    if !job.running {
                        break job;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the job went on")
        };

        let built = ended("built:1").await;
        let digest = built.digest.expect("the build did not fail");
        let beyond = Arc::clone(&homes.made.lock().unwrap()[0].0);
        assert_eq!(
            *beyond.tagged.lock().unwrap(),
            [format!("docker.io/library/built:1 {digest}")]
        );

        beyond.down.store(true, std::sync::atomic::Ordering::SeqCst);
        let refused = ended("built:2").await;
        assert!(refused.error.is_some(), "{refused:?}");
        let built_2: Reference = "built:2".parse().unwrap();
        assert_eq!(beyond.images.get_tag(&built_2).unwrap(), None);

        // Cancelled as its tag is being kept, it is answered once that is done: it ended first.
        beyond
            .down
            .store(false, std::sync::atomic::Ordering::SeqCst);
        beyond
            .paused
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let job = client
            .build_image(Recipe::new("scratch"), "built:3")
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while !beyond.tagging.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the build never tagged");
        let cancelled = async {
            client.remove_image_job(&job.id).await.unwrap();
            beyond.tagged.lock().unwrap().len()
        };
        let resumed = async {
            // Time for a cancel not ordered after the tag to be answered first.
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            beyond.resumed.add_permits(1);
        };
        let (kept_by_then, ()) = tokio::join!(cancelled, resumed);
        assert_eq!(kept_by_then, 2, "answered before the tag was kept");
        let built_3: Reference = "built:3".parse().unwrap();
        let tagged = beyond.images.get_tag(&built_3).unwrap();
        assert_eq!(tagged.map(|image| image.to_string()), Some(digest));
    }

    /// A backend that keeps nothing, whose `want` -- the first wait of a sandbox's making --
    /// says it was reached in `wanting` and, while `holding`, waits for `go`; and writes the
    /// record `writes` names, where it names one, as another making under its id would.
    struct Holding {
        holding: std::sync::atomic::AtomicBool,
        wanting: tokio::sync::Semaphore,
        go: tokio::sync::Semaphore,
        writes: Mutex<Option<PathBuf>>,
    }

    impl Default for Holding {
        fn default() -> Holding {
            Holding {
                holding: Default::default(),
                wanting: tokio::sync::Semaphore::new(0),
                go: tokio::sync::Semaphore::new(0),
                writes: Mutex::default(),
            }
        }
    }

    impl StorageBackend for Holding {
        fn want<'a>(&'a self, _: Wanted<'a>) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move {
                self.wanting.add_permits(1);
                if self.holding.load(std::sync::atomic::Ordering::SeqCst) {
                    self.go.acquire().await.unwrap().forget();
                }
                if let Some(record) = self.writes.lock().unwrap().take() {
                    std::fs::write(record, b"{}").unwrap();
                }
                Ok(())
            })
        }
    }

    /// A sandbox made under an id and keys given it is refused, before its machine boots, an id
    /// a sandbox here has or one made meanwhile takes, a key one here opens a port by or one
    /// being made was given, an id that cannot be one and keys that are not one for each port;
    /// a port over HTTP and with inbound rules is not refused for it. Nothing of it is made,
    /// and a key a making that failed was given is given back.
    #[tokio::test]
    async fn a_sandbox_made_under_an_id_and_keys_given_it_is_refused_only_what_it_cannot_be() {
        use crate::endpoints::{Given, Home};

        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let images = ImageStore::new(root.join("images")).unwrap();
        let digest = empty_image(&images);
        images
            .insert_tag(&"kept:1".parse().unwrap(), &digest)
            .unwrap();
        let dir = root.join("sandbox");
        std::fs::create_dir_all(&dir).unwrap();
        let capability = "c".repeat(64);
        let image = json!({ "type": "digest", "digest": digest.to_string() });
        let args = json!({ "image": image, "ports": [8080] });
        let record = json!({ "args": args, "keys": { capability.clone(): 8080 } });
        std::fs::write(dir.join("kept.ext4"), b"").unwrap();
        std::fs::write(dir.join("kept.json"), record.to_string()).unwrap();
        let registry =
            virtx_uvm::RegistryServer::new(images.clone(), crate::access::REGISTRY_PATH).unwrap();
        let backend = Arc::new(Holding::default());
        let home = Home::new(images, Arc::new(registry), root).unwrap();
        let home = home.backed_by(Arc::clone(&backend) as Arc<dyn StorageBackend>);
        assert!(home.sandboxes().get("kept").is_some());

        let web = PortSpec::new(8080)
            .protocol(PortProtocol::Http)
            .inbound([InboundRule::new("10.0.0.0/8")]);
        let made = async |image: &str, id: &str, keys: &[(&str, u16)]| {
            let args = SandboxCreateArgs::new(image).port(web.clone());
            let keys = keys.iter().map(|(key, port)| (key.to_string(), *port));
            let given = Given {
                id: id.to_owned(),
                keys: keys.collect(),
            };
            refused_with(home.make(args, given).await.err().unwrap()).await
        };
        let exists = (StatusCode::CONFLICT, "exists".to_owned());
        assert_eq!(made("kept:1", "kept", &[("k", 8080)]).await, exists);
        // Taken before its image is looked for.
        assert_eq!(made("none:1", "kept", &[("k", 8080)]).await, exists);
        assert_eq!(made("kept:1", "new", &[(&capability, 8080)]).await, exists);
        let invalid = (StatusCode::BAD_REQUEST, "invalid".to_owned());
        for id in ["", "../x", "a.b", &"a".repeat(65)] {
            assert_eq!(made("kept:1", id, &[("k", 8080)]).await, invalid, "{id:?}");
        }
        let unkeyed: [&[(&str, u16)]; 7] = [
            &[],
            &[("k", 9090)],
            &[("k", 8080), ("j", 8080)],
            &[("k", 8080), ("j", 9090)],
            &[("", 8080)],
            &[("k-1", 8080)],
            &[(&"k".repeat(129), 8080)],
        ];
        for keys in unkeyed {
            assert_eq!(made("kept:1", "new", keys).await, invalid, "{keys:?}");
        }
        // Past each check of its own, refused for what is not here alone.
        let missing = (StatusCode::NOT_FOUND, "not_found".to_owned());
        assert_eq!(made("none:1", "new", &[("k", 8080)]).await, missing);

        // A key one being made was given is taken until its making ends, and then given back.
        backend
            .holding
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let first = made("none:1", "first", &[("k", 8080)]);
        let second = async {
            backend.wanting.acquire().await.unwrap().forget();
            // The first held; this one, past `keying`, would not be.
            backend
                .holding
                .store(false, std::sync::atomic::Ordering::SeqCst);
            let second = made("none:1", "second", &[("k", 8080)]).await;
            backend.go.add_permits(1);
            second
        };
        let (first, second) = tokio::join!(first, second);
        assert_eq!((first, second), (missing.clone(), exists.clone()));
        assert_eq!(made("none:1", "second", &[("k", 8080)]).await, missing);

        // An id another making takes after it was looked at is refused as its disk is made.
        *backend.writes.lock().unwrap() = Some(dir.join("raced.json"));
        assert_eq!(made("kept:1", "raced", &[("k", 8080)]).await, exists);
        // What the other making holds, as it would.
        for left in ["raced.json", "raced.ext4.lock"] {
            std::fs::remove_file(dir.join(left)).unwrap();
        }

        let mut left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| !name.starts_with("kept."))
            .collect();
        left.sort();
        assert_eq!(left, Vec::<String>::new());
        assert_eq!(home.sandboxes().list().len(), 1);
    }

    /// A sandbox whose record and disk are put in a home is taken in, its image wanted from the
    /// home's backend: parked, behind its port URLs, and taken in once. Let go of, no request
    /// reaches it, and its record and disk stay for it to be taken in again; in use, it is not
    /// let go of. Deleted, its disk is removed beyond this machine first, and where that is
    /// refused it stays.
    #[tokio::test]
    async fn a_sandbox_is_taken_in_and_let_go_of() {
        let homes = Arc::new(BackedHomes::default());
        let made = Arc::clone(&homes);
        let access = Arc::new(OnceLock::new());
        let kept = Arc::clone(&access);
        let served = Served::spawn_over("moved", async move |home| {
            let projects = TcpMembership::new(home);
            projects
                .create(&Principal::new("tester"), "demo")
                .await
                .unwrap();
            let store = ProjectStore::new(home);
            made.store.set(store.clone()).unwrap();
            let made = Access::new(
                Vec::new(),
                Arc::new(Tester),
                Arc::new(projects),
                store,
                made,
            );
            let made = Arc::new(made);
            let _ = kept.set(Arc::clone(&made));
            super::router(made, axum::Router::new())
        })
        .await;
        let (status, _) = served.send("GET", "sandboxes/", Some("demo"), "").await;
        assert_eq!(status, StatusCode::OK);
        let home = access.get().unwrap().homes().pop().unwrap();
        let beyond = Arc::clone(&homes.made.lock().unwrap()[0].0);

        // Its image is not here: the backend files it as it is wanted.
        let elsewhere = tempfile::tempdir().unwrap();
        let digest = empty_image(&ImageStore::new(elsewhere.path()).unwrap()).to_string();
        let dir = home.root.join("sandbox");
        std::fs::create_dir_all(&dir).unwrap();
        let capability = "c".repeat(64);
        let image = json!({ "type": "digest", "digest": digest });
        let record = json!({ "args": { "image": image, "ports": [8080] }, "keys": { capability.clone(): 8080 } });
        std::fs::write(dir.join("moved.ext4"), b"").unwrap();
        std::fs::write(dir.join("moved.json"), record.to_string()).unwrap();
        let (status, _) = served
            .send("GET", "sandboxes/moved", Some("demo"), "")
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "not taken in yet");

        let taken = home.take_in("moved").await.ok().unwrap();
        assert_eq!(
            (taken.id.as_str(), taken.image.as_str()),
            ("moved", digest.as_str())
        );
        assert!(taken.running);
        let (status, got) = served
            .send("GET", "sandboxes/moved", Some("demo"), "")
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            serde_json::from_str::<Value>(&got).unwrap()["image"],
            digest
        );
        let port = served.port_url(&format!("ports/{capability}"));
        assert!(Served::open(&port, Some("virtx.tcp")).await.is_ok());
        let again = home.take_in("moved").await.err().unwrap();
        assert_eq!(
            refused_with(again).await,
            (StatusCode::CONFLICT, "exists".into())
        );
        let none = home.take_in("none").await.err().unwrap();
        assert_eq!(refused_with(none).await.0, StatusCode::NOT_FOUND);
        // One whose record does not read is refused as making it is, by the failure's kind.
        let (disk, unread) = (dir.join("unread.ext4"), dir.join("unread.json"));
        std::fs::write(&disk, b"").unwrap();
        std::fs::write(&unread, b"not a record").unwrap();
        let refused = home.take_in("unread").await.err().unwrap();
        let refused = axum::response::IntoResponse::into_response(refused);
        assert_eq!(refused.status().as_u16(), 500);
        let body = axum::body::to_bytes(refused.into_body(), usize::MAX).await;
        let body: Value = serde_json::from_slice(&body.unwrap()).unwrap();
        assert_eq!(body["error"]["message"], "the server's files: invalid data");
        std::fs::remove_file(disk).unwrap();
        std::fs::remove_file(unread).unwrap();

        // In use, it stays; let go of, it is reached no more, and its record and disk stay.
        let tunnel = home.tunnel(&capability).unwrap();
        let in_use = home.let_go("moved").await.err().unwrap();
        assert_eq!(
            refused_with(in_use).await,
            (StatusCode::CONFLICT, "in_use".into())
        );
        drop(tunnel);
        home.let_go("moved").await.ok().unwrap();
        let (status, _) = served
            .send("GET", "sandboxes/moved", Some("demo"), "")
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(
            Served::open(&port, Some("virtx.tcp")).await.err(),
            Some(404)
        );
        assert!(dir.join("moved.ext4").is_file() && dir.join("moved.json").is_file());
        let gone = home.let_go("moved").await.err().unwrap();
        assert_eq!(refused_with(gone).await.0, StatusCode::NOT_FOUND);
        home.take_in("moved").await.ok().unwrap();

        // Held beyond this machine, its delete is refused and it stays; let, it goes there first.
        beyond.held.lock().unwrap().insert("moved".into());
        let (status, refused) = served
            .send("DELETE", "sandboxes/moved", Some("demo"), "")
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let refused: Value = serde_json::from_str(&refused).unwrap();
        assert_eq!(refused["error"]["code"], "in_use");
        assert!(dir.join("moved.ext4").is_file());
        beyond.held.lock().unwrap().clear();
        let (status, _) = served
            .send("DELETE", "sandboxes/moved", Some("demo"), "")
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(*beyond.removed.lock().unwrap(), ["moved"]);
        assert!(!dir.join("moved.ext4").exists() && !dir.join("moved.json").exists());
    }

    /// A home's sandboxes, as a server built on this one reads them for routes of its own: each
    /// found by its id, and counted with how many have their machine up, none while each is
    /// parked; one not known again is none of them, and counted apart.
    #[tokio::test]
    async fn a_home_s_sandboxes_are_found_and_counted() {
        let access = Arc::new(OnceLock::new());
        let kept = Arc::clone(&access);
        let served = Served::spawn_over("counted", async move |home| {
            let projects = TcpMembership::new(home);
            let tester = Principal::new("tester");
            projects.create(&tester, "demo").await.unwrap();
            let store = ProjectStore::new(home);
            let made = Access::new(
                Vec::new(),
                Arc::new(Tester),
                Arc::new(projects),
                store.clone(),
                Arc::new(store),
            );
            let made = Arc::new(made);
            let _ = kept.set(Arc::clone(&made));
            super::router(made, axum::Router::new())
        })
        .await;
        served.file_image("kept:1").await;
        let demo = served.project("demo").await;
        let images = ImageStore::new(demo.join("images")).unwrap();
        let digest = images.get_tag(&"kept:1".parse().unwrap()).unwrap();
        let digest = digest.unwrap().to_string();
        let dir = demo.join("sandbox");
        std::fs::create_dir_all(&dir).unwrap();
        let gone = format!("sha256:{}", "0".repeat(64));
        for (id, image) in [("a", &digest), ("b", &digest), ("lost", &gone)] {
            let image = json!({ "type": "digest", "digest": image });
            std::fs::write(dir.join(format!("{id}.ext4")), b"").unwrap();
            let record = json!({ "args": { "image": image } });
            std::fs::write(dir.join(format!("{id}.json")), record.to_string()).unwrap();
        }
        let (status, _) = served.send("GET", "sandboxes/", Some("demo"), "").await;
        assert_eq!(status, StatusCode::OK);
        let home = access.get().unwrap().homes().pop().unwrap();

        let sandboxes = home.sandboxes();
        let counts = |known, up, unknown| crate::endpoints::Counts { known, up, unknown };
        assert_eq!(sandboxes.counts(), counts(2, 0, 1));
        let mut listed: Vec<_> = sandboxes
            .list()
            .iter()
            .map(|sandbox| sandbox.id().to_owned())
            .collect();
        listed.sort();
        assert_eq!(listed, ["a", "b"]);
        assert_eq!(sandboxes.get("a").unwrap().id(), "a");
        assert!(sandboxes.get("lost").is_none(), "not known again");
        assert!(sandboxes.get("none").is_none());
        assert!(sandboxes.running_on("a").is_none(), "parked");

        let (status, _) = served.send("DELETE", "sandboxes/b", Some("demo"), "").await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(home.sandboxes().counts(), counts(1, 0, 1));
        assert!(home.sandboxes().get("b").is_none());
    }
}
