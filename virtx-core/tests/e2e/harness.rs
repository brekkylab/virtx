//! What every scenario stands on: the server it gets clients of, what it made there to remove
//! once it ends, and the steps scenarios share.

use std::{
    collections::BTreeMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
};

use axum::{
    middleware::{self, Next},
    response::{IntoResponse as _, Response},
};
use futures_util::{Stream, StreamExt, TryStreamExt};
use http::{HeaderMap, StatusCode, header};
use virtx_core::{
    Auth, Client, Error, Event, ImageJobEnd, ImageJobEntry, Recipe, Sandbox, SandboxCreateArgs,
    SandboxExecArgs, SandboxJobEnd, SandboxJobEntry, Step, testing::router,
};

/// Set in the environment of this binary when it runs as the server.
pub(crate) const SERVE: &str = "VIRTX_E2E_SERVE";

/// The image every scenario that boots a sandbox pulls first.
pub(crate) const ALPINE: &str = "alpine:3.20";

/// The bearer token a server over TCP here takes, made up here and no one's: the test server's,
/// and the one user's of the config `virtx-server-tcp` is served with.
const TOKEN: &str = "e2e-token";

/// The header a server over TCP here takes a request's project from, as `virtx-server-tcp`
/// does, and the one project it lets a request reach.
pub(crate) const PROJECT_HEADER: &str = "virtx-project";
const PROJECT: &str = "e2e";

/// The servers a directory of built ones is taken to hold, as `cargo xtask build` names them.
const STDIO_SERVER: &str = "virtx-stdio";
const TCP_SERVER: &str = "virtx-server-tcp";

/// A server clients reach over TCP: its base URL, and the credentials and headers it wants.
#[derive(Clone)]
pub(crate) struct Reach {
    url: String,
    pub(crate) auth: Auth,
    pub(crate) headers: HeaderMap,
}

impl Reach {
    /// A client of it, with what it wants.
    pub(crate) fn client(&self) -> Client {
        self.with(self.auth.clone(), self.headers.clone())
    }

    /// A client of it, with `auth` and `headers` instead.
    pub(crate) fn with(&self, auth: Auth, headers: HeaderMap) -> Client {
        Client::connect(&self.url, auth, headers).unwrap()
    }
}

/// The server each scenario gets a client of.
#[derive(Clone)]
pub(crate) enum Server {
    /// The test server, a child process of this binary over stdio.
    Test,
    /// The test server, served on the loopback over TCP.
    TestTcp,
    /// A server spawned from this argv.
    Spawn(Vec<String>),
    /// The servers built into this directory, which the run takes as [`STDIO_SERVER`],
    /// spawned, and [`TCP_SERVER`], [`Served`](Server::Served).
    Built(PathBuf),
    /// A server at a URL.
    Url(Reach),
    /// [`TCP_SERVER`], served on the loopback for the run, with [`TOKEN`] and [`PROJECT`].
    Served(Reach),
}

impl Server {
    /// The server the environment names, or else the test server over stdio, which the run
    /// takes over TCP as well.
    pub(crate) fn from_env() -> Server {
        let var = |name| std::env::var(name).ok().filter(|v| !v.is_empty());
        if let Some(server) = var("VIRTX_E2E_SERVER") {
            if std::path::Path::new(&server).is_dir() {
                return Server::Built(server.into());
            }
            return Server::Spawn(server.split_whitespace().map(Into::into).collect());
        }
        if let Some(url) = var("VIRTX_E2E_URL") {
            let (auth, headers) = (Auth::None, HeaderMap::new());
            return Server::Url(Reach { url, auth, headers });
        }
        Server::Test
    }

    /// Whether it is the test server, whose pretending a scenario may lean on.
    pub(crate) fn is_test(&self) -> bool {
        matches!(self, Server::Test | Server::TestTcp)
    }

    /// Whether each client has a server process of its own, which closing it ends.
    pub(crate) fn is_spawned(&self) -> bool {
        matches!(self, Server::Test | Server::Spawn(_))
    }

    /// Whether several clients can reach one server: over TCP or at a URL. Over stdio a server
    /// carries its one client.
    pub(crate) fn is_shared(&self) -> bool {
        matches!(self, Server::TestTcp | Server::Url(_) | Server::Served(_))
    }

