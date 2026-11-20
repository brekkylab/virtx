//! The built binary, served over a config of its own and called over HTTP as a client would:
//! what its sandboxes' machines do, and what it keeps across a restart -- each test booting
//! them, and so ignored but where `--include-ignored` asks. What is answered without a machine
//! is the app's own tests, in `src/serve.rs`.

use std::path::PathBuf;

use futures_util::{SinkExt as _, StreamExt as _, TryStreamExt as _};
use reqwest::{Method, StatusCode, Url};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::{self, Message};
use virtx_core::{
    Auth, Client, Recipe, SandboxCreateArgs, SandboxExecArgs, SandboxJobEnd, SandboxJobEndReason,
    SandboxJobEntry, Step,
};
use virtx_uvm::{ImageStore, RegistryClient};

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A server over an empty home, killed and removed when this drops.
struct Served {
    process: Child,
    /// Its base URL, ending in `/`.
    url: String,
    home: PathBuf,
    /// The issuer whose tokens it takes, serving its keys.
    issuer: tokio::task::JoinHandle<()>,
    /// The directory of the project `demo`, the tests' own.
    demo: PathBuf,
    /// What its environment has besides this process's, as each server over its home is
    /// started.
    env: Vec<(&'static str, &'static str)>,
}

/// The issuer the server's config names, and a P-256 key of its, with its public half as a
/// JWK's `x` and `y`.
const ISSUER: &str = "https://issuer.example";
const KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgTFNNLGR4z9BOxmmG
kWTAwIsXQv7M5AnnFYbw7KqIAduhRANCAARm+2fsK24SWK8QJjqyNzhrBvlhw8UG
rABUOPQZw+SIzclThn/7Vcn7i0rAcmmlVj8sdMeJsFeMZ3CehtnTDGVK
-----END PRIVATE KEY-----
";
const X: &str = "Zvtn7CtuElivECY6sjc4awb5YcPFBqwAVDj0GcPkiM0";
const Y: &str = "yVOGf_tVyfuLSsByaaVWPyx0x4mwV4xncJ6G2dMMZUo";

/// A token of `user`'s, as the issuer signs one for the server, good for an hour.
fn token(user: &str) -> String {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    header.kid = Some("k1".into());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let claims = json!({"iss": ISSUER, "aud": "virtx", "exp": now + 3600, "sub": user});
    let key = jsonwebtoken::EncodingKey::from_ec_pem(KEY.as_bytes()).unwrap();
    jsonwebtoken::encode(&header, &claims, &key).unwrap()
}

