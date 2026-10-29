//! The client over `libvirtx_core`: where no server is needed, and against `virtx-core`'s
//! `files_server` example over stdio for the files.

use std::{
    env,
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    process::{Command, ExitCode},
    sync::OnceLock,
};

use std::{io::Read, net::TcpStream};

use virtx_rs::{Auth, Client, Error, FileRoot, PortForwarder};

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

/// `virtx-core`'s `files_server` example, built now.
fn files_server() -> PathBuf {
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
                "files_server",
            ]);
            if profile.ends_with("release") {
                build.arg("--release");
            }
            assert!(build.status().unwrap().success(), "building files_server");
            profile
                .join("examples")
                .join(format!("files_server{}", env::consts::EXE_SUFFIX))
        })
        .clone()
}

fn the_files_of_a_volume_are_written_listed_moved_and_read_back() {
    let client = Client::spawn(files_server().to_str().unwrap(), [] as [&str; 0]).unwrap();
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

    // An etag to write over, and only over the file that still has it.
    client
        .write_file(&volume, "dir/e.txt", "e", None)
        .unwrap_wait();
    let etag = client
        .stat_file(&volume, "dir/e.txt")
        .wait()
        .unwrap()
        .etag
        .unwrap();
    client
        .write_file(&volume, "dir/e.txt", "ee", Some(&etag))
        .unwrap_wait();
    let stale = client
        .write_file(&volume, "dir/e.txt", "eee", Some(&etag))
        .wait()
        .unwrap_err();
    assert!(stale.to_string().contains("412"), "{stale}");
    client.remove_file(&volume, "dir/e.txt").unwrap_wait();

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
    client.remove_file(&volume, "dir").unwrap_wait();
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
