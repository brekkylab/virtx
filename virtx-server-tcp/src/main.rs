//! `virtx-server-tcp` -- a virtx server over HTTP, to the users and projects its config names
//! (see [`config`]): each request carries a user's token and names a project, and the endpoints
//! `virtx-core`'s `PROTOCOL.md` names answer it from that project's home; see [`serve`] for the
//! rest.
//!
//! `virtx-server-tcp token` makes a token, and says what the config keeps of it.
//! `virtx-server-tcp vm …` is this binary spawned by itself to be a machine; see
//! [`virtx_uvm::Machine`].

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use virtx_server_tcp::access::Access;
use virtx_server_tcp::config::{self, Config};
use virtx_server_tcp::{endpoints, serve};

#[derive(Parser)]
enum Command {
    /// Serve the endpoints, and sandboxes' ports beside them, as `--config` says.
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    /// Make a token for a user, and its `token_sha256` for the config.
    Token,
}

fn main() -> anyhow::Result<()> {
    // Before any runtime: this process may be about to become a machine and never come back.
    serve::become_machine_if_spawned();
    match Command::parse() {
        Command::Serve { config } => run(Config::read(&config)?),
        Command::Token => {
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            println!("token:        {token}");
            println!("token_sha256: {}", config::hashed(&token));
            Ok(())
        }
    }
}

#[tokio::main]
async fn run(config: Config) -> anyhow::Result<()> {
    if let Some(secs) = config.minute_secs {
        endpoints::set_minute(std::time::Duration::from_secs(secs.max(1)));
    }
    if let Some(mirror) = config
        .registry_mirror
        .as_deref()
        .filter(|m| !m.trim().is_empty())
    {
        endpoints::set_registry_mirror(mirror);
    }
    let access = Arc::new(Access::new(&config)?);
    let router = serve::router(Arc::clone(&access), &config.origins, axum::Router::new());
    serve::spawn_loops(&access);
    let served = serve::run(config.listen, router).await;
    // Every sandbox stops with this server, what it wrote on its disk, kept for the next.
    tokio::task::spawn_blocking(move || {
        for home in access.homes() {
            home.close_all_blocking();
        }
    })
    .await?;
    served
}

#[cfg(test)]
mod tests {
    //! The app, served on the loopback over a config of its own and called over HTTP as a
    //! client would: each request to an endpoint with a token and a project -- by hand, and
    //! through `virtx-core`'s client given both at connect.

    use std::path::PathBuf;
    use std::sync::Arc;

