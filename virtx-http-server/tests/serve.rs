//! The built binary, served over a config of its own and called over HTTP as a client would:
//! each request a `POST` to `/rpc`, or one to the files endpoints, with a token and a project
//! -- by hand, and through `virtx-core`'s client given both at connect.

use std::path::PathBuf;

use futures_util::{SinkExt as _, StreamExt as _};
use reqwest::{StatusCode, Url};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::{self, Message};
use virtx_core::{
    DirEntry, FileRoot, Request as _, SnapshotCreate, SnapshotCreateArgs, SnapshotCreateOutput,
    TransportClient, VolumeCreate, VolumeCreateArgs, VolumeCreateOutput,
};
use virtx_uvm::{Entry, FsTree, ImageStore, RegistryClient};

const INVALID_PARAMS: i64 = -32602;
const NOT_FOUND: i64 = -31001;
const ALREADY_EXISTS: i64 = -31002;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A server over an empty home, killed and removed when this drops.
struct Served {
    process: Child,
    url: String,
    home: PathBuf,
}

impl Served {
    async fn spawn(name: &str) -> Served {
        let home = std::env::temp_dir().join(format!("virtx-http-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let config = json!({
            "listen": "127.0.0.1:0",
            "home": home,
            "origins": ["https://allowed.example"],
            "users": {
                "alice": { "token_sha256": sha256("a-token") },
                "bob": { "token_sha256": sha256("b-token") },
            },
            "projects": { "demo": { "members": ["alice"] } },
        });
        let path = home.join("config.json");
        std::fs::write(&path, config.to_string()).unwrap();

        let mut process = Command::new(env!("CARGO_BIN_EXE_virtx-http-server"))
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
        Served { process, url, home }
    }

    /// `method` called with `params`, as `token` in `project`: the HTTP status, and the
    /// message answered.
    async fn call(
        &self,
        token: Option<&str>,
        project: Option<&str>,
        method: &str,
        params: Value,
    ) -> (StatusCode, Option<Value>) {
        let mut request = reqwest::Client::new()
            .post(&self.url)
            .header("accept", "application/json")
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(project) = project {
            request = request.header("virtx-project", project);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        (status, response.json().await.ok())
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
        let reference = reference.parse().unwrap();
        let registry = RegistryClient::new(store.clone(), None);
        let (_, _, manifest) = registry.pull_manifest(&reference).await.unwrap();
        let (_, config) = registry.pull_config(&reference, &manifest).await.unwrap();
        let mut layers = Vec::new();
        for descriptor in manifest.layers() {
            let tree = registry.pull_layer(&reference, descriptor).await.unwrap();
            layers.push(store.insert_layer(&tree).unwrap());
        }
        let image = store.insert_image(&config, &layers).unwrap();
        store.insert_tag(&reference, &image).unwrap();
    }

    /// A port's URL, as the client resolves what `sandbox.init` answered: against the URL it
    /// called.
    fn port_url(&self, answered: &Value) -> Url {
        Url::parse(&self.url)
            .unwrap()
            .join(answered["url"].as_str().unwrap())
            .unwrap()
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

    /// `method`'s result, as alice in `demo`.
    async fn result(&self, method: &str, params: Value) -> Result<Value, i64> {
        let (status, answer) = self
            .call(Some("a-token"), Some("demo"), method, params)
            .await;
        assert_eq!(status, StatusCode::OK);
        let answer = answer.expect("a message");
        match answer.get("error") {
            Some(error) => Err(error["code"].as_i64().unwrap()),
            None => Ok(answer["result"].clone()),
        }
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.process.start_kill();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn sha256(text: &str) -> String {
    let out = std::process::Command::new("shasum")
        .args(["-a", "256"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write as _;
            child.stdin.take().unwrap().write_all(text.as_bytes())?;
            child.wait_with_output()
        })
        .unwrap();
    String::from_utf8(out.stdout).unwrap()[..64].to_owned()
}

#[tokio::test]
async fn a_request_reaches_only_a_member_s_project() {
    let served = Served::spawn("access").await;
    let list = |token, project| served.call(token, project, "snapshot.list", json!({}));

    let (status, answer) = list(Some("a-token"), Some("demo")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer.unwrap()["result"], json!([]));

    assert_eq!(list(None, Some("demo")).await.0, StatusCode::UNAUTHORIZED);
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
            .post(&served.url)
            .bearer_auth("a-token")
            .header("virtx-project", "demo")
            .header("origin", origin)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "snapshot.list"}))
            .send()
            .await
            .unwrap()
            .status()
    };
    assert_eq!(from("https://allowed.example").await, StatusCode::OK);
    assert_eq!(from("https://evil.example").await, StatusCode::FORBIDDEN);

