//! What every test file stands on, as Python's `conftest.py` is: `libvirtx_core`, opened once,
//! and the servers `cargo xtask fixture` serves, as it names them in the environment; and the
//! steps tests share. Each file uses what it needs of it.

#![allow(dead_code)]

use std::{
    env,
    future::Future,
    path::PathBuf,
    pin::pin,
    process::Command,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU32, Ordering},
    },
    task::{Context, Poll, Wake},
    thread::{self, Thread},
};

use virtx::{
    Auth, Client, Event, Events, ImageJobEnd, ImageJobEntry, Sandbox, SandboxExecArgs,
    SandboxJobEntry,
};

/// The image every test that boots a sandbox pulls first.
pub const ALPINE: &str = "alpine:3.20";

/// `$VIRTX_CORE_LIB`, or else `libvirtx_core` built now, with its default features. Not the
/// one in `deps`: that is the dependency's, built without the C ABI.
pub fn library() -> PathBuf {
    static LIBRARY: OnceLock<PathBuf> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            if let Some(path) = env::var_os(virtx::LIBRARY_ENV) {
                return path.into();
            }
            // This binary is in `target/<profile>/deps`.
            let exe = env::current_exe().unwrap();
            let profile = exe.parent().unwrap().parent().unwrap();
            let mut build = Command::new(env!("CARGO"));
            build.args(["build", "-p", "virtx-core", "--lib"]);
            if profile.ends_with("release") {
                build.arg("--release");
            }
            assert!(build.status().unwrap().success(), "building virtx-core");
            profile.join(format!(
                "{}virtx_core{}",
                env::consts::DLL_PREFIX,
                env::consts::DLL_SUFFIX
            ))
        })
        .clone()
}

/// The servers `cargo xtask fixture` serves, as it names them in the environment.
pub struct Fixture {
    pub stdio: String,
    pub url: String,
    pub token: String,
    pub project: String,
}

pub fn fixture() -> &'static Fixture {
    static FIXTURE: OnceLock<Fixture> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let var = |name: &str| {
            env::var(name).unwrap_or_else(|_| {
                panic!("{name} is not set: run these as `cargo xtask test -p virtx`")
            })
        };
        Fixture {
            stdio: var("VIRTX_TEST_STDIO"),
            url: var("VIRTX_TEST_URL"),
            token: var("VIRTX_TEST_TOKEN"),
            project: var("VIRTX_TEST_PROJECT"),
        }
    })
}

/// A client of a `virtx-stdio` of its own, the library opened first.
pub fn client() -> Client {
    virtx::load(library()).unwrap();
    Client::spawn(&fixture().stdio, [] as [&str; 0]).unwrap()
}

/// A client of the fixture's `virtx-server-tcp`, with its token and project, the library
/// opened first.
pub fn tcp_client() -> Client {
    virtx::load(library()).unwrap();
    let Fixture {
        url,
        token,
        project,
        ..
    } = fixture();
    let token = Auth::Bearer {
        token: token.clone(),
    };
    Client::connect(url, token, [("virtx-project", project.as_str())]).unwrap()
}

/// What `cmd` wrote in `sandbox`, a line at a time, stdout and stderr alike, and its exit code.
pub fn run(sandbox: &Sandbox, cmd: &[&str]) -> (Vec<String>, i32) {
    let job = sandbox
        .exec(&SandboxExecArgs::new(cmd.iter().copied()))
        .wait()
        .unwrap();
    let mut lines = Vec::new();
    for entry in entries(job.log(None)) {
        match entry.entry {
            SandboxJobEntry::Stdout(line) | SandboxJobEntry::Stderr(line) => lines.push(line),
            SandboxJobEntry::End(end) => return (lines, end.code),
            SandboxJobEntry::Truncated {} => {}
        }
    }
    panic!("a job's log ends with its end")
}

/// A name no other test gives, of this file or another: each file is a process of its own.
pub fn unique(what: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("test-{what}-{}-{n}", std::process::id())
}

/// The digest the tag `reference` points at, if it is there. A server may answer a reference
/// in full, as `docker.io/library/alpine:3.20` for `alpine:3.20`: each is read as named here.
pub fn tagged(client: &Client, reference: &str) -> Option<String> {
    let familiar = |listed: &str| {
        let Some(rest) = listed.strip_prefix("docker.io/") else {
            return listed.to_owned();
        };
        match rest.strip_prefix("library/") {
            Some(name) if !name.contains('/') => name.to_owned(),
            _ => rest.to_owned(),
        }
    };
    let images = client.list_image().wait().unwrap();
    images
        .into_iter()
        .find(|image| familiar(&image.reference) == reference)
        .map(|image| image.digest)
}

/// [`ALPINE`] on the server, pulled unless it is there already, and its digest. Docker Hub
/// limits the pulls an address makes, so a test that only needs the image takes the one there.
pub fn alpine(client: &Client) -> String {
    // One pull at a time: the second finds the first's, and Docker Hub counts one.
    static PULL: Mutex<()> = Mutex::new(());
    let _pull = PULL.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(digest) = tagged(client, ALPINE) {
        return digest;
    }
    let mut log = client.pull_image(ALPINE, None).log();
    let mut last = None;
    while let Some(entry) = log.wait_next() {
        last = Some(entry.unwrap().entry);
    }
    match last {
        Some(ImageJobEntry::End(ImageJobEnd {
            digest: Some(digest),
            ..
        })) => digest,
        other => panic!("pulling {ALPINE} ended with {other:?}"),
    }
}

/// Each entry of `log`, to its end.
pub fn entries<E: serde::de::DeserializeOwned>(
    mut log: Events<virtx::LogEntry<E>>,
) -> Vec<virtx::LogEntry<E>> {
    let mut entries = Vec::new();
    while let Some(entry) = log.wait_next() {
        entries.push(entry.unwrap());
    }
    entries
}

/// What a stream of the client's own, such as a download, gave last.
pub fn output<T: serde::de::DeserializeOwned>(mut events: Events<Event<T, String>>) -> T {
    while let Some(event) = events.wait_next() {
        if let Event::Output(output) = event.unwrap() {
            return output;
        }
    }
    panic!("a stream ended without its output")
}

/// Polls `future` on this thread until it is ready, parking between wakes.
pub fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(Thread);
    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Arc::new(Unpark(thread::current())).into();
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(output) => return output,
            Poll::Pending => thread::park(),
        }
    }
}