    /// Whether it refuses a request without valid credentials: a server over TCP served here
    /// does, with ones the harness makes up. A server at a URL is reached with none, and over
    /// stdio a server takes none.
    pub(crate) fn wants_credentials(&self) -> bool {
        matches!(self, Server::TestTcp | Server::Served(_))
    }

    /// Whether a scenario that `needs` it runs against this server, a prune `prune` lets it.
    pub(crate) fn runs(&self, needs: Needs, prune: bool) -> bool {
        match needs {
            Needs::Any => true,
            Needs::Test => self.is_test(),
            Needs::Spawned => self.is_spawned(),
            Needs::Prune => self.is_test() || prune,
            Needs::Shared => self.is_shared(),
            Needs::Credentials => self.wants_credentials(),
        }
    }

    /// A client of a server of its own, or of the one at the URL.
    async fn client(&self) -> Client {
        match self {
            Server::Test => {
                let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
                command.env(SERVE, "1");
                Client::spawn(command).unwrap()
            }
            Server::TestTcp => serve().await.client(),
            Server::Spawn(argv) => {
                let mut command = tokio::process::Command::new(&argv[0]);
                command.args(&argv[1..]);
                Client::spawn(command).unwrap()
            }
            Server::Url(reach) | Server::Served(reach) => reach.client(),
            Server::Built(_) => panic!("a directory's servers are each taken apart"),
        }
    }

    /// One server several clients reach: the one at the URL or served for the run, or a test
    /// server served on the loopback over TCP.
    async fn reach(&self) -> Reach {
        match self {
            Server::Test | Server::TestTcp => serve().await,
            Server::Url(reach) | Server::Served(reach) => reach.clone(),
            Server::Spawn(_) | Server::Built(_) => {
                panic!("a spawned server answers its own client alone")
            }
        }
    }

    /// What the run takes this server over, each with what it adds to a scenario's name: the
    /// test server over stdio and over TCP, the servers built into a directory likewise, and
    /// any other over its own. What is served for the run is kept by the [`Serving`] handed
    /// back, until it is stopped.
    pub(crate) async fn transports(self) -> (Vec<(Option<&'static str>, Server)>, Option<Serving>) {
        match self {
            Server::Test | Server::TestTcp => (
                vec![
                    (Some("stdio"), Server::Test),
                    (Some("tcp"), Server::TestTcp),
                ],
                None,
            ),
            Server::Built(dir) => {
                let built =
                    |name: &str| dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
                let stdio = built(STDIO_SERVER).to_string_lossy().into_owned();
                let serving = Serving::start(&built(TCP_SERVER)).await;
                let tcp = Server::Served(serving.reach.clone());
                (
                    vec![
                        (Some("stdio"), Server::Spawn(vec![stdio])),
                        (Some("tcp"), tcp),
                    ],
                    Some(serving),
                )
            }
            server => (vec![(None, server)], None),
        }
    }
}

/// [`TCP_SERVER`], served for the run over a config made up here: one user, whose token is
/// [`TOKEN`], in one project, [`PROJECT`], kept under `VIRTX_HOME` as the stdio server's are, or
/// else under a home of the run's own, removed once it is stopped.
pub(crate) struct Serving {
    process: tokio::process::Child,
    reach: Reach,
    config: PathBuf,
    home: Option<PathBuf>,
}

impl Serving {
    async fn start(program: &std::path::Path) -> Serving {
        let scratch = std::env::temp_dir().join(format!("virtx-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&scratch);
        std::fs::create_dir_all(&scratch).unwrap();
        let (home, own) = match std::env::var_os("VIRTX_HOME").filter(|h| !h.is_empty()) {
            Some(home) => (PathBuf::from(home), None),
            None => (scratch.join("home"), Some(scratch.clone())),
        };
        let token_sha256 = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(TOKEN));
        let config = serde_json::json!({
            "listen": "127.0.0.1:0",
            "home": home,
            "users": { "e2e": { "token_sha256": token_sha256 } },
            "projects": { PROJECT: { "members": ["e2e"] } },
        });
        let path = scratch.join("config.json");
        std::fs::write(&path, config.to_string()).unwrap();

        let mut process = tokio::process::Command::new(program)
            .arg("serve")
            .arg("--config")
            .arg(&path)
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap_or_else(|e| panic!("spawning {}: {e}", program.display()));
        // Where it listens is the port it was given, which it says first; the rest of what it
        // says, its machines' consoles among it, is read only so that it never blocks.
        let mut said = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(
            process.stderr.take().unwrap(),
        ));
        let line = said
            .next_line()
            .await
            .unwrap()
            .expect("it says where it serves");
        let url = line
            .strip_prefix("serving at ")
            .unwrap_or_else(|| panic!("{} said {line}", program.display()))
            .to_owned();
        tokio::spawn(async move { while let Ok(Some(_)) = said.next_line().await {} });

        let mut headers = HeaderMap::new();
        headers.insert(PROJECT_HEADER, PROJECT.parse().unwrap());
        let auth = Auth::Bearer {
            token: TOKEN.into(),
        };
        let reach = Reach { url, auth, headers };
        Serving {
            process,
            reach,
            config: scratch,
            home: own,
        }
    }

