//! The client against a server over stdio: this binary, spawned with `--serve`, serving
//! `examples/test_server.rs`.

use std::{
    env,
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    process::{Command, ExitCode},
    sync::OnceLock,
};

use futures_util::StreamExt;
use virtx_rs::{Client, Error, Event, ImageBuildOutput, Recipe, SandboxExecOutput};

#[path = "../examples/test_server.rs"]
mod test_server;

fn spawn() -> Client {
    let this = env::current_exe().unwrap();
    Client::spawn(this.to_str().unwrap(), ["--serve"]).unwrap()
}

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

fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(future)
}

fn a_call_answers_its_output() {
    let client = spawn();
    let images = block_on(client.list_image()).unwrap();
    assert_eq!(images[0].reference, "a:latest");
}

fn a_call_answers_the_server_s_error() {
    let client = spawn();
    let error = block_on(client.remove_image("a:latest")).unwrap_err();
    assert!(
        matches!(&error, Error::Call(m) if m.contains("no such image")),
        "{error:?}"
    );
}

fn a_build_streams_its_log_then_its_output() {
    let client = spawn();
    let events: Vec<_> = block_on(
        client
            .build_image(Recipe::new("alpine:3.20"), Some("jq:latest"))
            .collect(),
    );
    let output = ImageBuildOutput {
        reference: "jq:latest".into(),
        digest: "sha256:0".into(),
    };
    assert_eq!(
        events,
        [
            Ok(Event::Progress("pulling alpine:3.20".into())),
            Ok(Event::Output(output))
        ]
    );
}

fn a_failed_build_ends_with_its_error() {
    let client = spawn();
    let mut build = client.build_image(Recipe::new("missing"), None::<String>);
    block_on(async {
        assert!(matches!(build.next().await, Some(Ok(Event::Progress(_)))));
        assert!(matches!(build.next().await, Some(Err(Error::Call(_)))));
        assert!(build.next().await.is_none());
    });
}

fn a_file_reads_back_as_its_bytes() {
    let client = spawn();
    let data = block_on(client.read_volume_file("v", "hello", 1, Some(3))).unwrap();
    assert_eq!(&data[..], b"ell");
}

fn a_file_written_reads_back() {
    let client = spawn();
    block_on(client.write_volume_file("v", "f", None, &b"\0\xffhi"[..])).unwrap();
    let data = block_on(client.read_volume_file("v", "f", 0, None)).unwrap();
    assert_eq!(&data[..], b"\0\xffhi");
}

fn a_thread_without_a_runtime_waits() {
    let client = spawn();
    assert_eq!(client.list_image().wait().unwrap().len(), 1);
    let mut exec = client.exec_sandbox("s", ["echo", "hi"], None);
    assert_eq!(exec.wait_next(), Some(Ok(Event::Progress("hi".into()))));
    assert_eq!(
        exec.wait_next(),
        Some(Ok(Event::Output(SandboxExecOutput { code: 0 })))
    );
    assert_eq!(exec.wait_next(), None);
}

fn a_client_handed_on_as_a_pointer_is_the_same_session() {
    let client = spawn();
    let kept = client.clone();
    let raw = client.into_raw();
    let taken = unsafe { Client::from_raw(raw) }.unwrap();
    assert_eq!(taken.as_raw(), kept.as_raw());
    drop(taken);
    assert_eq!(kept.list_image().wait().unwrap().len(), 1, "still held");
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

fn main() -> ExitCode {
    if env::args().nth(1).as_deref() == Some("--serve") {
        test_server::serve();
        return ExitCode::SUCCESS;
    }
    virtx_rs::load(library()).unwrap();

    macro_rules! tests {
        ($($test:ident),* $(,)?) => {
            [$((stringify!($test), $test as fn())),*]
        };
    }
    let tests = tests![
        a_call_answers_its_output,
        a_call_answers_the_server_s_error,
        a_build_streams_its_log_then_its_output,
        a_failed_build_ends_with_its_error,
        a_file_reads_back_as_its_bytes,
        a_file_written_reads_back,
        a_thread_without_a_runtime_waits,
        a_client_handed_on_as_a_pointer_is_the_same_session,
        spawning_nothing_fails_with_a_message,
        a_second_library_is_refused,
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
