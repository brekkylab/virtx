//! The client over `libvirtx_core`: where no server is needed, and against `virtx-core`'s
//! `test_server` example over stdio.

use std::{
    env,
    future::{self, Future},
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    pin::{Pin, pin},
    process::{Command, ExitCode},
    sync::{Arc, OnceLock},
    task::{Context, Poll, Wake},
    thread::{self, Thread},
};

use futures_core::Stream;

use std::{io::Read, net::TcpStream};

use virtx_rs::{
    Auth, Client, Error, Event, Events, FileRoot, ImageJobEnd, ImageJobEntry, ImageJobKind,
    LogEntry, MountSource, MountType, PortForwarder, Recipe, RegistryAuth, SandboxCreateArgs,
    SandboxExecArgs, SandboxJobEnd, SandboxJobEntry, SnapshotListOutputItem,
    VolumeMountListOutputItem,
};

/// `$VIRTX_CORE_LIB`, or else `libvirtx_core` built now, with its default features. Not the
/// one in `deps`: that is the dependency's, built without the C ABI.
fn library() -> PathBuf {
    static LIBRARY: OnceLock<PathBuf> = OnceLock::new();
    LIBRARY
        .get_or_init(|| {
            if let Some(path) = env::var_os(virtx_rs::LIBRARY_ENV) {
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
            let name = if cfg!(windows) {
                "virtx_core.dll"
            } else if cfg!(target_os = "macos") {
                "libvirtx_core.dylib"
            } else {
                "libvirtx_core.so"
            };
            profile.join(name)
        })
        .clone()
}

/// `virtx-core`'s `test_server` example, built now.
fn test_server() -> PathBuf {
    static SERVER: OnceLock<PathBuf> = OnceLock::new();
    SERVER
        .get_or_init(|| {
            let exe = env::current_exe().unwrap();
            let profile = exe.parent().unwrap().parent().unwrap();
            let mut build = Command::new(env!("CARGO"));
            build.args([
                "build",
                "-p",
                "virtx-core",
                "--features",
                "testing",
                "--example",
                "test_server",
            ]);
            if profile.ends_with("release") {
                build.arg("--release");
            }
            assert!(build.status().unwrap().success(), "building test_server");
            profile
                .join("examples")
                .join(format!("test_server{}", env::consts::EXE_SUFFIX))
        })
        .clone()
}

fn the_files_of_a_volume_are_written_listed_moved_and_read_back() {
    let client = Client::spawn(test_server().to_str().unwrap(), [] as [&str; 0]).unwrap();
    let volume = FileRoot::Volume("v".into());
    client.create_dir(&volume, "dir").unwrap_wait();
    client
        .write_file(&volume, "dir/a.txt", "hello world", None)
        .unwrap_wait();
    client.create_dir_all(&volume, "deep/er/dir").unwrap_wait();
    client.create_dir_all(&volume, "deep/er/dir").unwrap_wait();
    assert!(
        client
            .stat_file(&volume, "deep/er/dir")
            .wait()
            .unwrap()
            .is_dir
    );

    let metadata = client.stat_file(&volume, "dir/a.txt").wait().unwrap();
    assert!(!metadata.is_dir && metadata.size == 11, "{metadata:?}");
    let entries = client.read_dir(&volume, "dir").wait().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "a.txt");
    assert_eq!(entries[0].metadata, metadata);

    let read = client
        .read_file(&volume, "dir/a.txt", 0, None)
        .wait()
        .unwrap();
    assert_eq!(read, b"hello world");
    let read = client
        .read_file(&volume, "dir/a.txt", 6, Some(3))
        .wait()
        .unwrap();
    assert_eq!(read, b"wor");

    // Text, an etag to write over, and what is there, told without an error.
    client
        .write_file(&volume, "dir/t.txt", "텍스트", None)
        .unwrap_wait();
    assert_eq!(
        client.read_text(&volume, "dir/t.txt").wait().unwrap(),
        "텍스트"
    );
    let etag = client
        .stat_file(&volume, "dir/t.txt")
        .wait()
        .unwrap()
        .etag
        .unwrap();
    client
        .write_file(&volume, "dir/t.txt", "text 2", Some(&etag))
        .unwrap_wait();
    let stale = client
        .write_file(&volume, "dir/t.txt", "text 3", Some(&etag))
        .wait()
        .unwrap_err();
    assert!(stale.to_string().contains("412"), "{stale}");
    assert!(client.exists(&volume, "dir/t.txt").wait().unwrap());
    assert!(client.is_file(&volume, "dir/t.txt").wait().unwrap());
    assert!(!client.is_dir(&volume, "dir/t.txt").wait().unwrap());
    assert!(client.is_dir(&volume, "dir").wait().unwrap());
    client
        .remove_file(&volume, "dir/t.txt", false)
        .unwrap_wait();
    assert!(!client.exists(&volume, "dir/t.txt").wait().unwrap());
    client.remove_file(&volume, "dir/t.txt", true).unwrap_wait();

    client
        .rename_file(&volume, "dir/a.txt", "b.txt", false)
        .unwrap_wait();
    client
        .copy_file(&volume, "b.txt", "c.txt", false)
        .unwrap_wait();
    let refused = client
        .copy_file(&volume, "b.txt", "c.txt", false)
        .wait()
        .unwrap_err();
    assert!(refused.to_string().contains("412"), "{refused}");
    client.remove_file(&volume, "dir", false).unwrap_wait();
    let gone = client.stat_file(&volume, "dir").wait().unwrap_err();
    assert!(gone.to_string().contains("404"), "{gone}");
    assert_eq!(
        client.read_file(&volume, "c.txt", 0, None).wait().unwrap(),
        b"hello world"
    );

    let locked = FileRoot::Snapshot("locked".into());
    let refused = client.read_dir(&locked, "").wait().unwrap_err();
    assert!(refused.to_string().contains("423"), "{refused}");
}

