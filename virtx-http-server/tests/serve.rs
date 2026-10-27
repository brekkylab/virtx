//! The built binary, served over a config of its own and called over HTTP as a client would:
//! each request a `POST` to `/rpc` with a token and a project.

use std::path::PathBuf;

use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};

const NOT_FOUND: i64 = -31001;
const ALREADY_EXISTS: i64 = -31002;
const UNSUPPORTED: i64 = -31004;

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

    // A daemon is not kept yet, and a sandbox of an image nobody filed is refused before it
    // boots.
    assert_eq!(
        served
            .result(
                "sandbox.init",
                json!({"image": {"type": "tag", "tag": "a:b"}, "daemon": true})
            )
            .await,
        Err(UNSUPPORTED)
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
