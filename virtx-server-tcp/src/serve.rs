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
    use tokio::io::AsyncReadExt as _;
    use tokio_tungstenite::tungstenite;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use virtx_core::{Auth, Client, DirEntry, FileRoot, ImageJob, Recipe, SandboxCreateArgs};
    use virtx_uvm::{FsTree, ImageStore, PublicRegistries, RegistryProxy};

    use std::sync::Arc;

    use futures_util::future::BoxFuture;

    use virtx_uvm::ProjectStore;

    use crate::access::{Access, Identity, Principal, TokenIsUser};
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
        /// `https://allowed.example`.
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
                super::router(Arc::new(access), axum::Router::new())
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
        let served = Served::spawn("unmerged").await;
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

    /// A refusal made before a handler runs is in PROTOCOL.md's envelope too, where a client
    /// reads its code: a body over the server's limit is `413` `too_large`, naming the limit,
    /// and a path whose segment does not read as one is `400` `invalid`.
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
                (Ok(()), Err(virtx_core::Error::Status(412, _))) => &from_a,
                (Err(virtx_core::Error::Status(412, _)), Ok(())) => &from_b,
                (by_a, by_b) => panic!("round {round}, over one etag: {by_a:?} and {by_b:?}"),
            };
            let read = a.read_file(&root, "read", 0, None).await.unwrap();
            assert!(read == *landed, "round {round}: not the write that landed");
        }
    }
}
