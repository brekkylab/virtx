//! The built binary, served over a config of its own and called over HTTP as a client would:
//! each request to an endpoint with a token and a project -- by hand, and through
//! `virtx-core`'s client given both at connect.

use std::path::PathBuf;

use futures_util::{SinkExt as _, StreamExt as _, TryStreamExt as _};
use reqwest::{Method, StatusCode, Url};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::{self, Message};
use virtx_core::{
    Auth, Client, DirEntry, FileRoot, PortProtocol, Recipe, SandboxCreateArgs, SandboxExecArgs,
    SandboxJobEntry, SandboxPortOpenArgs, SandboxState, Step,
};
use virtx_uvm::{FsTree, ImageStore, RegistryClient};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A server over an empty home, killed and removed when this drops.
struct Served {
    process: Child,
    /// Its base URL, ending in `/`.
    url: String,
    home: PathBuf,
}

/// Where a test pulls `reference` from: Docker Hub's official images from Google's mirror of
/// them, as Docker Hub refuses anonymous pulls past a few an hour, and AWS's mirror past one a
/// second -- a test failing on that says nothing of what it tries. Anything else from where it
/// names.
fn mirrored(reference: &str) -> String {
    let name = reference.strip_prefix("docker.io/").unwrap_or(reference);
    let name = name.strip_prefix("library/").unwrap_or(name);
    if name.contains('/') {
        return reference.to_owned();
    }
    format!("mirror.gcr.io/library/{name}")
}

impl Served {
    async fn spawn(name: &str) -> Served {
        Served::spawn_with(name, json!({})).await
    }

    /// As [`Served::spawn`], its config with `more` in it besides.
    async fn spawn_with(name: &str, more: Value) -> Served {
        let home = std::env::temp_dir().join(format!("virtx-http-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let mut config = json!({
            "listen": "127.0.0.1:0",
            "home": home,
            "origins": ["https://allowed.example"],
            "users": {
                "alice": { "token_sha256": sha256("a-token") },
                "bob": { "token_sha256": sha256("b-token") },
            },
            "projects": { "demo": { "members": ["alice"] } },
        });
        for (key, value) in more.as_object().unwrap() {
            config[key] = value.clone();
        }
        std::fs::write(home.join("config.json"), config.to_string()).unwrap();
        Served::start(home).await
    }

    /// A server over `home`, as its `config.json` says.
    async fn start(home: PathBuf) -> Served {
        let path = home.join("config.json");
        let mut process = Command::new(env!("CARGO_BIN_EXE_virtx-server-tcp"))
            .arg("serve")
            .arg("--config")
            .arg(&path)
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        // Where it listens is the port it was given, which it says first.
        let mut said = BufReader::new(process.stderr.take().unwrap()).lines();
        let line = said
            .next_line()
            .await
            .unwrap()
            .expect("it says where it serves");
        let url = line
            .strip_prefix("serving at ")
            .unwrap_or_else(|| panic!("{line}"))
            .to_owned();
        // And what it says after, read for as long as it says anything: a stderr nobody reads
        // is one it can't write to. Said again here, where a failing test shows it.
        tokio::spawn(async move {
            while let Ok(Some(line)) = said.next_line().await {
                eprintln!("{line}");
            }
        });
        Served { process, url, home }
    }