    use reqwest::{Method, StatusCode, Url};
    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};
    use tokio_tungstenite::tungstenite;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
    use virtx_core::{Auth, Client, DirEntry, FileRoot, SandboxCreateArgs};
    use virtx_server_tcp::access::Access;
    use virtx_server_tcp::config::Config;
    use virtx_server_tcp::serve;
    use virtx_uvm::{FsTree, ImageStore};

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
        async fn spawn(name: &str) -> Served {
            let home =
                std::env::temp_dir().join(format!("virtx-tcp-app-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(&home).unwrap();
            let config = json!({
                "listen": "127.0.0.1:0",
                "home": home,
                "origins": ["https://allowed.example"],
                "users": {
                    "alice": { "token_sha256": virtx_server_tcp::config::hashed("a-token") },
                    "bob": { "token_sha256": virtx_server_tcp::config::hashed("b-token") },
                },
                "projects": { "demo": { "members": ["alice"] } },
            });
            let path = home.join("config.json");
            std::fs::write(&path, config.to_string()).unwrap();
            let config = Config::read(&path).unwrap();

            let access = Arc::new(Access::new(&config).unwrap());
            let router = serve::router(access, &config.origins, axum::Router::new());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let task = tokio::spawn(async move { axum::serve(listener, router).await });
            Served { task, url, home }
        }

        /// `method` sent to `path` as `token` in `project`: the HTTP status, and the body.
        async fn send(
            &self,
            method: &str,
            path: &str,
            token: Option<&str>,
            project: Option<&str>,
            body: &'static str,
        ) -> (StatusCode, String) {
            let method = Method::from_bytes(method.as_bytes()).unwrap();
            let mut request = reqwest::Client::new()
                .request(method, format!("{}{path}", self.url))
                .body(body);
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            if let Some(project) = project {
                request = request.header("virtx-project", project);
            }
            let response = request.send().await.unwrap();
            let status = response.status();
            (status, response.text().await.unwrap())
        }

        /// A client of `token`'s, in `demo`.
        fn connect(&self, token: &str) -> Client {
            let mut headers = http::HeaderMap::new();
            headers.insert("virtx-project", "demo".parse().unwrap());
            let auth = Auth::Bearer {
                token: token.into(),
            };
            Client::connect(&self.url, auth, headers).unwrap()
        }

        /// File an empty image under `tag` in the store the server reads.
        fn file_image(&self, tag: &str) {
            let store = ImageStore::new(self.home.join("images")).unwrap();
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

    /// The HTTP status a request was refused with, whatever the body it came with.
    fn status<T: std::fmt::Debug>(answered: Result<T, virtx_core::Error>) -> u16 {
        match answered {
            Err(e) => e
                .status()
                .unwrap_or_else(|| panic!("not refused by the server: {e}")),
            Ok(answer) => panic!("expected a refusal, got {answer:?}"),
        }
    }

    fn names(entries: Vec<DirEntry>) -> Vec<String> {
        entries.into_iter().map(|entry| entry.name).collect()
    }

    #[tokio::test]
    async fn a_request_reaches_only_a_member_s_project() {
        let served = Served::spawn("access").await;
        let list = async |token, project| served.send("GET", "volumes/", token, project, "").await;

        let (status, body) = list(Some("a-token"), Some("demo")).await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "[]"));

        let (status, body) = list(None, Some("demo")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let refusal: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(refusal["error"]["code"], "unauthorized");
        assert_eq!(
            list(Some("x"), Some("demo")).await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(list(Some("a-token"), None).await.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            list(Some("b-token"), Some("demo")).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            list(Some("a-token"), Some("other")).await.0,
            StatusCode::FORBIDDEN
        );

        let from = async |origin| {
            reqwest::Client::new()
                .get(format!("{}volumes/", served.url))
                .bearer_auth("a-token")
                .header("virtx-project", "demo")
                .header("origin", origin)
                .send()
                .await
                .unwrap()
                .status()
        };
        assert_eq!(from("https://allowed.example").await, StatusCode::OK);
        assert_eq!(from("https://evil.example").await, StatusCode::FORBIDDEN);

        // A browser's preflight, sent without the token, is answered for the WebDAV methods from
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
        served
            .connect("a-token")
            .create_volume(Some("data"))
            .await
            .unwrap();
        let listed = reqwest::Client::new()
            .request(Method::from_bytes(b"PROPFIND").unwrap(), &files)
            .bearer_auth("a-token")
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

    /// The images are an OCI registry at `/images/oci/v2/`, to any user, naming no project.
    #[tokio::test]
    async fn the_images_are_a_registry_to_any_user() {
        let served = Served::spawn("registry").await;
        served.file_image("alpine:3.21");
        let base = format!("{}images/oci/v2", served.url);
        let client = reqwest::Client::new();

        let anonymous = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
        let root = client
            .get(format!("{base}/"))
            .bearer_auth("b-token")
            .send()
            .await
            .unwrap();
        assert_eq!(root.status(), StatusCode::OK);

        let manifest = client
            .get(format!("{base}/alpine/manifests/3.21"))
            .bearer_auth("b-token")
            .send()
            .await
            .unwrap();
        assert_eq!(manifest.status(), StatusCode::OK);
        let manifest: Value = manifest.json().await.unwrap();
        let layer = &manifest["layers"][0];
        assert_eq!(layer["mediaType"], "application/vnd.erofs.layer.v1");
        let blob = client
            .get(format!(
                "{base}/alpine/blobs/{}",
                layer["digest"].as_str().unwrap()
            ))
            .bearer_auth("b-token")
            .send()
            .await
            .unwrap();
        assert_eq!(blob.status(), StatusCode::OK);
        assert_eq!(
            format!("sha256:{:x}", Sha256::digest(blob.bytes().await.unwrap())),
            layer["digest"]
        );
    }

    /// A stand-in for an embedder's registry proxy, which refuses whatever it is asked, saying so.
    struct Refusing {
        url: String,
    }

    struct RefusingPass {
        url: String,
        refused: std::sync::Mutex<bool>,
    }

    impl virtx_uvm::RegistryProxy for Refusing {
        fn pass(&self) -> Box<dyn virtx_uvm::ProxyPass> {
            Box::new(RefusingPass {
                url: self.url.clone(),
                refused: std::sync::Mutex::new(false),
            })
        }
    }

    impl virtx_uvm::ProxyPass for RefusingPass {
        fn url(&self) -> &str {
            &self.url
        }

        fn refused(&self) -> Option<virtx_uvm::ProxyRefusal> {
            let mut refused = self.refused.lock().unwrap();
            (!std::mem::replace(&mut *refused, true)).then(|| virtx_uvm::ProxyRefusal {
                host: "the registry".into(),
                why: "it is not let through".into(),
            })
        }
    }

    /// A server given a registry proxy, as virtx cloud's nodes are: a pull goes through it --
    /// what it refuses fails saying so -- and connects to the registry itself not at all.
    #[tokio::test]
    async fn a_server_s_pulls_go_through_its_registry_proxy() {
        use futures_util::StreamExt as _;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        use virtx_core::ImageJobEntry;

        let served = Served::spawn("registry-proxy").await;
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", proxy.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = proxy.accept().await {
                let mut head = [0; 1024];
                let _ = stream.read(&mut head).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });
        virtx_server_tcp::endpoints::set_registry_proxy(Arc::new(Refusing { url }));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = served.connect("a-token");

        let reference = format!("127.0.0.1:{port}/team/app:1");
        let job = client.pull_image(&reference, None).await.unwrap();
        let mut log = client.image_job_log(&job.id, None);
        // A guard against a hang, not a measure of anything.
        let end = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                if let ImageJobEntry::End(end) = log.next().await.unwrap().unwrap().entry {
                    break end;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            end.error.as_deref(),
            Some(
                format!(
                    "{reference}: the registry is not reachable from this server: \
                     it is not let through"
                )
                .as_str()
            )
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "the registry was connected to"
        );
    }

    #[tokio::test]
    async fn the_files_endpoints_take_the_same_token_and_project() {
        let served = Served::spawn("files").await;
        served
            .connect("a-token")
            .create_volume(Some("data"))
            .await
            .unwrap();
        let send = async |method, path, token, body| {
            served.send(method, path, token, Some("demo"), body).await
        };

        let put = "volumes/data/files/a.txt";
        assert_eq!(
            send("PUT", put, None, "hello").await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            send("PUT", put, Some("b-token"), "hello").await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send("PUT", put, Some("a-token"), "hello").await.0,
            StatusCode::CREATED
        );
        assert_eq!(
            std::fs::read(served.home.join("project/demo/volume/data/files/a.txt")).unwrap(),
            b"hello"
        );
        let (status, body) = send("GET", put, Some("a-token"), "").await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "hello"));
        let (status, body) = send("PROPFIND", "volumes/data/files/", Some("a-token"), "").await;
        assert_eq!(status, StatusCode::MULTI_STATUS);
        assert!(body.contains("a.txt"), "{body}");
        assert_eq!(
            send("GET", "volumes/nobody/files/a.txt", Some("a-token"), "")
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }

    /// The files of a volume through `virtx-core`'s client, given the token and the
    /// project at connect, as a program over the bindings reaches them.
    #[tokio::test]
    async fn the_client_reaches_a_project_with_the_headers_given_at_connect() {
        let served = Served::spawn("client").await;
        let client = served.connect("a-token");
        client.create_volume(Some("data")).await.unwrap();
        let volume = FileRoot::Volume("data".into());
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

        // Not a member of the project: every request is refused, the resources and the files alike.
        let bob = served.connect("b-token");
        assert_eq!(status(bob.create_volume(None::<String>).await), 403);
        assert_eq!(status(bob.stat_file(&volume, "c.txt").await), 403);
    }

    #[tokio::test]
    async fn a_project_keeps_its_volumes() {
        let served = Served::spawn("project").await;
        let client = served.connect("a-token");
        client.create_volume(Some("data")).await.unwrap();
        assert!(served.home.join("project/demo/volume/data/files").is_dir());
        assert_eq!(status(client.create_volume(Some("data")).await), 409);

        // A port that is none, and a sandbox of an image nobody filed, are refused before it
        // boots.
        let mut args = SandboxCreateArgs::new("a:b");
        args.ports = vec![0.into()];
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
        let (a, b) = (served.connect("a-token"), served.connect("a-token"));
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
