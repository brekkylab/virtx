//! What every scenario stands on: the server it gets clients of, what it made there to remove
//! once it ends, and the steps scenarios share.

use std::{
    collections::BTreeMap,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
};

use futures_util::{Stream, StreamExt, TryStreamExt};
use http::HeaderMap;
use virtx_core::{
    Auth, Client, Error, Event, ImageJobEnd, ImageJobEntry, Recipe, Sandbox, SandboxCreateArgs,
    SandboxExecArgs, SandboxJobEnd, SandboxJobEntry, Step, testing::router,
};

/// Set in the environment of this binary when it runs as the server.
pub(crate) const SERVE: &str = "VIRTX_E2E_SERVE";

/// The image every scenario that boots a sandbox pulls first.
pub(crate) const ALPINE: &str = "alpine:3.20";

/// The server each scenario gets a client of.
#[derive(Clone)]
pub(crate) enum Server {
    /// The test server, a child process of this binary over stdio.
    Test,
    /// The test server, served on the loopback over TCP.
    TestTcp,
    /// A server spawned from this argv.
    Spawn(Vec<String>),
    /// A server at this URL, and the credentials it wants.
    Url(String, Auth),
}

impl Server {
    pub(crate) fn from_env() -> Server {
        let var = |name| std::env::var(name).ok().filter(|v| !v.is_empty());
        if let Some(command) = var("VIRTX_E2E_SERVER") {
            return Server::Spawn(command.split_whitespace().map(Into::into).collect());
        }
        if let Some(url) = var("VIRTX_E2E_URL") {
            let auth = match var("VIRTX_E2E_TOKEN") {
                Some(token) => Auth::Bearer { token },
                None => Auth::None,
            };
            return Server::Url(url, auth);
        }
        if var("VIRTX_E2E_TCP").is_some() {
            return Server::TestTcp;
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

    /// Whether several clients can reach one server: any but a spawned one, which answers its
    /// own client alone.
    pub(crate) fn is_shared(&self) -> bool {
        !matches!(self, Server::Spawn(_))
    }

    /// A client of a server of its own, or of the one at the URL.
    async fn client(&self) -> Client {
        match self {
            Server::Test => {
                let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
                command.env(SERVE, "1");
                Client::spawn(command).unwrap()
            }
            Server::TestTcp => {
                Client::connect(&serve().await, Auth::None, HeaderMap::new()).unwrap()
            }
            Server::Spawn(argv) => {
                let mut command = tokio::process::Command::new(&argv[0]);
                command.args(&argv[1..]);
                Client::spawn(command).unwrap()
            }
            Server::Url(url, auth) => Client::connect(url, auth.clone(), HeaderMap::new()).unwrap(),
        }
    }

    /// `N` clients of one server: the one at the URL, or else a test server served on the
    /// loopback over TCP, even where the other scenarios have it over stdio, which carries one
    /// client.
    async fn clients<const N: usize>(&self) -> [Client; N] {
        let (url, auth) = match self {
            Server::Test | Server::TestTcp => (serve().await, Auth::None),
            Server::Url(url, auth) => (url.clone(), auth.clone()),
            Server::Spawn(_) => panic!("a spawned server answers its own client alone"),
        };
        std::array::from_fn(|_| Client::connect(&url, auth.clone(), HeaderMap::new()).unwrap())
    }
}

/// The test server, served on the loopback over TCP, and its base URL.
async fn serve() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router()).await });
    url
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
        self.server.clients().await
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
pub(crate) async fn tags(client: &Client) -> BTreeMap<String, String> {
    let listed = client.list_image().await.unwrap();
    listed
        .into_iter()
        .map(|image| (image.reference, image.digest))
        .collect()
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

/// [`ALPINE`] pulled, and its digest.
pub(crate) async fn alpine(client: &Client) -> String {
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