    // A browser's preflight, sent without the token, is answered for the WebDAV methods from
    // an origin let in, and left without its answer from any other.
    let files = format!("{}volumes/data/", served.url.strip_suffix("rpc").unwrap());
    let preflight = async |origin| {
        reqwest::Client::new()
            .request(reqwest::Method::OPTIONS, &files)
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
        .result("volume.create", json!({"name": "data"}))
        .await
        .unwrap();
    let listed = reqwest::Client::new()
        .request(reqwest::Method::from_bytes(b"PROPFIND").unwrap(), &files)
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

/// The images are an OCI registry at `/images/v2/`, to any user, naming no project.
#[tokio::test]
async fn the_images_are_a_registry_to_any_user() {
    let served = Served::spawn("registry").await;
    served.file_image("alpine:3.21");
    let base = served.url.strip_suffix("/rpc").unwrap();
    let client = reqwest::Client::new();

    let anonymous = client
        .get(format!("{base}/images/v2/"))
        .send()
        .await
        .unwrap();
    assert_eq!(anonymous.status(), StatusCode::UNAUTHORIZED);
    let root = client
        .get(format!("{base}/images/v2/"))
        .bearer_auth("b-token")
        .send()
        .await
        .unwrap();
    assert_eq!(root.status(), StatusCode::OK);

    let manifest = client
        .get(format!("{base}/images/v2/alpine/manifests/3.21"))
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
            "{base}/images/v2/alpine/blobs/{}",
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
        .result("volume.create", json!({"name": "data"}))
        .await
        .unwrap();
    let base = served.url.strip_suffix("rpc").unwrap();
    let send = async |method: &str, path: &str, token: Option<&str>, body: &'static str| {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut request = reqwest::Client::new()
            .request(method, format!("{base}{path}"))
            .header("virtx-project", "demo")
            .body(body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        (status, response.text().await.unwrap())
    };

    let put = "volumes/data/a.txt";
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
    let (status, body) = send("PROPFIND", "volumes/data/", Some("a-token"), "").await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    assert!(body.contains("a.txt"), "{body}");
    assert_eq!(
        send("GET", "volumes/nobody/a.txt", Some("a-token"), "")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

/// The HTTP status a files request was refused with.
fn status<T: std::fmt::Debug>(answered: Result<T, virtx_core::Error>) -> u16 {
    match answered {
        Err(virtx_core::Error::Status(status, _)) => status,
        other => panic!("expected a status, got {other:?}"),
    }
}

fn names(entries: Vec<DirEntry>) -> Vec<String> {
    entries.into_iter().map(|entry| entry.name).collect()
}

/// The files of a volume and a snapshot through `virtx-core`'s client, given the token and the
/// project at connect, as a program over the bindings reaches them.
#[tokio::test]
async fn the_client_reaches_a_project_with_the_headers_given_at_connect() {
    let served = Served::spawn("client").await;
    let base = served.url.strip_suffix("rpc").unwrap();
    let connect = |token: &str| {
        let mut headers = http::HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers.insert("virtx-project", "demo".parse().unwrap());
        TransportClient::connect(base, headers).unwrap()
    };
    let client = connect("a-token");
    client
        .request::<VolumeCreateOutput>(
            VolumeCreate::NAME,
            VolumeCreateArgs {
                name: Some("data".into()),
            },
        )
        .await
        .unwrap();
    client
        .request::<SnapshotCreateOutput>(
            SnapshotCreate::NAME,
            SnapshotCreateArgs {
                name: Some("work".into()),
                disk_gib: Some(1),
            },
        )
        .await
        .unwrap();
    let volume = FileRoot::Volume("data".into());
    let snapshot = FileRoot::Snapshot("work".into());
    let home = served.home.join("project/demo");

    // A volume's files, on the host's disk.
    client.create_dir(&volume, "dir").await.unwrap();
    client
        .write(&volume, "dir/a.txt", "hello world", None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(home.join("volume/data/files/dir/a.txt")).unwrap(),
        b"hello world"
    );
    let metadata = client.stat(&volume, "dir/a.txt").await.unwrap();
    assert!(!metadata.is_dir && metadata.size == 11, "{metadata:?}");
    assert_eq!(
        names(client.read_dir(&volume, "dir").await.unwrap()),
        ["a.txt"]
    );
    assert_eq!(names(client.read_dir(&volume, "").await.unwrap()), ["dir"]);
    let read = client.read(&volume, "dir/a.txt", 6, Some(3)).await.unwrap();
    assert_eq!(&read[..], b"wor");
    client
        .rename(&volume, "dir/a.txt", "b.txt", false)
        .await
        .unwrap();
    client.copy(&volume, "b.txt", "c.txt", false).await.unwrap();
    assert_eq!(
        status(client.copy(&volume, "b.txt", "c.txt", false).await),
        412
    );
    client.copy(&volume, "dir", "copied", false).await.unwrap();
    client.remove(&volume, "dir", false).await.unwrap();
    assert_eq!(status(client.stat(&volume, "dir").await), 404);
    assert!(client.stat(&volume, "copied").await.unwrap().is_dir);
    let read = client.read(&volume, "c.txt", 0, None).await.unwrap();
    assert_eq!(&read[..], b"hello world");
    assert_eq!(
        status(client.write(&volume, "no/file", "x", None).await),
        409
    );
    let nobody = FileRoot::Volume("nobody".into());
    assert_eq!(status(client.stat(&nobody, "").await), 404);

    // A snapshot's files, in its image.
    assert_eq!(
        names(client.read_dir(&snapshot, "").await.unwrap()),
        [] as [&str; 0]
    );
    client.create_dir(&snapshot, "etc").await.unwrap();
    client
        .write(&snapshot, "etc/hostname", "guest\n", None)
        .await
        .unwrap();
    assert_eq!(
        status(client.write(&snapshot, "no/file", "x", None).await),
        409
    );
    assert_eq!(status(client.create_dir(&snapshot, "etc").await), 405);
    let read = client
        .read(&snapshot, "etc/hostname", 1, Some(2))
        .await
        .unwrap();
    assert_eq!(&read[..], b"ue");
    client
        .rename(&snapshot, "etc/hostname", "hostname", false)
        .await
        .unwrap();
    client
        .copy(&snapshot, "hostname", "etc/copy", false)
        .await
        .unwrap();
    assert_eq!(
        status(client.copy(&snapshot, "hostname", "etc/copy", false).await),
        412
    );
    client.remove(&snapshot, "hostname", false).await.unwrap();
    assert_eq!(status(client.stat(&snapshot, "hostname").await), 404);
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
    assert_eq!(status(client.write(&snapshot, "x", "y", None).await), 423);
    drop(lock);
    let nobody = FileRoot::Snapshot("nobody".into());
    assert_eq!(status(client.read_dir(&nobody, "").await), 404);

    // Not a member of the project: every request is refused, the RPC and the files alike.
    let bob = connect("b-token");
    let refused = bob
        .request::<VolumeCreateOutput>(VolumeCreate::NAME, VolumeCreateArgs { name: None })
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("403"), "{refused}");
    assert_eq!(status(bob.stat(&volume, "c.txt").await), 403);
}

#[tokio::test]
async fn a_project_keeps_its_snapshots_and_volumes() {
    let served = Served::spawn("project").await;
    let created = served
        .result("snapshot.create", json!({"name": "work", "disk_gib": 1}))
        .await
        .unwrap();
    assert_eq!(created, json!({"name": "work"}));
    assert!(
        served
            .home
            .join("project/demo/snapshot/work.ext4")
            .is_file()
    );
    let listed = served.result("snapshot.list", json!({})).await.unwrap();
    assert_eq!(listed[0]["name"], "work");

    served
        .result("volume.create", json!({"name": "data"}))
        .await
        .unwrap();
    assert!(served.home.join("project/demo/volume/data/files").is_dir());
    assert_eq!(
        served
            .result("volume.create", json!({"name": "data"}))
            .await,
        Err(ALREADY_EXISTS)
    );

    // A port that is none, and a sandbox of an image nobody filed, are refused before it
    // boots.
    assert_eq!(
        served
            .result(
                "sandbox.init",
                json!({"image": {"type": "tag", "tag": "a:b"}, "ports": [0]})
            )
            .await,
        Err(INVALID_PARAMS)
    );
    assert_eq!(
        served
            .result(
                "sandbox.init",
                json!({"image": {"type": "tag", "tag": "a:b"}})
            )
            .await,
        Err(NOT_FOUND)
    );
}

#[tokio::test]
async fn a_port_is_a_websocket_behind_its_capability() {
    let served = Served::spawn("ports").await;
    served.file_image("here:latest");
    let init = served
        .result(
            "sandbox.init",
            json!({"image": {"type": "tag", "tag": "here:latest"}, "ports": [3000]}),
        )
        .await
        .unwrap();
    let opened = &init["ports"][0];
    assert_eq!(opened["port"], 3000);
    let url = served.port_url(opened);
    let capability = url.path().strip_prefix("/ports/").unwrap().to_owned();
    assert_eq!(capability.len(), 64);
    let listed = served.result("sandbox.list", json!({})).await.unwrap();
    assert_eq!(listed[0]["ports"], init["ports"]);

    // A WebSocket of `virtx.tcp`, and nothing else; the capability is all it needs.
    let plain = reqwest::get(url.clone()).await.unwrap();
    assert_eq!(plain.status(), StatusCode::UPGRADE_REQUIRED);
    let mut ws = url.clone();
    ws.set_scheme("ws").unwrap();
    assert_eq!(Served::open(&ws, None).await.err(), Some(400));
    let mut guessed = ws.clone();
    guessed.set_path(&format!("/ports/{}", "0".repeat(64)));
    assert_eq!(
        Served::open(&guessed, Some("virtx.tcp")).await.err(),
        Some(404)
    );

    // Opened, and closed at once: no machine runs to reach the port in.
    let mut socket = Served::open(&ws, Some("virtx.tcp")).await.unwrap();
    assert!(matches!(
        socket.next().await,
        Some(Ok(Message::Close(_))) | None
    ));

    served
        .result("sandbox.close", json!({"sandbox": init["id"]}))
        .await
        .unwrap();
    assert_eq!(Served::open(&ws, Some("virtx.tcp")).await.err(), Some(404));
}

/// Boots VMs: needs the network to pull alpine, the binary signed for the hypervisor, and the
/// guest kernel where libkrun finds it.
#[tokio::test]
#[ignore]
async fn a_port_s_websocket_reaches_what_listens_on_it_in_the_running_machine() {
    let served = Served::spawn("port-reached").await;
    served.pull_image("alpine:3.20").await;
    let init = served
        .result(
            "sandbox.init",
            json!({"image": {"type": "tag", "tag": "alpine:3.20"}, "ports": [8000]}),
        )
        .await
        .unwrap();
    let mut url = served.port_url(&init["ports"][0]);
    url.set_scheme("ws").unwrap();

    let serving = served.result(
        "sandbox.exec",
        json!({
            "sandbox": init["id"],
            "cmd": ["sh", "-c", "while true; do echo hello | nc -l -p 8000; done"],
            "timeout_ms": 30000,
        }),
    );
    let reached = async {
        // Until the machine is up and something listens in it.
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
    tokio::select! {
        said = reached => assert_eq!(said, b"hello\n"),
        ended = serving => panic!("nc ended before it was reached: {ended:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn a_cancelled_exec_kills_its_command_and_keeps_the_sandbox() {
    let served = Served::spawn("exec-cancelled").await;
    served.pull_image("alpine:3.20").await;
    let init = served
        .result(
            "sandbox.init",
            json!({"image": {"type": "tag", "tag": "alpine:3.20"}}),
        )
        .await
        .unwrap();
    let post = |message: Value| {
        reqwest::Client::new()
            .post(&served.url)
            .header("accept", "application/json, text/event-stream")
            .bearer_auth("a-token")
            .header("virtx-project", "demo")
            .json(&message)
            .send()
    };
    let running = || async {
        let listed = served.result("sandbox.list", json!({})).await.unwrap();
        listed[0]["running"].as_bool().unwrap()
    };

    let exec = post(json!({
        "jsonrpc": "2.0",
        "id": "exec",
        "method": "sandbox.exec",
        "params": {"sandbox": init["id"], "cmd": ["sleep", "600"]},
    }));
    let cancelled = async {
        while !running().await {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let cancel = post(json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": {"requestId": "exec"},
        }));
        assert_eq!(cancel.await.unwrap().status(), StatusCode::ACCEPTED);
    };
    let (exec, ()) = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::join!(exec, cancelled)
    })
    .await
    .expect("the exec ends with its cancellation");
    // Nothing printed, so nothing streamed: no response, and no body.
    assert_eq!(exec.unwrap().status(), StatusCode::NO_CONTENT);
    assert!(!running().await);
}