/// The issuer's keys, served on the loopback: their URL, and the task serving them.
async fn issuer() -> (String, tokio::task::JoinHandle<()>) {
    let keys = json!({"keys": [{
        "kty": "EC", "crv": "P-256", "kid": "k1", "alg": "ES256", "use": "sig", "x": X, "y": Y,
    }]});
    let served = axum::Router::new().route(
        "/keys",
        axum::routing::get(move || async move { keys.to_string() }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/keys", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, served).await;
    });
    (url, task)
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
        Served::spawn_with(name, json!({}), &[]).await
    }

    /// As [`Served::spawn`], its config with `more` in it besides, and its environment with
    /// `env`.
    async fn spawn_with(name: &str, more: Value, env: &[(&'static str, &'static str)]) -> Served {
        let home = std::env::temp_dir().join(format!("virtx-http-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        let mut config = json!({
            "listen": "127.0.0.1:0",
            "home": home,
            "origins": ["https://allowed.example"],
            // The issuer's keys are where `start` serves them.
            "oidc": {
                "issuer": ISSUER,
                "client_id": "virtx",
                "username_claim": "sub",
                "jwks_uri": "",
            },
        });
        for (key, value) in more.as_object().unwrap() {
            config[key] = value.clone();
        }
        std::fs::write(home.join("config.json"), config.to_string()).unwrap();
        Served::start(home, env.to_vec()).await
    }

    /// A server over `home`, as its `config.json` says, with `env` in its environment.
    async fn start(home: PathBuf, env: Vec<(&'static str, &'static str)>) -> Served {
        let path = home.join("config.json");
        // The issuer's keys served anew, at the address the config now names.
        let (keys, issuer) = issuer().await;
        let mut config: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        config["oidc"]["jwks_uri"] = json!(keys);
        std::fs::write(&path, config.to_string()).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_virtx-server-tcp"));
        command
            .arg("serve")
            .arg("--config")
            .arg(&path)
            .envs(env.iter().copied())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // A process group of its own, which ctrl-break can be sent to alone (`again`).
        #[cfg(windows)]
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
        let mut process = command.spawn().unwrap();
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
        // The tests' project, `demo`, made the first time; kept across a restart.
        let http = reqwest::Client::new();
        let made = http
            .post(format!("{url}projects/"))
            .bearer_auth(token("tester"))
            .json(&json!({ "name": "demo" }))
            .send()
            .await
            .unwrap();
        assert!(
            made.status().is_success() || made.status() == StatusCode::CONFLICT,
            "{}",
            made.status()
        );
        let demo: Value = http
            .get(format!("{url}projects/demo"))
            .bearer_auth(token("tester"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let demo = home.join("project").join(demo["id"].as_str().unwrap());
        Served {
            process,
            url,
            home,
            issuer,
            demo,
            env,
        }
    }

    /// This server interrupted, as its operator stops it, and another started over its home.
    async fn again(mut self) -> Served {
        let pid = self.process.id().unwrap();
        #[cfg(unix)]
        {
            let pid = pid.to_string();
            let interrupted = Command::new("kill").args(["-INT", &pid]).status().await;
            assert!(interrupted.unwrap().success());
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
            // SAFETY: a plain call, naming the group the server leads.
            let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
            assert_ne!(sent, 0, "{}", std::io::Error::last_os_error());
        }
        self.process.wait().await.unwrap();
        // Not removed: the next is over it.
        let home = std::mem::take(&mut self.home);
        Served::start(home, std::mem::take(&mut self.env)).await
    }

    /// Sandbox `id`'s machine as `PUT /sandboxes/{id}/resources` answers it, asking no change:
    /// whether it is up.
    async fn up(&self, id: &str) -> bool {
        let answer: Value = reqwest::Client::new()
            .put(format!(
                "{}/sandboxes/{id}/resources",
                self.url.trim_end_matches('/')
            ))
            .bearer_auth(token("tester"))
            .header("virtx-project", "demo")
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        answer["up"].as_bool().unwrap_or_else(|| panic!("{answer}"))
    }

    /// Until sandbox `id`'s machine is parked: a guard against a hang, not a measure of how
    /// soon.
    async fn parked(&self, id: &str) {
        let parked = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            while self.up(id).await {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        })
        .await;
        assert!(parked.is_ok(), "sandbox {id} never parked");
    }

    /// A client of `user`'s, in `demo`.
    fn connect(&self, user: &str) -> Client {
        let auth = Auth::Bearer { token: token(user) };
        {
            let client = Client::connect(&self.url, auth, http::HeaderMap::new()).unwrap();
            client.set_project(Some("demo")).unwrap();
            client
        }
    }

    /// Pull `reference` from its registry into `demo`'s images.
    async fn pull_image(&self, reference: &str) {
        let store = ImageStore::new(self.demo.join("images")).unwrap();
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
        self.issuer.abort();
        let _ = self.process.start_kill();
        if !self.home.as_os_str().is_empty() {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }
}

/// The HTTP status a request was refused with.
fn status<T: std::fmt::Debug>(answered: Result<T, virtx_core::Error>) -> u16 {
    match answered {
        Err(virtx_core::Error::Status(status, ..) | virtx_core::Error::Refusal { status, .. }) => {
            status
        }
        other => panic!("expected a status, got {other:?}"),
    }
}

/// A sandbox of `image` with 1 GiB of memory: enough for what these tests run in it, and little
/// enough that the suite's machines, booting at once, fit a host that commits each one's memory as
/// it boots (`VIRTX_PREFAULT_MEMORY`, as Windows' CI runs them), where 4 GiB each does not.
fn small(image: impl Into<virtx_core::ImageName>) -> SandboxCreateArgs {
    let mut args = SandboxCreateArgs::new(image);
    args.memory_mib = Some(1024);
    args
}

/// Boots VMs: needs the network to pull alpine, the binary signed for the hypervisor, and the
/// guest kernel where libkrun finds it.
/// A server whose sandboxes stay up a minute past their last use, for a test of what a machine
/// left running between commands: its own default parks them at once.
const KEPT_UP: (&str, &str) = ("VIRTX_PARK_AFTER_MS", "60000");

#[tokio::test]
#[ignore]
async fn a_port_s_websocket_reaches_what_listens_on_it_until_the_sandbox_goes() {
    let served = Served::spawn("port-reached").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("tester");
    let mut args = small("alpine:3.20");
    args.ports = vec![8000.into()];
    let created = client.create_sandbox(&args).await.unwrap();
    assert_eq!(created.ports[0].port, 8000);
    let url = served.port_url(created.ports[0].url.as_deref().unwrap());
    assert_eq!(url.path().strip_prefix("/ports/").unwrap().len(), 64);
    // Its URL answered only on its making: the URLs carry the capability.
    let asked = virtx_core::SandboxPort {
        url: None,
        ..created.ports[0].clone()
    };
    assert_eq!(
        client.list_sandbox().await.unwrap()[0].ports,
        [asked.clone()]
    );
    assert_eq!(
        client.get_sandbox(&created.id).await.unwrap().ports,
        [asked]
    );

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
    let client = served.connect("tester");
    let mut args = small("nginx:1.27-alpine");
    args.ports = vec![80.into()];
    let created = client.create_sandbox(&args).await.unwrap();
    let url = served.port_url(created.ports[0].url.as_deref().unwrap());

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

/// Boots VMs, as the one above does. A built image keeps the recipe it was built from in its
/// config, under its label, and each step in its history, as `docker history` reads them.
#[tokio::test]
#[ignore]
async fn a_built_image_keeps_its_recipe_and_its_steps_in_its_history() {
    let served = Served::spawn("recipe").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("tester");
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
            .bearer_auth(token("tester"))
            .header("virtx-project", "demo")
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
        .bearer_auth(token("tester"))
        .header("virtx-project", "demo")
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
}

/// Boots VMs, as the one above does. A sandbox resized: its machine parked, if nothing uses
/// it, and booted with the vCPUs and memory it was given from its next boot on; one in use
/// refused.
#[tokio::test]
#[ignore]
async fn an_unused_sandbox_is_resized_as_it_next_boots() {
    let served = Served::spawn("resize").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("tester");
    let mut args = small("alpine:3.20");
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
            .bearer_auth(token("tester"))
            .header("virtx-project", "demo")
            .json(&body)
            .send()
            .await
            .unwrap();
        (r.status(), r.json().await.unwrap_or(Value::Null))
    };
    assert_eq!(said("nproc").await.trim(), "2");

    // In use: its machine is the size it booted as.
    let sleeping = client
        .exec_sandbox(&id, &SandboxExecArgs::new(["sleep", "600"]))
        .await
        .unwrap();
    let (status, refused) = resize(json!({ "vcpus": 3 })).await;
    assert_eq!(status, StatusCode::CONFLICT, "{refused}");
    assert_eq!(refused["error"]["code"], "in_use");
    assert!(served.up(&id).await);

    // Unused: parked, resized, and booted so.
    client.remove_sandbox_job(&id, &sleeping.id).await.unwrap();
    let (status, answer) = resize(json!({ "vcpus": 3, "memory_mib": 1536 })).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(
        (
            answer["vcpus"].as_u64(),
            answer["memory_mib"].as_u64(),
            answer["up"].as_bool()
        ),
        (Some(3), Some(1536), Some(false))
    );
    assert!(client.get_sandbox(&id).await.unwrap().running);
    assert_eq!(said("nproc").await.trim(), "3");
    let kib: u64 = said("awk '/MemTotal/ {print $2}' /proc/meminfo")
        .await
        .trim()
        .parse()
        .unwrap();
    assert!((1300 << 10..=1536 << 10).contains(&kib), "{kib} KiB");
    client.remove_sandbox(&id).await.unwrap();
}

/// Boots VMs, as the one above does. A sandbox's disk grown: its filesystem as large as it
/// once its machine boots again, what was on it kept; never shrunk.
#[tokio::test]
#[ignore]
async fn a_sandbox_s_disk_is_grown_as_it_next_boots() {
    // Kept up between commands: grown while up, it is the old size until it next boots.
    let served = Served::spawn_with("disk", json!({}), &[KEPT_UP]).await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("tester");
    let mut args = small("alpine:3.20");
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
            .bearer_auth(token("tester"))
            .header("virtx-project", "demo")
            .json(&json!({ "disk_gib": n }))
            .send()
    };
    // Up: grown once it boots again -- here, once a resize has parked it.
    let r = grow(3).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let answer: Value = r.json().await.unwrap();
    assert_eq!(answer, json!({ "disk_gib": 3, "up": true }));
    assert_eq!(gib().await, 1);
    let parked = reqwest::Client::new()
        .put(format!(
            "{}/sandboxes/{id}/resources",
            served.url.trim_end_matches('/')
        ))
        .bearer_auth(token("tester"))
        .header("virtx-project", "demo")
        .json(&json!({ "vcpus": 2 }))
        .send()
        .await
        .unwrap();
    assert_eq!(parked.status(), StatusCode::OK);
    assert!(!served.up(&id).await);
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

/// Boots a VM, as the one above does. A sandbox's files, read over WebDAV through its agent:
/// its image's and what it wrote alike, a directory listed, a file read whole and in part, and
/// written; once it is parked, its machine booted to read or write them.
#[tokio::test]
#[ignore]
async fn a_sandbox_s_files_are_read_and_written_its_machine_booted_to_write_them() {
    let served = Served::spawn_with("files", json!({}), &[("VIRTX_PARK_AFTER_MS", "1000")]).await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("tester");
    let id = client
        .create_sandbox(&small("alpine:3.20"))
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
            .bearer_auth(token("tester"))
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

    // Parked: booted again, to read as to write.
    served.parked(&id).await;
    let r = asked("GET", "/root/d/b.txt").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.text().await.unwrap(), "put\n");
    assert!(served.up(&id).await);
    served.parked(&id).await;
    let r = asked("PUT", "/root/d/c.txt")
        .body("c\n")
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    assert!(served.up(&id).await);
    client.remove_sandbox(&id).await.unwrap();
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_cancelled_job_is_killed_and_the_sandbox_kept() {
    let served = Served::spawn("job-cancelled").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("tester");
    let id = client
        .create_sandbox(&small("alpine:3.20"))
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
    assert!(client.get_sandbox(&id).await.unwrap().running);
}

/// Boots VMs, as the one above does. A job's end says why the server ended it, and only
/// then: its timeout run out, or it cancelled; not a signal sent it, nor its own exit.
#[tokio::test]
#[ignore]
async fn a_job_s_end_says_why_the_server_ended_it() {
    let served = Served::spawn_with("job-ends", json!({}), &[KEPT_UP]).await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("tester");
    let id = client
        .create_sandbox(&small("alpine:3.20"))
        .await
        .unwrap()
        .id;
    let end = async |job: &str| {
        let log: Vec<_> = client
            .sandbox_job_log(&id, job, None)
            .try_collect()
            .await
            .unwrap();
        match log.last().map(|entry| &entry.entry) {
            Some(SandboxJobEntry::End(end)) => end.clone(),
            other => panic!("a log ends with its end, not {other:?}"),
        }
    };

    // Its own exit, 137 as well: no reason.
    for code in [0, 137] {
        let exit = SandboxExecArgs::new(["sh".into(), "-c".into(), format!("exit {code}")]);
        let job = client.exec_sandbox(&id, &exit).await.unwrap();
        assert_eq!(end(&job.id).await, SandboxJobEnd::exited(code));
        let job = client.get_sandbox_job(&id, &job.id).await.unwrap();
        assert_eq!((job.code, job.reason), (Some(code), None));
    }

    // Past its `timeout_ms`: killed, for its timeout.
    let sleep = SandboxExecArgs::new(["sleep", "300"]);
    let job = client
        .exec_sandbox(&id, &sleep.clone().timeout_ms(500))
        .await
        .unwrap();
    let timed_out = SandboxJobEnd {
        code: 137,
        reason: Some(SandboxJobEndReason::Timeout),
    };
    assert_eq!(end(&job.id).await, timed_out);
    let job = client.get_sandbox_job(&id, &job.id).await.unwrap();
    assert_eq!(
        (job.code, job.reason),
        (Some(137), Some(SandboxJobEndReason::Timeout))
    );

    // `SIGKILL` sent it: the process's own end, as the signal made it.
    let job = client.exec_sandbox(&id, &sleep).await.unwrap();
    client
        .signal_sandbox_job(&id, &job.id, "SIGKILL")
        .await
        .unwrap();
    assert_eq!(end(&job.id).await, SandboxJobEnd::exited(137));

    // Cancelled while its log is read: the log reads on to its end, cancelled, and the job is
    // gone.
    let job = client.exec_sandbox(&id, &sleep).await.unwrap();
    let reading = reqwest::Client::new()
        .get(format!(
            "{}sandboxes/{id}/jobs/{}/events",
            served.url, job.id
        ))
        .bearer_auth(token("tester"))
        .header("virtx-project", "demo")
        .header("accept", "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(reading.status(), StatusCode::OK);
    client.remove_sandbox_job(&id, &job.id).await.unwrap();
    let log = tokio::time::timeout(std::time::Duration::from_secs(60), reading.text())
        .await
        .expect("the log of a job cancelled ends")
        .unwrap();
    assert!(
        log.ends_with("data: {\"end\":{\"code\":137,\"reason\":\"cancelled\"}}\n\n"),
        "{log}"
    );
    assert_eq!(status(client.get_sandbox_job(&id, &job.id).await), 404);
}

/// Boots VMs, as the one above does. A sandbox's machine parked once nothing uses it, and
/// booted again over its disk as it was left for the next job; its ports' connections boot it
/// not.
#[tokio::test]
#[ignore]
async fn an_unused_sandbox_is_parked_and_booted_again_as_it_was_left() {
    let served =
        Served::spawn_with("lifecycle", json!({}), &[("VIRTX_PARK_AFTER_MS", "1000")]).await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("tester");
    let mut args = small("alpine:3.20");
    args.name = Some("work".into());
    args.ports = vec![8000.into()];
    let made = client.create_sandbox(&args).await.unwrap();
    assert!(made.running);
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
    ran(&id, "echo kept > /root/note; sleep 1000 >/dev/null 2>&1 &").await;

    // A job that runs keeps it up, past its park many times over.
    let sleeping = client
        .exec_sandbox(&id, &SandboxExecArgs::new(["sleep", "600"]))
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(4)).await;
    assert!(served.up(&id).await);
    client.remove_sandbox_job(&id, &sleeping.id).await.unwrap();

    // Unused: parked, running still; a connection to its port refused, and booting nothing.
    served.parked(&id).await;
    assert!(client.get_sandbox(&id).await.unwrap().running);
    let url = served.port_url(made.ports[0].url.as_deref().unwrap());
    let mut socket = Served::open(&url, Some("virtx.tcp")).await.unwrap();
    assert!(
        !matches!(socket.next().await, Some(Ok(Message::Binary(_)))),
        "closed, as a port nothing listens on is"
    );
    assert!(!served.up(&id).await);

    // A job boots it: its files as they were, what ran in it gone.
    assert_eq!(
        ran(&id, "cat /root/note; ps | grep -c '[s]leep 1000'").await,
        ["kept", "0"]
    );
    assert!(served.up(&id).await);

    // Deleted: gone, its disk with it.
    client.remove_sandbox(&id).await.unwrap();
    assert_eq!(status(client.get_sandbox(&id).await), 404);
    assert!(!served.demo.join(format!("sandbox/{id}.ext4")).exists());
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_sandbox_is_kept_across_a_server_started_again() {
    let served = Served::spawn("kept").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("tester");
    let mut args = small("alpine:3.20");
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

    // Parked with its server, and known to the next, parked and running still.
    let served = served.again().await;
    let client = served.connect("tester");
    let found = client.find_sandbox("kept").await.unwrap().unwrap();
    assert_eq!((found.id.as_str(), found.running), (made.id.as_str(), true));
    assert!(!served.up(&made.id).await);

    // Booted by a job, as it was left, its port behind the capability it had.
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
    let url = served.port_url(made.ports[0].url.as_deref().unwrap());
    assert!(Served::open(&url, Some("virtx.tcp")).await.is_ok());
    client.remove_sandbox(&made.id).await.unwrap();
    assert_eq!(Served::open(&url, Some("virtx.tcp")).await.err(), Some(404));
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn an_exec_that_accepts_its_log_is_answered_with_it() {
    let served = Served::spawn("exec-streamed").await;
    served.pull_image("alpine:3.20").await;
    let created = served
        .connect("tester")
        .create_sandbox(&small("alpine:3.20"))
        .await
        .unwrap();
    let answer = reqwest::Client::new()
        .post(format!("{}sandboxes/{}/jobs/exec/", served.url, created.id))
        .bearer_auth(token("tester"))
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
        .bearer_auth(token("tester"))
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
    // Kept up between its commands: what a program left running is there for the next.
    let served = Served::spawn_with("exec-background", json!({}), &[KEPT_UP]).await;
    served.pull_image("alpine:3.20").await;
    let created = served
        .connect("tester")
        .create_sandbox(&small("alpine:3.20"))
        .await
        .unwrap();
    let exec = async |cmd: &str| {
        let started = std::time::Instant::now();
        let log = reqwest::Client::new()
            .post(format!("{}sandboxes/{}/jobs/exec/", served.url, created.id))
            .bearer_auth(token("tester"))
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
    let client = served.connect("tester");
    let id = client
        .create_sandbox(&small("alpine:3.20"))
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

/// Boots VMs, as the one above does. Input a process does not read holds back whoever writes
/// it, and nothing else in its sandbox (issue #159): a job's stdin write is answered only once
/// it is written, and holds up no command run beside it.
#[tokio::test]
#[ignore]
async fn input_a_process_does_not_read_holds_back_its_writer_and_nothing_else() {
    let served = Served::spawn("unread-input").await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("tester");
    let id = client
        .create_sandbox(&small("alpine:3.20"))
        .await
        .unwrap()
        .id;
    // What `echo` beside them says, run to its end: answered, whatever they hold.
    let beside = async || {
        let job = client
            .exec_sandbox(&id, &SandboxExecArgs::new(["echo", "beside"]))
            .await
            .unwrap();
        let log: Vec<_> = client
            .sandbox_job_log(&id, &job.id, None)
            .try_collect()
            .await
            .unwrap();
        log.iter()
            .any(|e| e.entry == SandboxJobEntry::Stdout("beside".into()))
    };

    // A job that never reads its stdin: a write past its pipe's buffer is not answered.
    let mut args = SandboxExecArgs::new(["sleep", "600"]);
    args.stdin = true;
    let sleeper = client.exec_sandbox(&id, &args).await.unwrap();
    let writing = {
        let (client, id, job) = (served.connect("tester"), id.clone(), sleeper.id.clone());
        tokio::spawn(async move {
            client
                .write_sandbox_job_stdin(&id, &job, Some("x".repeat(1 << 20)), false)
                .await
        })
    };

    // Let the write get ahead of the sleeper.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let answered = tokio::time::timeout(std::time::Duration::from_secs(60), beside()).await;
    assert_eq!(answered, Ok(true), "a command beside them answered");
    assert!(
        !writing.is_finished(),
        "the unread write is answered only once written"
    );

    // The sleeper ended: its write answered -- refused, its reader gone.
    client
        .signal_sandbox_job(&id, &sleeper.id, "SIGKILL")
        .await
        .unwrap();
    let written = tokio::time::timeout(std::time::Duration::from_secs(60), writing)
        .await
        .expect("answered once its reader is gone")
        .unwrap();
    if written.is_err() {
        assert_eq!(status(written), 409, "refused as ended");
    }
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_fork_bomb_leaves_the_sandbox_answering() {
    // Large enough for more threads than the kernel's pids, as most are.
    answered_beside_a_bomb("fork-bomb", 2048, "f(){ f & f & wait; }; f").await;
}

/// Boots VMs, as the one above does.
#[tokio::test]
#[ignore]
async fn a_fork_bomb_out_of_memory_leaves_the_sandbox_answering() {
    // Each of its processes holding 600 KB of its own: the memory runs out long before the
    // pids do, in a machine of any size.
    let bomb = r#"s=$(head -c 300000 /dev/zero | tr "\0" x); f(){ y="$s$s"; f & f & wait; }; f"#;
    answered_beside_a_bomb("fork-bomb-memory", 1024, bomb).await;
}

/// A command run in a sandbox of `memory_mib` answered beside `bomb`, a script forking without
/// end, as it goes on.
async fn answered_beside_a_bomb(name: &str, memory_mib: u32, bomb: &str) {
    // Kept up between its commands: the bomb left running goes on beside the next.
    let served = Served::spawn_with(name, json!({}), &[KEPT_UP]).await;
    served.pull_image("alpine:3.20").await;
    let client = served.connect("tester");
    let mut args = small("alpine:3.20");
    args.memory_mib = Some(memory_mib);
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
    ran(&format!(
        "sh -c '{bomb}' >/dev/null 2>&1 </dev/null & echo started"
    ))
    .await;
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
    client.remove_sandbox(&id).await.unwrap();
}
