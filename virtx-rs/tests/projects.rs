//! Who reaches what on `virtx-server-tcp`: the members of a project share its volumes,
//! snapshots, images and sandboxes, and no one else reaches any of them. Against the
//! fixture's server, whose provider is the fixture's own; and against a server of this file's
//! over Dex, where `dex` is installed.

use std::{
    io::{BufRead, BufReader},
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use virtx::{Auth, Client, Files, SandboxCreateArgs, SandboxVolume};

mod common;

use common::ALPINE;

/// The client the tokens of either provider are for.
const CLIENT: &str = "virtx-test";

/// A server whose project `demo` has alice and carol, and `other` bob.
struct Served {
    /// Its base URL, ending in `/`.
    url: String,
    alice: String,
    carol: String,
    bob: String,
    /// Tokens it lets no one in with, by what is wrong with each.
    refused: Vec<(String, String)>,
}

impl Served {
    /// The fixture's, with the tokens it names in the environment.
    fn fixture() -> Served {
        let named = |name: &str| -> Value {
            let value = std::env::var(name).unwrap_or_else(|_| {
                panic!("{name} is not set: run these as `cargo xtask test -p virtx`")
            });
            serde_json::from_str(&value).unwrap()
        };
        let users = named("VIRTX_TEST_USERS");
        let token = |user: &str| users[user].as_str().unwrap().to_owned();
        let refused = named("VIRTX_TEST_REFUSED");
        Served {
            url: common::fixture().url.clone(),
            alice: token("alice"),
            carol: token("carol"),
            bob: token("bob"),
            refused: refused
                .as_object()
                .unwrap()
                .iter()
                .map(|(why, token)| (why.clone(), token.as_str().unwrap().to_owned()))
                .collect(),
        }
    }

    /// A client of `token`'s in `project`.
    fn client(&self, token: &str, project: &str) -> Client {
        let auth = Auth::Bearer {
            token: token.into(),
        };
        Client::connect(&self.url, auth, [("virtx-project", project)]).unwrap()
    }

    /// `method` sent to `path` as `token` in `project`: the status, and the body.
    fn send(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        project: Option<&str>,
        body: &'static str,
    ) -> (u16, String) {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut request = reqwest::blocking::Client::new()
            .request(method, format!("{}{path}", self.url))
            .body(body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(project) = project {
            request = request.header("virtx-project", project);
        }
        let response = request.send().unwrap();
        (response.status().as_u16(), response.text().unwrap())
    }
}

/// The status a call was refused with.
fn refused<T: std::fmt::Debug>(answered: Result<T, virtx::Error>) -> Option<u16> {
    answered.unwrap_err().status()
}

/// Everything this file claims, of `served`.
fn members_share_a_project_and_no_one_else_reaches_it(served: &Served) {
    virtx::load(common::library()).unwrap();
    let (alice, carol, bob) = (&served.alice, &served.carol, &served.bob);

    // A request is let in with a member's token, naming the project.
    let list = |token, project| served.send("GET", "volumes/", token, project, "").0;
    assert_eq!(list(Some(alice), Some("demo")), 200);
    assert_eq!(list(Some(bob), Some("other")), 200);
    assert_eq!(list(None, Some("demo")), 401);
    assert_eq!(list(Some("x"), Some("demo")), 401);
    assert_eq!(list(Some(alice), None), 400);
    assert_eq!(list(Some(bob), Some("demo")), 403);
    assert_eq!(list(Some(alice), Some("other")), 403);
    assert_eq!(list(Some(alice), Some("nobody")), 403);
    for (why, token) in &served.refused {
        assert_eq!(list(Some(token), Some("demo")), 401, "a token {why}");
    }
    // Where to sign in is told to anyone.
    let (status, told) = served.send("GET", "auth", None, None, "");
    let told: Value = serde_json::from_str(&told).unwrap();
    assert_eq!((status, &told["oidc"]["client_id"]), (200, &json!(CLIENT)));

    let alice = served.client(alice, "demo");
    let carol = served.client(carol, "demo");
    let intruder = served.client(bob, "demo");
    let bob = served.client(bob, "other");

    // A volume one member makes is the other's too, and its files.
    let name = common::unique("shared");
    let volume = alice.create_volume(Some(&name)).wait().unwrap();
    volume
        .write_file("a.txt", "from alice", None)
        .wait()
        .unwrap();
    let theirs = carol.get_volume(&name).wait().unwrap();
    assert_eq!(theirs.read_text("a.txt").wait().unwrap(), "from alice");
    theirs
        .write_file("b.txt", "from carol", None)
        .wait()
        .unwrap();
    assert_eq!(volume.read_text("b.txt").wait().unwrap(), "from carol");
    let listed = |client: &Client| {
        let volumes = client.list_volume().wait().unwrap();
        volumes.iter().any(|volume| volume.name() == name)
    };
    assert!(listed(&alice) && listed(&carol));
    // Another project has none of it, under the same name or any; a user of another project,
    // naming this one, is refused before anything is looked for.
    assert!(!listed(&bob));
    assert_eq!(refused(bob.get_volume(&name).wait()), Some(404));
    let elsewhere = bob.volume(volume.info().clone());
    assert_eq!(refused(elsewhere.read_text("a.txt").wait()), Some(404));
    assert_eq!(refused(intruder.get_volume(&name).wait()), Some(403));
    let reached = intruder.volume(volume.info().clone());
    assert_eq!(refused(reached.read_text("a.txt").wait()), Some(403));
    assert_eq!(
        refused(intruder.create_volume(None::<String>).wait()),
        Some(403)
    );

    // The files over WebDAV take the same token and project.
    let file = format!("volumes/{name}/files/c.txt");
    let put = |token, project| {
        served
            .send("PUT", &file, token, Some(project), "over dav")
            .0
    };
    assert_eq!(put(None, "demo"), 401);
    assert_eq!(put(Some(&served.bob), "demo"), 403);
    assert_eq!(put(Some(&served.bob), "other"), 404);
    assert_eq!(put(Some(&served.carol), "demo"), 201);
    let got = served.send("GET", &file, Some(&served.alice), Some("demo"), "");
    assert_eq!(got, (200, "over dav".to_owned()));
    let dir = format!("volumes/{name}/files/");
    let (status, entries) = served.send("PROPFIND", &dir, Some(&served.alice), Some("demo"), "");
    assert!(
        status == 207 && entries.contains("c.txt"),
        "{status} {entries}"
    );

    // A snapshot likewise.
    let kept = common::unique("shared");
    let snapshot = alice.create_snapshot(Some(&kept), Some(1)).wait().unwrap();
    snapshot.write_file("a.txt", "kept", None).wait().unwrap();
    let theirs = carol.get_snapshot(&kept).wait().unwrap();
    assert_eq!(theirs.read_text("a.txt").wait().unwrap(), "kept");
    assert_eq!(refused(bob.get_snapshot(&kept).wait()), Some(404));
    assert_eq!(refused(intruder.get_snapshot(&kept).wait()), Some(403));

    // An image one member pulls is in the project's images, and in its registry.
    let digest = common::alpine(&alice);
    assert_eq!(common::tagged(&carol, ALPINE), Some(digest));
    assert_eq!(common::tagged(&bob, ALPINE), None);
    assert_eq!(refused(intruder.list_image().wait()), Some(403));
    let registry = |path: &str, token, project| {
        let path = format!("images/oci/v2/{path}");
        served.send("GET", &path, token, Some(project), "")
    };
    assert_eq!(registry("", None, "demo").0, 401);
    assert_eq!(registry("", Some(&served.bob), "demo").0, 403);
    assert_eq!(registry("", Some(&served.carol), "demo").0, 200);
    let repositories = |token, project| -> Vec<String> {
        let (status, catalog) = registry("_catalog", Some(token), project);
        assert_eq!(status, 200, "{catalog}");
        let catalog: Value = serde_json::from_str(&catalog).unwrap();
        serde_json::from_value(catalog["repositories"].clone()).unwrap()
    };
    let repos = repositories(&served.carol, "demo");
    let repository = repos.iter().find(|name| name.ends_with("alpine"));
    let repository = repository.unwrap_or_else(|| panic!("no alpine among {repos:?}"));
    let manifest = format!("{repository}/manifests/3.20");
    assert_eq!(registry(&manifest, Some(&served.carol), "demo").0, 200);
    assert_eq!(repositories(&served.bob, "other"), [] as [&str; 0]);
    assert_eq!(registry(&manifest, Some(&served.bob), "other").0, 404);

    // A sandbox one member boots is the other's to see, to run in, and to remove.
    let args = SandboxCreateArgs {
        volumes: vec![SandboxVolume {
            volume: name.clone(),
            path: "/data".into(),
            readonly: false,
        }],
        ..SandboxCreateArgs::new(ALPINE)
    };
    let sandbox = alice.create_sandbox(&args).wait().unwrap();
    let wrote = common::run(
        &sandbox,
        &["sh", "-c", "echo from the sandbox > /data/s.txt"],
    );
    assert_eq!(wrote.1, 0, "{wrote:?}");
    let written = carol.get_volume(&name).wait().unwrap().read_text("s.txt");
    assert_eq!(written.wait().unwrap(), "from the sandbox\n");

    let theirs = carol.get_sandbox(sandbox.id()).wait().unwrap();
    let read = common::run(&theirs, &["cat", "/data/s.txt"]);
    assert_eq!(read, (vec!["from the sandbox".into()], 0));
    let running = |client: &Client| {
        let sandboxes = client.list_sandbox().wait().unwrap();
        sandboxes.iter().any(|listed| listed.id() == sandbox.id())
    };
    assert!(running(&alice) && running(&carol));
    assert!(!running(&bob));
    assert_eq!(refused(bob.get_sandbox(sandbox.id()).wait()), Some(404));
    let elsewhere = bob.sandbox(sandbox.info().clone());
    assert_eq!(refused(elsewhere.remove().wait()), Some(404));
    assert_eq!(refused(intruder.list_sandbox().wait()), Some(403));
    assert_eq!(
        refused(intruder.get_sandbox(sandbox.id()).wait()),
        Some(403)
    );
    let reached = intruder.sandbox(sandbox.info().clone());
    assert_eq!(refused(reached.remove().wait()), Some(403));

    theirs.remove().wait().unwrap();
    assert_eq!(refused(alice.get_sandbox(sandbox.id()).wait()), Some(404));
    carol
        .get_snapshot(&kept)
        .wait()
        .unwrap()
        .remove()
        .wait()
        .unwrap();
    carol
        .get_volume(&name)
        .wait()
        .unwrap()
        .remove()
        .wait()
        .unwrap();
    assert!(!listed(&alice));
}

#[test]
fn members_share_a_project_and_no_one_else_reaches_it_with_the_fixture_s_provider() {
    members_share_a_project_and_no_one_else_reaches_it(&Served::fixture());
}

/// The provider's keys are asked for again for a token of a key the server has not, and not
/// again for each such token.
#[test]
fn a_key_unknown_has_the_provider_s_keys_asked_for_again_once_not_for_each_token() {
    let served = Served::fixture();
    let (_, told) = served.send("GET", "auth", None, None, "");
    let told: Value = serde_json::from_str(&told).unwrap();
    let issuer = told["oidc"]["issuer"].as_str().unwrap();
    let asked = || -> u64 {
        let asked = reqwest::blocking::get(format!("{issuer}/keys/asked")).unwrap();
        asked.text().unwrap().parse().unwrap()
    };
    let unknown = served
        .refused
        .iter()
        .find(|(why, _)| why.contains("has not"));
    let unknown = unknown.unwrap().1.as_str();

    let before = asked();
    for _ in 0..5 {
        let refused = served.send("GET", "volumes/", Some(unknown), Some("demo"), "");
        assert_eq!(refused.0, 401);
    }
    assert!(asked() - before <= 1);
}

/// With tokens Dex issued, for the password of each of its users.
#[test]
#[ignore = "needs dex on PATH"]
fn members_share_a_project_and_no_one_else_reaches_it_with_dex() {
    let own = Own::start();
    members_share_a_project_and_no_one_else_reaches_it(&own.served);
}

/// A process, stopped as `ctrl-c` would when this drops, and killed if it has not ended a
/// while later: a server ends its sandboxes' machines first.
struct Stopped(Child);

impl Drop for Stopped {
    fn drop(&mut self) {
        let _ = Command::new("kill")
            .args(["-INT", &self.0.id().to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(30);
        while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Dex and a `virtx-server-tcp` over it, in a directory of their own, gone when this drops:
/// the server first.
struct Own {
    served: Served,
    _server: Stopped,
    _dex: Stopped,
    dir: PathBuf,
}

impl Drop for Own {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The password of each of Dex's users, and its bcrypt hash.
const PASSWORD: &str = "password";
const PASSWORD_HASH: &str = "$2y$10$AjXiIdb6nVRJp36ZKGSHFOfLQ31qiK6PU5rsSemAMrRddGXf1ojcO";
const SECRET: &str = "virtx-test-secret";

impl Own {
    fn start() -> Own {
        let dir = std::env::temp_dir().join(format!("virtx-dex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (dex, issuer) = dex(&dir);
        let token = |user: &str| password_token(&issuer, &format!("{user}@example.com"));
        let (alice, carol, bob) = (token("alice"), token("carol"), token("bob"));

        let config = json!({
            "listen": "127.0.0.1:0",
            "home": dir.join("home"),
            "oidc": { "issuer": issuer, "client_id": CLIENT },
            "projects": {
                "demo": { "members": ["alice@example.com", "carol@example.com"] },
                "other": { "members": ["bob@example.com"] },
            },
        });
        let path = dir.join("config.json");
        std::fs::write(&path, config.to_string()).unwrap();
        // Beside `virtx-stdio`, as the fixture builds and signs both.
        let binary = Path::new(&common::fixture().stdio)
            .with_file_name(format!("virtx-server-tcp{}", std::env::consts::EXE_SUFFIX));
        let mut server = Command::new(&binary)
            .arg("serve")
            .arg("--config")
            .arg(&path)
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("starting {}: {e}", binary.display()));
        // It says where it listens first; the rest is read only so that it never blocks.
        let mut lines = BufReader::new(server.stderr.take().unwrap()).lines();
        let server = Stopped(server);
        let line = lines.next().expect("it ended before it served").unwrap();
        let url = line.strip_prefix("serving at ").expect(&line).to_owned();
        thread::spawn(move || lines.for_each(drop));

        Own {
            served: Served {
                url,
                alice,
                carol,
                bob,
                refused: Vec::new(),
            },
            _server: server,
            _dex: dex,
            dir,
        }
    }
}

/// Dex on a port of the loopback, over a config in `dir`, with alice, carol and bob, each
/// `<name>@example.com` with [`PASSWORD`]: the process, and its issuer, once it answers.
fn dex(dir: &Path) -> (Stopped, String) {
    let port = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let issuer = format!("http://127.0.0.1:{port}/dex");
    let users: String = ["alice", "carol", "bob"]
        .iter()
        .map(|user| {
            format!(
                "- email: {user}@example.com\n  hash: \"{PASSWORD_HASH}\"\n  username: {user}\n  \
                 userID: {user}\n"
            )
        })
        .collect();
    let config = format!(
        "issuer: {issuer}\n\
         storage:\n  type: memory\n\
         web:\n  http: 127.0.0.1:{port}\n\
         oauth2:\n  passwordConnector: local\n\
         enablePasswordDB: true\n\
         staticClients:\n- id: {CLIENT}\n  name: virtx\n  secret: {SECRET}\n  \
         redirectURIs: ['http://127.0.0.1/callback']\n\
         staticPasswords:\n{users}"
    );
    let path = dir.join("dex.yaml");
    std::fs::write(&path, config).unwrap();
    let process = Command::new("dex")
        .arg("serve")
        .arg(&path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|e| panic!("starting dex, which this test needs on PATH: {e}"));
    let process = Stopped(process);

    let discovery = format!("{issuer}/.well-known/openid-configuration");
    let deadline = Instant::now() + Duration::from_secs(30);
    while reqwest::blocking::get(&discovery).map_or(true, |got| !got.status().is_success()) {
        assert!(
            Instant::now() < deadline,
            "dex did not answer at {discovery}"
        );
        thread::sleep(Duration::from_millis(200));
    }
    (process, issuer)
}

/// An ID token Dex at `issuer` issues `email` for [`PASSWORD`].
fn password_token(issuer: &str, email: &str) -> String {
    let form = format!(
        "grant_type=password&scope=openid+email&username={}&password={PASSWORD}\
         &client_id={CLIENT}&client_secret={SECRET}",
        email.replace('@', "%40")
    );
    let response = reqwest::blocking::Client::new()
        .post(format!("{issuer}/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .unwrap();
    let (status, body) = (response.status(), response.text().unwrap());
    assert!(status.is_success(), "dex answered {status}: {body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    body["id_token"].as_str().expect("an ID token").to_owned()
}
