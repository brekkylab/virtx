//! The built binary, served over a config of its own and called over HTTP as a client would:
//! each request a `POST` to `/rpc` with a token and a project.

use std::path::PathBuf;

use futures_util::{SinkExt as _, StreamExt as _};
use reqwest::{StatusCode, Url};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::{self, Message};
use virtx_uvm::{FsTree, ImageStore, Registry};

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
        let registry = Registry::default();
        let (_, manifest) = registry.pull_manifest(&reference).await.unwrap();
        let config = registry.pull_config(&reference, &manifest).await.unwrap();
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