    /// Stop it as `ctrl-c` would, so that it ends its sandboxes' machines, and kill it if it has
    /// not ended a while later; then remove what was made for it.
    pub(crate) async fn stop(mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.process.id() {
            // SAFETY: a signal to a child process this owns and has not reaped.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
        }
        let waited = tokio::time::timeout(std::time::Duration::from_secs(30), self.process.wait());
        if waited.await.is_err() {
            let _ = self.process.kill().await;
        }
        let _ = std::fs::remove_dir_all(self.home.as_ref().unwrap_or(&self.config));
        let _ = std::fs::remove_dir_all(&self.config);
    }
}

/// The test server, served on the loopback over TCP behind [`gate`], and how to reach it.
async fn serve() -> Reach {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let router = router().layer(middleware::from_fn(gate));
    tokio::spawn(async move { axum::serve(listener, router).await });
    let mut headers = HeaderMap::new();
    headers.insert(PROJECT_HEADER, PROJECT.parse().unwrap());
    let auth = Auth::Bearer {
        token: TOKEN.into(),
    };
    Reach { url, auth, headers }
}

/// Lets a request through only with [`TOKEN`] and [`PROJECT`], as a server for many would: one
/// without the token is answered `401`, and one with it reaching another project `403`. So each
/// scenario against it fails where the client leaves either off a request of any kind.
async fn gate(request: axum::extract::Request, next: Next) -> Response {
    let headers = request.headers();
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if token != Some(&format!("Bearer {TOKEN}")) {
        return refuse(StatusCode::UNAUTHORIZED, "unauthorized", "no valid token");
    }
    let project = headers.get(PROJECT_HEADER).and_then(|v| v.to_str().ok());
    if project != Some(PROJECT) {
        return refuse(StatusCode::FORBIDDEN, "forbidden", "not this project's");
    }
    next.run(request).await
}

/// `status`, with the error body `PROTOCOL.md` gives it.
fn refuse(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({ "error": { "code": code, "message": message } });
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// What a scenario made on the server, to remove once it ends.
pub(crate) enum Made {
    Sandbox(String),
    Volume(String),
    Snapshot(String),
    Tag(String),
}

/// One scenario's server, and what the scenario has made there.
#[derive(Clone)]
pub(crate) struct Run {
    server: Server,
    made: Arc<Mutex<Vec<Made>>>,
}

impl Run {
    pub(crate) fn new(server: Server) -> Run {
        Run {
            server,
            made: Arc::default(),
        }
    }

    pub(crate) async fn client(&self) -> Client {
        self.server.client().await
    }

    /// `N` clients of one server, for a scenario that [`Needs::Shared`] it.
    pub(crate) async fn clients<const N: usize>(&self) -> [Client; N] {
        let reach = self.server.reach().await;
        std::array::from_fn(|_| reach.client())
    }

    /// One server several clients reach, for a scenario that [`Needs::Shared`] it or
    /// [`Needs::Credentials`].
    pub(crate) async fn reach(&self) -> Reach {
        self.server.reach().await
    }

    pub(crate) fn is_test(&self) -> bool {
        self.server.is_test()
    }

    /// Keep `made` to remove once the scenario ends. A name is the scenario's own, from
    /// [`unique`], so what is removed is never what was there before it.
    pub(crate) fn made(&self, made: Made) {
        if let Made::Volume(name) | Made::Snapshot(name) | Made::Tag(name) = &made {
            assert!(name.starts_with("e2e-"), "{name} is not a scenario's own");
        }
        self.made.lock().unwrap().push(made);
    }

    pub(crate) async fn create_sandbox(
        &self,
        client: &Client,
        args: &SandboxCreateArgs,
    ) -> Sandbox {
        let sandbox = client.create_sandbox(args).await.unwrap();
        self.made(Made::Sandbox(sandbox.id.clone()));
        sandbox
    }

    /// Remove what the scenario made and did not remove, whether it passed or not: sandboxes
    /// first, since each holds its snapshot and the volumes it mounts. What is gone already
    /// is passed over.
    pub(crate) async fn clean(&self, client: &Client) {
        let mut made = std::mem::take(&mut *self.made.lock().unwrap());
        made.sort_by_key(|made| match made {
            Made::Sandbox(_) => 0,
            Made::Volume(_) => 1,
            Made::Snapshot(_) => 2,
            Made::Tag(_) => 3,
        });
        for made in made {
            let _ = match &made {
                Made::Sandbox(id) => client.remove_sandbox(id).await,
                Made::Volume(name) => client.remove_volume(name).await,
                Made::Snapshot(name) => client.remove_snapshot(name).await,
                Made::Tag(reference) => client.remove_image(reference).await,
            };
        }
    }
}

/// What a scenario needs of the server to run.
#[derive(Clone, Copy)]
pub(crate) enum Needs {
    /// Any server.
    Any,
    /// The test server, whose pretending it leans on.
    Test,
    /// A server process of its own.
    Spawned,
    /// The test server, or one `VIRTX_E2E_ALLOW_PRUNE=1` lets it prune: a prune deletes every
    /// image nothing holds, not only the scenario's.
    Prune,
    /// One server several clients reach, as [`Server::is_shared`] says.
    Shared,
    /// One that refuses a request without valid credentials, as
    /// [`Server::wants_credentials`] says.
    Credentials,
}

/// A scenario: its name, `<group>::<scenario>`, what it needs of the server, and itself.
pub(crate) struct Scenario {
    pub(crate) name: &'static str,
    pub(crate) needs: Needs,
    pub(crate) run: fn(Run) -> Pin<Box<dyn Future<Output = ()>>>,
}

/// A group's scenarios, as its `SCENARIOS`: `scenarios![images: Any a_pull, ...]`.
macro_rules! scenarios {
    ($group:ident: $($needs:ident $scenario:ident),* $(,)?) => {
        pub(crate) const SCENARIOS: &[crate::harness::Scenario] = &[$(
            crate::harness::Scenario {
                name: concat!(stringify!($group), "::", stringify!($scenario)),
                needs: crate::harness::Needs::$needs,
                run: |run| -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> {
                    Box::pin($scenario(run))
                },
            }
        ),*];
    };
}

/// A name no other scenario, or run, gives: `e2e-<what>-<8 hex digits>`.
pub(crate) fn unique(what: &str) -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("e2e-{what}-{}", &id[..8])
}

/// The status and the `code` `result` was refused with.
pub(crate) fn refused<T: std::fmt::Debug>(result: Result<T, Error>) -> (u16, String) {
    match result {
        Err(Error::Status(status, message)) => {
            let code = message.split(':').next().unwrap_or("").to_owned();
            (status, code)
        }
        other => panic!("not a refusal: {other:?}"),
    }
}

/// Every tag on the server's images, as `name:tag`, with the digest it points at.
///
/// A server may answer a reference in full, as `docker.io/library/alpine:3.20` for
/// `alpine:3.20`, which the protocol leaves to it: each is read back as the scenarios name it.
pub(crate) async fn tags(client: &Client) -> BTreeMap<String, String> {
    let listed = client.list_image().await.unwrap();
    listed
        .into_iter()
        .map(|image| (familiar(&image.reference), image.digest))
        .collect()
}

/// `reference` without the registry and namespace Docker Hub's names are filled in with:
/// `docker.io/library/alpine:3.20` is `alpine:3.20`, and `docker.io/user/app:1` is `user/app:1`.
fn familiar(reference: &str) -> String {
    let Some(rest) = reference.strip_prefix("docker.io/") else {
        return reference.to_owned();
    };
    match rest.strip_prefix("library/") {
        Some(name) if !name.contains('/') => name.to_owned(),
        _ => rest.to_owned(),
    }
}

/// How the image job `id` ended, once it has.
pub(crate) async fn image_end(client: &Client, id: &str) -> ImageJobEnd {
    let log: Vec<_> = client.image_job_log(id, None).try_collect().await.unwrap();
    match log.into_iter().last().map(|entry| entry.entry) {
        Some(ImageJobEntry::End(end)) => end,
        other => panic!("the log of {id} ends with {other:?}"),
    }
}

/// The digest the image job `id` ended with, which it fails without.
pub(crate) async fn image_digest(client: &Client, id: &str) -> String {
    let end = image_end(client, id).await;
    end.digest
        .unwrap_or_else(|| panic!("{id} failed: {:?}", end.error))
}

/// [`ALPINE`] on the server, pulled unless it is there already, and its digest.
///
/// A real server keeps its images from one scenario to the next, and Docker Hub limits the
/// pulls an address makes without credentials, so a scenario that only needs the image takes
/// the one there. Those that are of a pull pull it themselves.
pub(crate) async fn alpine(client: &Client) -> String {
    if let Some(digest) = tags(client).await.remove(ALPINE) {
        return digest;
    }
    let job = client.pull_image(ALPINE, None).await.unwrap();
    image_digest(client, &job.id).await
}

/// `reference` built from [`ALPINE`] with a step of its own, so its image is no other's, and
/// its digest.
pub(crate) async fn build(run: &Run, client: &Client, reference: &str) -> String {
    run.made(Made::Tag(reference.into()));
    let recipe = Recipe::new(ALPINE).step(Step::run(format!("echo {reference} > /built")));
    let job = client.build_image(recipe, reference).await.unwrap();
    image_digest(client, &job.id).await
}

/// What a stream of the client's own, such as a download, gave last.
pub(crate) async fn output<T>(
    events: impl Stream<Item = Result<Event<T, String>, Error>> + Unpin,
) -> T {
    let mut events = events;
    while let Some(event) = events.next().await {
        if let Event::Output(output) = event.unwrap() {
            return output;
        }
    }
    panic!("a stream ended without its output")
}

/// A path in the system's temporary directory no other scenario uses.
pub(crate) fn temporary(name: &str) -> PathBuf {
    std::env::temp_dir().join(unique(name))
}

pub(crate) fn exec(cmd: &[&str]) -> SandboxExecArgs {
    SandboxExecArgs::new(cmd.iter().copied())
}

/// A `sleep` that outlives no scenario: killed at its timeout if the scenario dies first.
pub(crate) fn sleep() -> SandboxExecArgs {
    SandboxExecArgs {
        timeout_ms: Some(120_000),
        ..exec(&["sleep", "1000"])
    }
}

/// The entries of the job `job`'s log in `sandbox`, to its end.
pub(crate) async fn sandbox_log(client: &Client, sandbox: &str, job: &str) -> Vec<SandboxJobEntry> {
    client
        .sandbox_job_log(sandbox, job, None)
        .map_ok(|entry| entry.entry)
        .try_collect()
        .await
        .unwrap()
}

pub(crate) fn end(code: i32) -> SandboxJobEntry {
    SandboxJobEntry::End(SandboxJobEnd { code })
}

/// A sandbox made from `args`, of [`ALPINE`] pulled first.
pub(crate) async fn sandbox(run: &Run, client: &Client, args: SandboxCreateArgs) -> String {
    alpine(client).await;
    run.create_sandbox(client, &args).await.id
}