fn a_snapshot_is_created_got_and_removed() {
    let client = Client::spawn(test_server().to_str().unwrap(), [] as [&str; 0]).unwrap();
    let created = client.create_snapshot(Some("t"), None).wait().unwrap();
    assert_eq!(
        (created.name.as_str(), created.in_use),
        ("t", false),
        "{created:?}"
    );
    let got: SnapshotListOutputItem = client.get_snapshot("t").wait().unwrap();
    assert_eq!(got, created);
    client.remove_snapshot("t").unwrap_wait();
    let gone = client.get_snapshot("t").wait().unwrap_err();
    assert!(gone.to_string().contains("404"), "{gone}");
}

fn a_process_run_in_a_sandbox_is_read_from_its_log() {
    let client = Client::spawn(test_server().to_str().unwrap(), [] as [&str; 0]).unwrap();
    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .wait()
        .unwrap();
    assert_eq!(
        client.get_sandbox(&sandbox.id).wait().unwrap().id,
        sandbox.id
    );

    let job = client
        .exec_sandbox(&sandbox.id, &SandboxExecArgs::new(["echo", "hi"]))
        .wait()
        .unwrap();
    let mut log = client.sandbox_job_log(&sandbox.id, &job.id, None);
    let mut entries = Vec::new();
    while let Some(entry) = log.wait_next() {
        entries.push(entry.unwrap());
    }
    assert_eq!(
        entries,
        [
            LogEntry {
                id: 1,
                entry: SandboxJobEntry::Stdout("hi".into()),
            },
            LogEntry {
                id: 2,
                entry: SandboxJobEntry::End(SandboxJobEnd { code: 0 }),
            },
        ]
    );
    let jobs = client.list_sandbox_job(&sandbox.id).wait().unwrap();
    assert!(
        jobs.iter().any(|j| j.id == job.id && j.code == Some(0)),
        "{jobs:?}"
    );

    // `cat` echoes its stdin until it is closed.
    let mut args = SandboxExecArgs::new(["cat"]);
    args.stdin = true;
    let cat = client.exec_sandbox(&sandbox.id, &args).wait().unwrap();
    client
        .write_sandbox_job_stdin(&sandbox.id, &cat.id, Some("a\n"), true)
        .unwrap_wait();
    let mut log = client.sandbox_job_log(&sandbox.id, &cat.id, None);
    let first = log.wait_next().unwrap().unwrap();
    assert_eq!(first.entry, SandboxJobEntry::Stdout("a".into()));

    client.remove_sandbox(&sandbox.id).unwrap_wait();
    let gone = client.get_sandbox(&sandbox.id).wait().unwrap_err();
    assert!(gone.to_string().contains("404"), "{gone}");
}