    /// This server interrupted, as its operator stops it, and another started over its home.
    async fn again(mut self) -> Served {
        let pid = self.process.id().unwrap().to_string();
        let interrupted = Command::new("kill").args(["-INT", &pid]).status().await;
        assert!(interrupted.unwrap().success());
        self.process.wait().await.unwrap();
        // Not removed: the next is over it.
        let home = std::mem::take(&mut self.home);
        Served::start(home).await
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

    /// Pull `reference` from its registry into the store the server reads.
    async fn pull_image(&self, reference: &str) {
        let store = ImageStore::new(self.home.join("images")).unwrap();
        let named = reference.parse().unwrap();
        let reference = mirrored(reference).parse().unwrap();
        let registry = RegistryClient::new(store.clone(), None);
        let (_, _, manifest) = registry.pull_manifest(&reference).await.unwrap();
        let (_, config) = registry.pull_config(&reference, &manifest).await.unwrap();
        let mut layers = Vec::new();
        for descriptor in manifest.layers() {
            let tree = registry.pull_layer(&reference, descriptor).await.unwrap();
            layers.push(store.insert_layer(&tree).unwrap());
        }
        let image = store.insert_image(&config, &layers).unwrap();
        store.insert_tag(&named, &image).unwrap();
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
        let _ = self.process.start_kill();
        if !self.home.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }
}

fn sha256(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

/// The HTTP status a request was refused with.
fn status<T: std::fmt::Debug>(answered: Result<T, virtx_core::Error>) -> u16 {
    match answered {
        Err(virtx_core::Error::Status(status, _) | virtx_core::Error::Refusal { status, .. }) => {
            status
        }
        other => panic!("expected a status, got {other:?}"),
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

/// The files of a volume through `virtx-core`'s client, given the token and the project at
/// connect, as a program over the bindings reaches them.
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

/// Boots VMs: needs the network to pull alpine, the binary signed for the hypervisor, and the
/// guest kernel where libkrun finds it.
#[tokio::test]
#[ignore]
async fn a_port_s_websocket_reaches_what_listens_on_it_until_the_sandbox_goes() {
    let served = Served::spawn("port-reached").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("a-token");
    let mut args = SandboxCreateArgs::new("alpine:3.20");
    args.ports = vec![8000.into()];
    let created = client.create_sandbox(&args).await.unwrap();
    assert_eq!(created.ports[0].port, 8000);
    let url = served.port_url(&created.ports[0].url);
    assert_eq!(url.path().strip_prefix("/ports/").unwrap().len(), 64);
    // Answered only on its making: the URLs carry the capability.
    assert_eq!(client.list_sandbox().await.unwrap()[0].ports, []);

    let mut serve = SandboxExecArgs::new([
        "sh",
        "-c",
        "while true; do echo hello | nc -l -p 8000; done",
    ]);
    serve.timeout_ms = Some(30_000);
    client.exec_sandbox(&created.id, &serve).await.unwrap();
    let said = async {
        // Until something listens in it.
        loop {
            let mut socket = Served::open(&url, Some("virtx.tcp")).await.unwrap();
            socket.send(Message::binary(b"hi".to_vec())).await.unwrap();
            let mut said = Vec::new();
            while let Some(Ok(Message::Binary(bytes))) = socket.next().await {
                said.extend_from_slice(&bytes);
                if said.ends_with(b"hello\n") {
                    return said;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    };
    let said = tokio::time::timeout(std::time::Duration::from_secs(60), said)
        .await
        .expect("port 8000 says hello within a minute");
    assert_eq!(said, b"hello\n");

    client.remove_sandbox(&created.id).await.unwrap();
    assert_eq!(Served::open(&url, Some("virtx.tcp")).await.err(), Some(404));
}

/// Boots a VM, as the one above does. nginx listens on its socket twice -- once as it opens
/// it, and again to set its backlog -- which is to leave it where the first put it.
#[tokio::test]
#[ignore]
async fn a_port_s_websocket_reaches_a_program_that_listens_twice() {
    let served = Served::spawn("port-listened-twice").await;
    served.pull_image("nginx:1.27-alpine").await;
    let client = served.connect("a-token");
    let mut args = SandboxCreateArgs::new("nginx:1.27-alpine");
    args.ports = vec![80.into()];
    let created = client.create_sandbox(&args).await.unwrap();
    let url = served.port_url(&created.ports[0].url);

    let mut serve = SandboxExecArgs::new(["nginx", "-g", "daemon off;"]);
    serve.timeout_ms = Some(30_000);
    client.exec_sandbox(&created.id, &serve).await.unwrap();
    let answered = async {
        // Until it listens.
        loop {
            let mut socket = Served::open(&url, Some("virtx.tcp")).await.unwrap();
            socket
                .send(Message::binary(b"GET / HTTP/1.0\r\n\r\n".to_vec()))
                .await
                .unwrap();
            let mut said = Vec::new();
            while let Some(Ok(Message::Binary(bytes))) = socket.next().await {
                said.extend_from_slice(&bytes);
            }
            if !said.is_empty() {
                return String::from_utf8_lossy(&said).into_owned();
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    };
    let answer = tokio::time::timeout(std::time::Duration::from_secs(20), answered)
        .await
        .expect("nginx answered on its port");
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.contains("Welcome to nginx!"), "{answer}");
    client.remove_sandbox(&created.id).await.unwrap();
}

/// Boots a VM, as the one above does. A program listening before its port is opened listened in
/// the guest alone -- the host was not asked to take it -- and is reached all the same, through
/// the agent, without listening again.
#[tokio::test]
#[ignore]
async fn a_port_reaches_a_program_that_listened_before_it_was_opened() {
    let served = Served::spawn("port-listened-before").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("a-token");
    let created = client
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .await
        .unwrap();
    // One connection, and then it would listen again: the first must come through as it is.
    // It answers what it is told, so both ways are carried: to it, and back.
    let mut serve = SandboxExecArgs::new([
        "sh",
        "-c",
        "mkfifo /tmp/back; nc -l -p 8000 < /tmp/back | (read line; echo \"before $line\") > /tmp/back",
    ]);
    serve.timeout_ms = Some(30_000);
    client.exec_sandbox(&created.id, &serve).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let opened = client.open_sandbox_port(&created.id, 8000).await.unwrap();
    let url = served.port_url(&opened.url);
    let said = async {
        let mut socket = Served::open(&url, Some("virtx.tcp")).await.unwrap();
        socket
            .send(Message::binary(b"hi\n".to_vec()))
            .await
            .unwrap();
        let mut said = Vec::new();
        while let Some(Ok(Message::Binary(bytes))) = socket.next().await {
            said.extend_from_slice(&bytes);
            if said.ends_with(b"\n") {
                break;
            }
        }
        said
    };
    let said = tokio::time::timeout(std::time::Duration::from_secs(20), said)
        .await
        .expect("what listened before the port was opened answered");
    assert_eq!(said, b"before hi\n");
    client.remove_sandbox(&created.id).await.unwrap();
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_port_opened_after_the_sandbox_reaches_what_listens_on_it() {
    let served = Served::spawn("port-opened").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("a-token");
    let created = client
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .await
        .unwrap();
    assert_eq!(created.ports, []);

    let opened = client.open_sandbox_port(&created.id, 8000).await.unwrap();
    assert_eq!(opened.port, 8000);
    // Opened again: the same port, at the same URL.
    assert_eq!(
        client.open_sandbox_port(&created.id, 8000).await.unwrap(),
        opened
    );
    assert_eq!(status(client.open_sandbox_port(&created.id, 0).await), 400);
    assert_eq!(status(client.open_sandbox_port("nobody", 8000).await), 404);
    assert_eq!(
        (opened.label.as_deref(), opened.protocol),
        (None, PortProtocol::Tcp)
    );

    // One for a browser, named: behind a capability a host name's label holds.
    let web = |label: &str, protocol| SandboxPortOpenArgs {
        label: Some(label.into()),
        protocol,
    };
    let http = client
        .open_sandbox_port_with(
            &created.id,
            5173,
            &web("dev server", Some(PortProtocol::Http)),
        )
        .await
        .unwrap();
    assert_eq!(
        (http.label.as_deref(), http.protocol),
        (Some("dev server"), PortProtocol::Http)
    );
    let capability = http.url.strip_prefix("ports/").unwrap();
    assert_eq!(capability.len(), 32, "{capability}");
    // Named again: its label now; another protocol refused.
    let renamed = client
        .open_sandbox_port_with(&created.id, 5173, &web("vite", None))
        .await
        .unwrap();
    assert_eq!(
        (renamed.label.as_deref(), &renamed.url),
        (Some("vite"), &http.url)
    );
    assert_eq!(
        status(
            client
                .open_sandbox_port_with(&created.id, 5173, &web("vite", Some(PortProtocol::Tcp)))
                .await
        ),
        409
    );
    let long = "x".repeat(129);
    assert_eq!(
        status(
            client
                .open_sandbox_port_with(&created.id, 9000, &web(&long, None))
                .await
        ),
        400
    );
    // Kept with it: answered so as it starts again.
    let started = client.start_sandbox(&created.id).await.unwrap();
    assert!(started.ports.contains(&renamed), "{:?}", started.ports);

    // Closed: its URL reaches it no more, nor is it opened again; closed again, not found.
    client.close_sandbox_port(&created.id, 5173).await.unwrap();
    let mut gone = served.port_url(&renamed.url);
    gone.set_scheme("ws").unwrap();
    assert_eq!(
        Served::open(&gone, Some("virtx.tcp")).await.err(),
        Some(404)
    );
    let started = client.start_sandbox(&created.id).await.unwrap();
    assert!(
        started.ports.iter().all(|p| p.port != 5173),
        "{:?}",
        started.ports
    );
    assert_eq!(
        status(client.close_sandbox_port(&created.id, 5173).await),
        404
    );

    let url = served.port_url(&opened.url);
    let mut serve = SandboxExecArgs::new([
        "sh",
        "-c",
        "while true; do echo hello | nc -l -p 8000; done",
    ]);
    serve.timeout_ms = Some(30_000);
    client.exec_sandbox(&created.id, &serve).await.unwrap();
    let said = async {
        loop {
            let mut socket = Served::open(&url, Some("virtx.tcp")).await.unwrap();
            socket.send(Message::binary(b"hi".to_vec())).await.unwrap();
            let mut said = Vec::new();
            while let Some(Ok(Message::Binary(bytes))) = socket.next().await {
                said.extend_from_slice(&bytes);
                if said.ends_with(b"hello\n") {
                    return said;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    };
    let said = tokio::time::timeout(std::time::Duration::from_secs(60), said)
        .await
        .expect("port 8000 says hello within a minute");
    assert_eq!(said, b"hello\n");
    client.remove_sandbox(&created.id).await.unwrap();
}

/// Boots VMs, as the one above does. A built image keeps the recipe it was built from in its
/// config, under its label, and each step in its history, as `docker history` reads them; a
/// sandbox of it committed keeps the commit in its history, and no recipe.
#[tokio::test]
#[ignore]
async fn a_built_image_keeps_its_recipe_and_a_commit_of_it_does_not() {
    let served = Served::spawn("recipe").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("a-token");
    let recipe = Recipe::new("alpine:3.20")
        .step("echo built > /root/built")
        .step(Step::env("GREETING", "hello"));
    let job = client
        .build_image(recipe.clone(), "built:v1")
        .await
        .unwrap();
    let log: Vec<_> = client
        .image_job_log(&job.id, None)
        .try_collect()
        .await
        .unwrap();
    assert!(
        client
            .get_image_job(&job.id)
            .await
            .unwrap()
            .digest
            .is_some(),
        "{log:?}"
    );

    // Its config, as the registry answers it.
    let base = format!("{}images/oci/v2", served.url);
    let config_of = async |repository: &str, tag: &str| -> Value {
        let http = reqwest::Client::new();
        let manifest: Value = http
            .get(format!("{base}/{repository}/manifests/{tag}"))
            .bearer_auth("a-token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        http.get(format!(
            "{base}/{repository}/blobs/{}",
            manifest["config"]["digest"].as_str().unwrap()
        ))
        .bearer_auth("a-token")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
    };
    let told = |config: &Value| -> Vec<String> {
        config["history"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|h| h["comment"] == "virtx")
            .map(|h| {
                format!(
                    "{}{}",
                    h["created_by"].as_str().unwrap(),
                    if h["empty_layer"] == true {
                        " (empty)"
                    } else {
                        ""
                    }
                )
            })
            .collect()
    };
    let built = config_of("built", "v1").await;
    let kept = built["config"]["Labels"][virtx_uvm::RECIPE_LABEL]
        .as_str()
        .unwrap();
    assert_eq!(serde_json::from_str::<Recipe>(kept).unwrap(), recipe);
    assert_eq!(
        told(&built),
        ["RUN echo built > /root/built", "ENV GREETING=hello (empty)"]
    );

    // A sandbox of it committed: its recipe not kept, the commit told after its steps.
    let id = client
        .create_sandbox(&SandboxCreateArgs::new("built:v1"))
        .await
        .unwrap()
        .id;
    let job = client
        .exec_sandbox(
            &id,
            &SandboxExecArgs::new(["sh", "-c", "echo more > /root/more"]),
        )
        .await
        .unwrap();
    let _: Vec<_> = client
        .sandbox_job_log(&id, &job.id, None)
        .try_collect()
        .await
        .unwrap();
    let job = client.commit_image(&id, "committed:v1").await.unwrap();
    let _: Vec<_> = client
        .image_job_log(&job.id, None)
        .try_collect()
        .await
        .unwrap();
    let committed = config_of("committed", "v1").await;
    assert!(
        committed["config"]["Labels"]
            .get(virtx_uvm::RECIPE_LABEL)
            .is_none(),
        "{committed}"
    );
    assert_eq!(
        told(&committed),
        [
            "RUN echo built > /root/built".to_string(),
            "ENV GREETING=hello (empty)".to_string(),
            format!("virtx commit {id}"),
        ]
    );
    client.remove_sandbox(&id).await.unwrap();
}

/// Boots VMs, as the one above does. A stopped sandbox resized: its machine booted with the
/// vCPUs and memory it was given, from its next start on; a running one's refused.
#[tokio::test]
#[ignore]
async fn a_stopped_sandbox_is_resized_as_it_starts_again() {
    let served = Served::spawn("resize").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("a-token");
    let mut args = SandboxCreateArgs::new("alpine:3.20");
    args.vcpus = Some(2);
    let id = client.create_sandbox(&args).await.unwrap().id;
    let said = async |cmd: &'static str| -> String {
        let job = client
            .exec_sandbox(&id, &SandboxExecArgs::new(["sh", "-c", cmd]))
            .await
            .unwrap();
        let log: Vec<_> = client
            .sandbox_job_log(&id, &job.id, None)
            .try_collect()
            .await
            .unwrap();
        log.into_iter()
            .filter_map(|entry| match entry.entry {
                SandboxJobEntry::Stdout(line) => Some(line),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let resize = async |body: Value| -> (StatusCode, Value) {
        let r = reqwest::Client::new()
            .put(format!(
                "{}/sandboxes/{id}/resources",
                served.url.trim_end_matches('/')
            ))
            .bearer_auth("a-token")
            .header("virtx-project", "demo")
            .json(&body)
            .send()
            .await
            .unwrap();
        (r.status(), r.json().await.unwrap_or(Value::Null))
    };
    assert_eq!(said("nproc").await.trim(), "2");

    // Running: its machine is the size it booted as.
    let (status, refused) = resize(json!({ "vcpus": 3 })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"]["code"], "running");

    // Stopped: resized, and booted so.
    client.stop_sandbox(&id).await.unwrap();
    let (status, answer) = resize(json!({ "vcpus": 3, "memory_mib": 1024 })).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        (answer["vcpus"].as_u64(), answer["memory_mib"].as_u64()),
        (Some(3), Some(1024))
    );
    assert_eq!(said("nproc").await.trim(), "3");
    let kib: u64 = said("awk '/MemTotal/ {print $2}' /proc/meminfo")
        .await
        .trim()
        .parse()
        .unwrap();
    assert!((800 << 10..=1024 << 10).contains(&kib), "{kib} KiB");
    client.remove_sandbox(&id).await.unwrap();
}

/// Boots VMs, as the one above does. A sandbox's disk grown: its filesystem as large as it
/// once it starts again, what was on it kept; never shrunk.
#[tokio::test]
#[ignore]
async fn a_sandbox_s_disk_is_grown_as_it_starts_again() {
    let served = Served::spawn("disk").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("a-token");
    let mut args = SandboxCreateArgs::new("alpine:3.20");
    args.disk_gib = Some(1);
    let id = client.create_sandbox(&args).await.unwrap().id;
    let said = async |cmd: &'static str| -> String {
        let job = client
            .exec_sandbox(&id, &SandboxExecArgs::new(["sh", "-c", cmd]))
            .await
            .unwrap();
        let log: Vec<_> = client
            .sandbox_job_log(&id, &job.id, None)
            .try_collect()
            .await
            .unwrap();
        log.into_iter()
            .filter_map(|entry| match entry.entry {
                SandboxJobEntry::Stdout(line) => Some(line),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    // `df -k /`'s size, in GiB, rounded.
    let gib = async || -> u64 {
        let kib: u64 = said("df -k / | awk 'NR==2 {print $2}'")
            .await
            .trim()
            .parse()
            .unwrap();
        (kib + (1 << 19)) >> 20
    };
    assert_eq!(gib().await, 1);
    // Written over its image, and taken from it: both as they were once it is grown.
    said("echo kept > /root/kept && rm /etc/alpine-release && chmod 700 /root/kept").await;

    let grow = |n: u32| {
        reqwest::Client::new()
            .put(format!(
                "{}/sandboxes/{id}/disk",
                served.url.trim_end_matches('/')
            ))
            .bearer_auth("a-token")
            .header("virtx-project", "demo")
            .json(&json!({ "disk_gib": n }))
            .send()
    };
    // Running: grown once it starts again.
    let r = grow(3).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let answer: Value = r.json().await.unwrap();
    assert_eq!(answer, json!({ "disk_gib": 3, "running": true }));
    assert_eq!(gib().await, 1);
    client.stop_sandbox(&id).await.unwrap();
    client.start_sandbox(&id).await.unwrap();
    assert_eq!(gib().await, 3);
    assert_eq!(said("cat /root/kept").await, "kept");
    assert_eq!(said("stat -c %a /root/kept").await, "700");
    assert_eq!(
        said("test -e /etc/alpine-release && echo there || echo gone").await,
        "gone"
    );
    // Never shrunk.
    assert_eq!(grow(2).await.unwrap().status(), StatusCode::BAD_REQUEST);
    client.remove_sandbox(&id).await.unwrap();
}

/// Boots a VM, as the one above does. A running sandbox's files, read over WebDAV through its
/// agent: its image's and what it wrote alike, a directory listed, a file read whole and in
/// part. Nothing written; a stopped one's not read.
#[tokio::test]
#[ignore]
async fn a_running_sandbox_s_files_are_read_and_written() {
    let served = Served::spawn("files").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("a-token");
    let id = client
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .await
        .unwrap()
        .id;
    let job = client
        .exec_sandbox(
            &id,
            &SandboxExecArgs::new(["sh", "-c", "mkdir -p /root/d && echo hello > /root/d/a.txt"]),
        )
        .await
        .unwrap();
    let _: Vec<_> = client
        .sandbox_job_log(&id, &job.id, None)
        .try_collect()
        .await
        .unwrap();

    let asked = |method: &str, path: &str| {
        reqwest::Client::new()
            .request(
                Method::from_bytes(method.as_bytes()).unwrap(),
                format!(
                    "{}/sandboxes/{id}/files{path}",
                    served.url.trim_end_matches('/')
                ),
            )
            .bearer_auth("a-token")
            .header("virtx-project", "demo")
    };
    // Listed: what it wrote.
    let r = asked("PROPFIND", "/root/d/")
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let listed = r.text().await.unwrap();
    assert_eq!(status, 207, "{listed}");
    assert!(listed.contains("/root/d/a.txt"), "{listed}");
    assert!(listed.contains("<D:getcontentlength>6<"), "{listed}");
    // Read whole, and from its second byte on.
    let r = asked("GET", "/root/d/a.txt").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.text().await.unwrap(), "hello\n");
    let r = asked("GET", "/root/d/a.txt")
        .header("Range", "bytes=1-")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 206);
    assert_eq!(r.text().await.unwrap(), "ello\n");
    // Its image's, beneath.
    let r = asked("GET", "/etc/alpine-release").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.text().await.unwrap().starts_with("3.20"));
    let r = asked("GET", "/nothing/here").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    // Written, as a process there reads it.
    let r = asked("PUT", "/root/d/b.txt")
        .body("put\n")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let job = client
        .exec_sandbox(&id, &SandboxExecArgs::new(["cat", "/root/d/b.txt"]))
        .await
        .unwrap();
    let log: Vec<_> = client
        .sandbox_job_log(&id, &job.id, None)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(log[0].entry, SandboxJobEntry::Stdout("put".into()));

    // Stopped: neither read nor started for it.
    client.stop_sandbox(&id).await.unwrap();
    let r = asked("GET", "/root/d/a.txt").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
    let said: Value = r.json().await.unwrap();
    assert_eq!(said["error"]["code"], "stopped");
    assert_eq!(
        client.get_sandbox(&id).await.unwrap().state,
        SandboxState::Stopped
    );
    client.remove_sandbox(&id).await.unwrap();
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_cancelled_job_is_killed_and_the_sandbox_kept() {
    let served = Served::spawn("job-cancelled").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("a-token");
    let id = client
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .await
        .unwrap()
        .id;

    let job = client
        .exec_sandbox(&id, &SandboxExecArgs::new(["sleep", "600"]))
        .await
        .unwrap();
    assert!(job.running);
    client.remove_sandbox_job(&id, &job.id).await.unwrap();
    assert_eq!(status(client.get_sandbox_job(&id, &job.id).await), 404);
    assert_eq!(client.list_sandbox_job(&id).await.unwrap(), []);
    assert_eq!(
        client.get_sandbox(&id).await.unwrap().state,
        SandboxState::Running
    );
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_sandbox_stops_with_its_disk_kept_and_starts_again_as_it_was_left() {
    let served = Served::spawn("lifecycle").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("a-token");
    let mut args = SandboxCreateArgs::new("alpine:3.20");
    args.name = Some("work".into());
    args.ports = vec![8000.into()];
    let made = client.create_sandbox(&args).await.unwrap();
    assert_eq!(made.state, SandboxState::Running);
    assert_eq!(made.auto_stop_minutes, 15);
    assert_eq!(made.auto_delete_days, Some(30));
    assert!(made.image.starts_with("sha256:"));
    let id = made.id.clone();
    assert_eq!(client.find_sandbox("work").await.unwrap().unwrap().id, id);
    assert_eq!(status(client.create_sandbox(&args).await), 409);

    let wrote = |cmd: &'static str| SandboxExecArgs::new(["sh", "-c", cmd]);
    let ran = async |id: &str, cmd: &'static str| {
        let job = client.exec_sandbox(id, &wrote(cmd)).await.unwrap();
        let log: Vec<_> = client
            .sandbox_job_log(id, &job.id, None)
            .try_collect()
            .await
            .unwrap();
        log.into_iter()
            .filter_map(|entry| match entry.entry {
                SandboxJobEntry::Stdout(line) => Some(line),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    ran(&id, "echo kept > /root/note && sync").await;

    // Stopped: its machine down, its jobs ended and kept, its disk kept.
    let sleeping = client
        .exec_sandbox(&id, &SandboxExecArgs::new(["sleep", "600"]))
        .await
        .unwrap();
    let stopped = client.stop_sandbox(&id).await.unwrap();
    assert_eq!(stopped.state, SandboxState::Stopped);
    let ended = client.get_sandbox_job(&id, &sleeping.id).await.unwrap();
    assert!(!ended.running);
    assert_eq!(
        client.get_sandbox(&id).await.unwrap().state,
        SandboxState::Stopped
    );

    // Started again: its files as they were, its port at the same URL.
    let started = client.start_sandbox(&id).await.unwrap();
    assert_eq!(started.state, SandboxState::Running);
    assert_eq!(started.ports, made.ports);
    assert_eq!(ran(&id, "cat /root/note").await, ["kept"]);

    // A job started in a stopped one starts it.
    client.stop_sandbox(&id).await.unwrap();
    assert_eq!(ran(&id, "cat /root/note").await, ["kept"]);
    assert_eq!(
        client.get_sandbox(&id).await.unwrap().state,
        SandboxState::Running
    );

    // Committed to an image: another sandbox of it begins with what this one wrote.
    let job = client.commit_image(&id, "mine:v1").await.unwrap();
    let log: Vec<_> = client
        .image_job_log(&job.id, None)
        .try_collect()
        .await
        .unwrap();
    let digest = client.get_image_job(&job.id).await.unwrap().digest;
    assert!(digest.is_some(), "{log:?}");
    let other = client
        .create_sandbox(&SandboxCreateArgs::new("mine:v1"))
        .await
        .unwrap();
    assert_eq!(ran(&other.id, "cat /root/note").await, ["kept"]);

    // Deleted: gone, its disk with it.
    client.remove_sandbox(&id).await.unwrap();
    assert_eq!(status(client.get_sandbox(&id).await), 404);
    assert!(
        !served
            .home
            .join(format!("project/demo/snapshot/{id}.ext4"))
            .exists()
    );
    client.remove_sandbox(&other.id).await.unwrap();

    // Ephemeral: deleted when it stops.
    let mut args = SandboxCreateArgs::new("alpine:3.20");
    args.ephemeral = true;
    let gone = client.create_sandbox(&args).await.unwrap();
    client.stop_sandbox(&gone.id).await.unwrap();
    assert_eq!(status(client.get_sandbox(&gone.id).await), 404);
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_sandbox_is_kept_across_a_server_started_again() {
    let served = Served::spawn("kept").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("a-token");
    let mut args = SandboxCreateArgs::new("alpine:3.20");
    args.name = Some("kept".into());
    args.ports = vec![8000.into()];
    let made = client.create_sandbox(&args).await.unwrap();
    let job = client
        .exec_sandbox(
            &made.id,
            &SandboxExecArgs::new(["sh", "-c", "echo kept > /root/kept"]),
        )
        .await
        .unwrap();
    let _: Vec<_> = client
        .sandbox_job_log(&made.id, &job.id, None)
        .try_collect()
        .await
        .unwrap();
    let ephemeral = client
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20").ephemeral(true))
        .await
        .unwrap()
        .id;

    // Stopped with its server, and known stopped to the next, its job with it -- but an
    // ephemeral one, deleted.
    let served = served.again().await;
    let client = served.connect("a-token");
    let found = client.find_sandbox("kept").await.unwrap().unwrap();
    assert_eq!(
        (found.id.as_str(), found.state),
        (made.id.as_str(), SandboxState::Stopped)
    );
    assert_eq!(status(client.get_sandbox(&ephemeral).await), 404);

    // Started, as it was left, its port behind the capability it had.
    let started = client.start_sandbox(&made.id).await.unwrap();
    assert_eq!(started.ports, made.ports);
    let job = client
        .exec_sandbox(&made.id, &SandboxExecArgs::new(["cat", "/root/kept"]))
        .await
        .unwrap();
    let log: Vec<_> = client
        .sandbox_job_log(&made.id, &job.id, None)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(log[0].entry, SandboxJobEntry::Stdout("kept".into()));
    client.remove_sandbox(&made.id).await.unwrap();
}

/// Boots VMs, as the one above does. A terminal's bash writes each command to its history
/// as it runs -- on the sandbox's disk, before the shell exits, so a stop does not lose it.
#[tokio::test]
#[ignore]
async fn a_terminal_s_history_is_written_as_each_command_runs() {
    let served = Served::spawn("history").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("a-token");
    let id = client
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .await
        .unwrap()
        .id;
    let said = async |cmd: &'static str| -> String {
        let job = client
            .exec_sandbox(&id, &SandboxExecArgs::new(["sh", "-c", cmd]))
            .await
            .unwrap();
        let log: Vec<_> = client
            .sandbox_job_log(&id, &job.id, None)
            .try_collect()
            .await
            .unwrap();
        log.into_iter()
            .filter_map(|entry| match entry.entry {
                SandboxJobEntry::Stdout(line) => Some(line),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    said("apk add --no-cache bash >/dev/null && echo ok").await;

    let mut url = Url::parse(&served.url)
        .unwrap()
        .join(&format!("terminal/{id}?cols=100&rows=30"))
        .unwrap();
    url.set_scheme("ws").unwrap();
    let mut request = url.as_str().into_client_request().unwrap();
    let headers = request.headers_mut();
    headers.insert("sec-websocket-protocol", "virtx.terminal".parse().unwrap());
    headers.insert("authorization", "Bearer a-token".parse().unwrap());
    headers.insert("virtx-project", "demo".parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    let mut shown = Vec::new();
    let mut read_until = async |socket: &mut Socket, until: &str| {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while !String::from_utf8_lossy(&shown).contains(until) {
                match socket.next().await {
                    Some(Ok(Message::Binary(bytes))) => shown.extend_from_slice(&bytes),
                    Some(Ok(_)) => {}
                    other => panic!("before {until:?}: {other:?}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {until:?}: {}", String::from_utf8_lossy(&shown)));
    };
    read_until(&mut socket, "# ").await;
    socket
        .send(Message::binary(b"echo kept-$((40+2))\n".to_vec()))
        .await
        .unwrap();
    read_until(&mut socket, "kept-42").await;
    // The shell still open: its history on the disk all the same.
    let mut history = String::new();
    // Looked for a minute: what is tried is that it is written, not how soon.
    for _ in 0..240 {
        history = said("cat /root/.bash_history 2>/dev/null").await;
        if history.contains("echo kept-") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    assert!(history.contains("echo kept-$((40+2))"), "{history:?}");
    drop(socket);
    client.remove_sandbox(&id).await.unwrap();
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_terminal_runs_an_interactive_shell_on_a_tty_of_its_size() {
    let served = Served::spawn("terminal").await;
    served.pull_image("alpine:3.20").await;
    let created = served
        .connect("a-token")
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .await
        .unwrap();
    // A connection to the terminal, attaching to `session` if given: it and what it is told
    // first, the session's id and whether it attached.
    let connect = async |session: Option<&str>| {
        let mut url = Url::parse(&served.url)
            .unwrap()
            .join(&format!("terminal/{}?cols=100&rows=30", created.id))
            .unwrap();
        if let Some(session) = session {
            url.query_pairs_mut().append_pair("session", session);
        }
        url.set_scheme("ws").unwrap();
        let mut request = url.as_str().into_client_request().unwrap();
        let headers = request.headers_mut();
        headers.insert("sec-websocket-protocol", "virtx.terminal".parse().unwrap());
        headers.insert("authorization", "Bearer a-token".parse().unwrap());
        headers.insert("virtx-project", "demo".parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let hello = match socket.next().await {
            Some(Ok(Message::Text(text))) => serde_json::from_str::<Value>(&text).unwrap(),
            other => panic!("before its session: {other:?}"),
        };
        (socket, hello)
    };
    // What `socket` shows, read until it has shown `until`.
    let read_until = async |socket: &mut _, said: &mut Vec<u8>, until: &str| {
        let socket: &mut tokio_tungstenite::WebSocketStream<_> = socket;
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while !String::from_utf8_lossy(said).contains(until) {
                match socket.next().await {
                    Some(Ok(Message::Binary(bytes))) => said.extend_from_slice(&bytes),
                    other => panic!("before {until:?}: {other:?}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {until:?}: {}", String::from_utf8_lossy(said)));
    };

    let (mut socket, hello) = connect(None).await;
    assert_eq!(hello["attached"], false);
    let session = hello["session"].as_str().unwrap().to_owned();
    // Typed once the shell prompts: typed before, it is echoed as it arrives and again after
    // the prompt, as on any terminal.
    let mut said = Vec::new();
    read_until(&mut socket, &mut said, "# ").await;
    socket
        .send(Message::binary(
            b"echo answer=$((6*7)); [ -t 0 ] && echo on-a-tty; stty size; cd /tmp\n".to_vec(),
        ))
        .await
        .unwrap();
    read_until(&mut socket, &mut said, "30 100").await;

    // Gone, and back: the same shell, where it was left, and what it wrote shown again.
    drop(socket);
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let (mut socket, hello) = connect(Some(&session)).await;
    assert_eq!(hello, json!({ "session": session, "attached": true }));
    let mut again = Vec::new();
    read_until(&mut socket, &mut again, "answer=42").await;
    socket
        .send(Message::binary(b"pwd\n".to_vec()))
        .await
        .unwrap();
    read_until(&mut socket, &mut again, "\n/tmp").await;
    // A session not this sandbox's, or not at all: another shell.
    let (other, hello) = connect(Some("nosuchsession")).await;
    assert_eq!(hello["attached"], false);
    assert_ne!(hello["session"], session.as_str());
    drop(other);

    socket
        .send(Message::text(r#"{"cols": 120, "rows": 40}"#))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    socket
        .send(Message::binary(b"stty size; exit 3\n".to_vec()))
        .await
        .unwrap();
    let (said, ended) = tokio::time::timeout(std::time::Duration::from_secs(90), async {
        while let Some(Ok(message)) = socket.next().await {
            match message {
                Message::Binary(bytes) => said.extend_from_slice(&bytes),
                Message::Text(text) => {
                    return (said, serde_json::from_str::<Value>(&text).unwrap());
                }
                _ => {}
            }
        }
        panic!(
            "closed before it said how it ended: {}",
            String::from_utf8_lossy(&said)
        );
    })
    .await
    .unwrap();
    let said = String::from_utf8_lossy(&said);
    assert!(said.contains("answer=42"), "{said}");
    assert!(said.contains("\non-a-tty"), "a tty: {said}");
    assert_eq!(
        said.matches("answer=$((6*7))").count(),
        1,
        "echoed once: {said}"
    );
    assert!(said.contains("30 100"), "its size: {said}");
    assert!(said.contains("40 120"), "resized: {said}");
    assert_eq!(ended, json!({"exit": 3}), "{said}");
    // Ended with its shell: named again, another opens.
    let (_, hello) = connect(Some(&session)).await;
    assert_eq!(hello["attached"], false);
}

/// Boots VMs, as the one above does, and waits for them to stop by themselves: a minute is a
/// second here. A terminal at its prompt does not keep its sandbox from stopping unused; one
/// running a command does, as the guest says when asked.
#[tokio::test]
#[ignore]
async fn a_terminal_keeps_its_sandbox_only_while_it_runs_a_command() {
    let served = Served::spawn_with("terminal-idle", json!({ "minute_secs": 1 })).await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("a-token");
    let mut args = SandboxCreateArgs::new("alpine:3.20");
    args.auto_stop_minutes = Some(1);
    let idle = client.create_sandbox(&args).await.unwrap();
    let busy = client.create_sandbox(&args).await.unwrap();
    // A terminal in each, left open: `busy`'s running a command, `idle`'s at its prompt.
    let open = async |id: &str, typed: &[u8]| {
        let mut url = Url::parse(&served.url)
            .unwrap()
            .join(&format!("terminal/{id}"))
            .unwrap();
        url.set_scheme("ws").unwrap();
        let mut request = url.as_str().into_client_request().unwrap();
        let headers = request.headers_mut();
        headers.insert("sec-websocket-protocol", "virtx.terminal".parse().unwrap());
        headers.insert("authorization", "Bearer a-token".parse().unwrap());
        headers.insert("virtx-project", "demo".parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let mut said = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while !said.windows(2).any(|w| w == b"# ") {
                if let Some(Ok(Message::Binary(bytes))) = socket.next().await {
                    said.extend_from_slice(&bytes);
                }
            }
        })
        .await
        .unwrap();
        socket.send(Message::binary(typed.to_vec())).await.unwrap();
        socket
    };
    let _idle = open(&idle.id, b"").await;
    let _busy = open(&busy.id, b"sleep 600\n").await;

    let state = async |id: &str| client.get_sandbox(id).await.unwrap().state;
    let stopped = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while state(&idle.id).await == SandboxState::Running {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    })
    .await;
    assert!(stopped.is_ok(), "a terminal at its prompt kept its sandbox");
    // Asked about many times since, a minute at a time: running still.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert_eq!(state(&busy.id).await, SandboxState::Running);
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn an_exec_that_accepts_its_log_is_answered_with_it() {
    let served = Served::spawn("exec-streamed").await;
    served.pull_image("alpine:3.20").await;
    let created = served
        .connect("a-token")
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .await
        .unwrap();
    let answer = reqwest::Client::new()
        .post(format!("{}sandboxes/{}/jobs/exec/", served.url, created.id))
        .bearer_auth("a-token")
        .header("virtx-project", "demo")
        .header("accept", "text/event-stream")
        .json(&json!({"cmd": ["sh", "-c", "echo héllo; echo two >&2; exit 3"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::CREATED);
    let job: Value = serde_json::from_str(answer.headers()["virtx-job"].to_str().unwrap()).unwrap();
    assert_eq!(job["cmd"][2], "echo héllo; echo two >&2; exit 3");
    assert_eq!(
        answer.headers()["location"].to_str().unwrap(),
        format!(
            "/sandboxes/{}/jobs/{}",
            created.id,
            job["id"].as_str().unwrap()
        )
    );
    let log = answer.text().await.unwrap();
    assert!(log.contains(r#"data: {"stdout":"héllo"}"#), "{log}");
    assert!(log.contains(r#"data: {"stderr":"two"}"#), "{log}");
    assert!(log.ends_with("data: {\"end\":{\"code\":3}}\n\n"), "{log}");

    // Without it, the job alone, as the protocol says.
    let answer = reqwest::Client::new()
        .post(format!("{}sandboxes/{}/jobs/exec/", served.url, created.id))
        .bearer_auth("a-token")
        .header("virtx-project", "demo")
        .json(&json!({"cmd": ["true"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(answer.status(), StatusCode::CREATED);
    assert!(answer.headers().get("virtx-job").is_none());
    assert_eq!(answer.json::<Value>().await.unwrap()["kind"], "exec");
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_job_ends_with_its_process_not_with_what_it_left_running() {
    let served = Served::spawn("exec-background").await;
    served.pull_image("alpine:3.20").await;
    let created = served
        .connect("a-token")
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .await
        .unwrap();
    let exec = async |cmd: &str| {
        let started = std::time::Instant::now();
        let log = reqwest::Client::new()
            .post(format!("{}sandboxes/{}/jobs/exec/", served.url, created.id))
            .bearer_auth("a-token")
            .header("virtx-project", "demo")
            .header("accept", "text/event-stream")
            .json(&json!({ "cmd": ["sh", "-c", cmd] }))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        (log, started.elapsed())
    };

    // A program left running, its output the job's own: the job ends with its shell, the
    // program running still.
    let (log, _) = exec("sleep 300 & echo started; printf 'no newline'").await;
    let (left, _) = exec("pgrep -f 'sleep 300' >/dev/null && echo running").await;
    assert!(left.contains(r#"data: {"stdout":"running"}"#), "{left}");
    assert!(log.contains(r#"data: {"stdout":"started"}"#), "{log}");
    assert!(log.contains(r#"data: {"stdout":"no newline"}"#), "{log}");
    assert!(log.ends_with("data: {\"end\":{\"code\":0}}\n\n"), "{log}");
    // One that goes on writing: none of what it writes after is the job's, and it is not held
    // up by a pipe no one reads. It writes to stderr: on the job's stdout, its lines -- longer
    // than a pipe writes at once -- would be cut by `seq`'s, and `seq`'s by them.
    let (log, _) =
        exec("(while true; do head -c 8000 /dev/zero | tr '\\0' x; echo; date >> /tmp/w; sleep 0.01; done) >&2 & seq 1 20000; echo done").await;
    // Ended while what it left writes on: /tmp/w grows after its end.
    let (grew, _) = exec(
        "a=$(wc -l < /tmp/w); sleep 1; b=$(wc -l < /tmp/w); [ \"$b\" -gt \"$a\" ] && echo growing",
    )
    .await;
    assert!(grew.contains(r#"data: {"stdout":"growing"}"#), "{grew}");
    let lines: Vec<&str> = log.lines().filter(|l| l.starts_with("data: ")).collect();
    assert!(
        lines.contains(&r#"data: {"stdout":"20000"}"#),
        "{}",
        lines.len()
    );
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.starts_with(r#"data: {"stdout":"#))
            .count(),
        20001,
        "every line of seq's and done, whole"
    );
    assert_eq!(lines.last().unwrap(), &"data: {\"end\":{\"code\":0}}");
    let (log, _) = exec("sleep 3; a=$(wc -l < /tmp/w); sleep 2; b=$(wc -l < /tmp/w); [ $b -gt $((a + 20)) ] && echo moving").await;
    assert!(
        log.contains(r#"data: {"stdout":"moving"}"#),
        "the writer was held up: {log}"
    );
    // A job that ends and leaves nothing: as before.
    let (log, _) = exec("echo one; exit 4").await;
    assert!(log.ends_with("data: {\"end\":{\"code\":4}}\n\n"), "{log}");
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_job_started_with_stdin_reads_what_is_written_to_it() {
    let served = Served::spawn("exec-stdin").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("a-token");
    let id = client
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .await
        .unwrap()
        .id;
    let mut args = SandboxExecArgs::new(["sh", "-c", "tr a-z A-Z; echo done"]);
    args.stdin = true;
    let job = client.exec_sandbox(&id, &args).await.unwrap();
    client
        .write_sandbox_job_stdin(&id, &job.id, Some("hello\n"), false)
        .await
        .unwrap();
    client
        .write_sandbox_job_stdin(&id, &job.id, Some("world\n"), true)
        .await
        .unwrap();
    let log: Vec<_> = client
        .sandbox_job_log(&id, &job.id, None)
        .try_collect()
        .await
        .unwrap();
    let out: Vec<_> = log
        .iter()
        .filter_map(|e| match &e.entry {
            SandboxJobEntry::Stdout(line) => Some(line.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(out, ["HELLO", "WORLD", "done"]);
    // Closed, and ended: refused.
    assert_eq!(
        status(
            client
                .write_sandbox_job_stdin(&id, &job.id, Some("x"), false)
                .await
        ),
        409
    );
    // Started without one: refused, and its stdin empty.
    let job = client
        .exec_sandbox(&id, &SandboxExecArgs::new(["sh", "-c", "cat; echo eof"]))
        .await
        .unwrap();
    assert_eq!(
        status(
            client
                .write_sandbox_job_stdin(&id, &job.id, Some("x"), false)
                .await
        ),
        409
    );
    let log: Vec<_> = client
        .sandbox_job_log(&id, &job.id, None)
        .try_collect()
        .await
        .unwrap();
    assert!(
        log.iter()
            .any(|e| e.entry == SandboxJobEntry::Stdout("eof".into()))
    );
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_fork_bomb_leaves_the_sandbox_answering() {
    let served = Served::spawn("fork-bomb").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("a-token");
    // Large enough for more threads than the kernel's pids, as most are.
    let mut args = SandboxCreateArgs::new("alpine:3.20");
    args.memory_mib = Some(2048);
    let id = client.create_sandbox(&args).await.unwrap().id;
    let ran = async |cmd: &str| {
        let job = client
            .exec_sandbox(&id, &SandboxExecArgs::new(["sh", "-c", cmd]))
            .await
            .unwrap();
        let log: Vec<_> = client
            .sandbox_job_log(&id, &job.id, None)
            .try_collect()
            .await
            .unwrap();
        log.into_iter()
            .filter_map(|e| match e.entry {
                SandboxJobEntry::Stdout(line) => Some(line),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    ran("sh -c 'f(){ f & f & wait; }; f' >/dev/null 2>&1 </dev/null & echo started").await;
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    // Answered while the bomb goes on, rightly: how soon is not what is tried here. The
    // timeout only keeps a sandbox that never answers from holding the suite.
    let asked = std::time::Instant::now();
    let answered = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        // The processes counted by the shell's own glob: no fork, which the bomb may starve.
        ran("echo alive; set -- /proc/[0-9]*; echo $#"),
    )
    .await
    .expect("a command answered beside the bomb");
    eprintln!("BOMB {answered:?} in {:?}", asked.elapsed());
    assert_eq!(answered[0], "alive", "{answered:?}");
    let processes: u32 = answered[1].parse().unwrap_or(0);
    assert!(processes > 100, "the bomb went on beside it: {answered:?}");
    client.stop_sandbox(&id).await.unwrap();
}