fn an_image_build_is_a_job_with_a_log() {
    let client = Client::spawn(test_server().to_str().unwrap(), [] as [&str; 0]).unwrap();
    let job = client
        .build_image(Recipe::new("alpine:3.20"), "t:latest")
        .wait()
        .unwrap();
    assert_eq!(job.kind, ImageJobKind::Build);
    let built = job.digest.clone().unwrap();
    let mut log = client.image_job_log(&job.id, None);
    let mut entries = Vec::new();
    while let Some(entry) = log.wait_next() {
        entries.push(entry.unwrap().entry);
    }
    assert_eq!(
        entries,
        [
            ImageJobEntry::Log("pulling alpine:3.20".into()),
            ImageJobEntry::End(ImageJobEnd {
                digest: Some(built.clone()),
                error: None,
            }),
        ]
    );
    let done = client.get_image_job(&job.id).wait().unwrap();
    assert_eq!(done.digest, Some(built), "{done:?}");
    assert!(
        client
            .list_image_job()
            .wait()
            .unwrap()
            .iter()
            .any(|j| j.id == job.id)
    );
    client.remove_image_job(&job.id).unwrap_wait();
    assert!(client.get_image_job(&job.id).wait().is_err());
}

fn a_started_job_is_read_as_its_log() {
    let client = Client::spawn(test_server().to_str().unwrap(), [] as [&str; 0]).unwrap();
    let mut log = client
        .build_image(Recipe::new("alpine:3.20"), "t:latest")
        .log();
    let mut entries = Vec::new();
    while let Some(entry) = log.wait_next() {
        entries.push(entry.unwrap().entry);
    }
    let listed = client.list_image().wait().unwrap();
    let built = listed
        .iter()
        .find(|image| image.reference == "t:latest")
        .map(|image| image.digest.clone());
    assert!(built.is_some(), "{listed:?}");
    assert_eq!(
        entries,
        [
            ImageJobEntry::Log("pulling alpine:3.20".into()),
            ImageJobEntry::End(ImageJobEnd {
                digest: built,
                error: None,
            }),
        ]
    );

    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .wait()
        .unwrap();
    let mut log = client
        .exec_sandbox(&sandbox.id, &SandboxExecArgs::new(["echo", "hi"]))
        .log();
    assert!(log.job().is_none());
    let mut entries = Vec::new();
    while let Some(entry) = log.wait_next() {
        entries.push(entry.unwrap().entry);
        // The job is the log's once started, to act on mid-stream.
        assert_eq!(log.job().unwrap().cmd, ["echo", "hi"]);
    }
    assert_eq!(
        entries,
        [
            SandboxJobEntry::Stdout("hi".into()),
            SandboxJobEntry::End(SandboxJobEnd { code: 0 }),
        ]
    );

    // Polled, as a runtime would.
    let job =
        block_on(client.exec_sandbox(&sandbox.id, &SandboxExecArgs::new(["echo", "a"]))).unwrap();
    assert_eq!(job.cmd, ["echo", "a"]);
    let mut log = client
        .exec_sandbox(&sandbox.id, &SandboxExecArgs::new(["echo", "b"]))
        .log();
    let mut entries = Vec::new();
    while let Some(entry) = block_on(future::poll_fn(|cx| Pin::new(&mut log).poll_next(cx))) {
        entries.push(entry.unwrap().entry);
    }
    assert_eq!(log.job().unwrap().cmd, ["echo", "b"]);
    assert_eq!(
        entries,
        [
            SandboxJobEntry::Stdout("b".into()),
            SandboxJobEntry::End(SandboxJobEnd { code: 0 }),
        ]
    );

    // A start refused is the log's one item.
    let mut log = client
        .exec_sandbox(&sandbox.id, &SandboxExecArgs::new(["nonexistent"]))
        .log();
    let refused = log.wait_next().unwrap().unwrap_err();
    assert!(refused.to_string().contains("422"), "{refused}");
    assert!(log.wait_next().is_none());
    assert!(log.job().is_none());
}

/// Polls `future` on this thread until it is ready, parking between wakes.
fn block_on<F: Future>(future: F) -> F::Output {
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

/// Waits on a call that answers nothing, panicking on an error.
trait UnwrapWait {
    fn unwrap_wait(self);
}

impl UnwrapWait for virtx_rs::Call<()> {
    fn unwrap_wait(self) {
        self.wait().unwrap();
    }
}

fn spawning_nothing_fails_with_a_message() {
    let error = Client::spawn("/nonexistent/virtx-server", [] as [&str; 0])
        .err()
        .unwrap();
    assert!(
        matches!(&error, Error::Call(m) if m.contains("starting the server")),
        "{error:?}"
    );
}

fn a_second_library_is_refused() {
    let error = virtx_rs::load("/nonexistent/libvirtx_core").unwrap_err();
    assert!(matches!(error, Error::Library(_)));
    virtx_rs::load(library()).unwrap();
}

fn a_header_that_cannot_be_sent_is_refused_at_connect() {
    let Err(error) = Client::connect("http://127.0.0.1:9/", Auth::None, [("not a name", "x")])
    else {
        panic!("connected");
    };
    assert!(error.to_string().contains("not a name"), "{error}");
}

fn a_token_does_not_go_in_the_clear_to_another_machine() {
    let token = Auth::Bearer { token: "t".into() };
    let Err(error) = Client::connect(
        "http://example.com/",
        token.clone(),
        [] as [(&str, &str); 0],
    ) else {
        panic!("connected");
    };
    assert!(error.to_string().contains("in the clear"), "{error}");
    let client = Client::connect(
        "http://127.0.0.1:9/",
        token.clone(),
        [] as [(&str, &str); 0],
    )
    .unwrap();
    assert!(client.set_auth(Auth::None).is_ok());
    assert!(client.set_auth(token).is_ok());
}

fn a_header_that_cannot_be_sent_is_refused_when_set() {
    let client =
        Client::connect("http://127.0.0.1:9/", Auth::None, [] as [(&str, &str); 0]).unwrap();
    assert!(client.set_headers([("authorization", "Bearer x")]).is_ok());
    let Err(error) = client.set_headers([("not a name", "x")]) else {
        panic!("set");
    };
    assert!(error.to_string().contains("not a name"), "{error}");
}

/// The digest the tag `reference` points at, if it is there.
fn tagged(client: &Client, reference: &str) -> Option<String> {
    let images = client.list_image().wait().unwrap();
    images
        .into_iter()
        .find(|image| image.reference == reference)
        .map(|image| image.digest)
}

/// What a stream of the client's own, such as a download, gave last.
fn output<T: serde::de::DeserializeOwned>(mut events: Events<Event<T, String>>) -> T {
    while let Some(event) = events.wait_next() {
        if let Event::Output(output) = event.unwrap() {
            return output;
        }
    }
    panic!("a stream ended without its output")
}

fn an_image_is_tagged_downloaded_removed_and_uploaded_back() {
    let client = Client::spawn(test_server().to_str().unwrap(), [] as [&str; 0]).unwrap();
    let digest = tagged(&client, "alpine:3.20").unwrap();
    client.tag_image("alpine:3.20", "copy:1").unwrap_wait();
    assert_eq!(tagged(&client, "copy:1").as_ref(), Some(&digest));

    let path = env::temp_dir().join(format!("virtx-rs-{}-copy.tar", std::process::id()));
    assert_eq!(
        output(client.download_image("copy:1", &path)).digest,
        digest
    );
    client.remove_image("copy:1").unwrap_wait();
    assert_eq!(tagged(&client, "copy:1"), None);
    assert_eq!(output(client.upload_image(&path, "copy:1")).digest, digest);
    assert_eq!(tagged(&client, "copy:1").as_ref(), Some(&digest));

    client.untag_image("copy:1").unwrap_wait();
    assert_eq!(tagged(&client, "copy:1"), None);
    std::fs::remove_file(&path).unwrap();
}

fn a_push_ends_with_the_digest_its_reference_points_at() {
    let client = Client::spawn(test_server().to_str().unwrap(), [] as [&str; 0]).unwrap();
    let digest = tagged(&client, "alpine:3.20");
    let auth = RegistryAuth::Bearer {
        token: "secret".into(),
    };
    let mut log = client.push_image("alpine:3.20", Some(auth)).log();
    let mut entries = Vec::new();
    while let Some(entry) = log.wait_next() {
        entries.push(entry.unwrap().entry);
    }
    assert_eq!(
        entries,
        [
            ImageJobEntry::Log("pushing alpine:3.20 as bearer".into()),
            ImageJobEntry::End(ImageJobEnd {
                digest,
                error: None,
            }),
        ]
    );
    let missing = client.push_image("nothing:here", None).wait().unwrap();
    assert_eq!(missing.error.as_deref(), Some("no image nothing:here"));
}

fn a_mount_is_made_listed_and_removed() {
    let client = Client::spawn(test_server().to_str().unwrap(), [] as [&str; 0]).unwrap();
    let source = || MountSource::Notion {
        api_key: "secret".into(),
    };
    let mount = client
        .create_volume_mount("v", "notion", source())
        .wait()
        .unwrap();
    assert_eq!(
        mount,
        VolumeMountListOutputItem {
            path: "notion".into(),
            mount_type: MountType::Notion,
        }
    );
    assert_eq!(client.list_volume_mount("v").wait().unwrap(), [mount]);
    let again = client
        .create_volume_mount("v", "notion", source())
        .wait()
        .unwrap_err();
    assert!(again.to_string().contains("409"), "{again}");
    client.remove_volume_mount("v", "notion").unwrap_wait();
    assert!(client.list_volume_mount("v").wait().unwrap().is_empty());
}

fn a_forwarder_is_on_the_loopback_until_it_is_dropped() {
    // Nothing listens there: each connection to the forwarder is closed.
    let client =
        Client::connect("http://127.0.0.1:9/", Auth::None, [] as [(&str, &str); 0]).unwrap();
    let forwarder = PortForwarder::open(&client, "ports/abc").unwrap();
    let address = forwarder.address();
    assert!(address.ip().is_loopback());
    let mut read = Vec::new();
    let mut stream = TcpStream::connect(address).unwrap();
    assert_eq!(stream.read_to_end(&mut read).unwrap(), 0);

    drop(forwarder);
    drop(client);
    assert!(TcpStream::connect(address).is_err());
}

fn main() -> ExitCode {
    // Built before the library is opened: building it relinks `libvirtx_core`, and Windows
    // cannot write a library a process has open.
    test_server();
    virtx_rs::load(library()).unwrap();

    macro_rules! tests {
        ($($test:ident),* $(,)?) => {
            [$((stringify!($test), $test as fn())),*]
        };
    }
    let tests = tests![
        spawning_nothing_fails_with_a_message,
        a_header_that_cannot_be_sent_is_refused_at_connect,
        a_header_that_cannot_be_sent_is_refused_when_set,
        a_token_does_not_go_in_the_clear_to_another_machine,
        a_second_library_is_refused,
        a_forwarder_is_on_the_loopback_until_it_is_dropped,
        the_files_of_a_volume_are_written_listed_moved_and_read_back,
        a_snapshot_is_created_got_and_removed,
        a_process_run_in_a_sandbox_is_read_from_its_log,
        an_image_build_is_a_job_with_a_log,
        a_started_job_is_read_as_its_log,
        an_image_is_tagged_downloaded_removed_and_uploaded_back,
        a_push_ends_with_the_digest_its_reference_points_at,
        a_mount_is_made_listed_and_removed,
    ];
    let filter = env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let mut failed = 0;
    for (name, test) in tests {
        if filter
            .as_deref()
            .is_some_and(|filter| !name.contains(filter))
        {
            continue;
        }
        let passed = panic::catch_unwind(AssertUnwindSafe(test)).is_ok();
        println!("test {name} ... {}", if passed { "ok" } else { "FAILED" });
        failed += usize::from(!passed);
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
