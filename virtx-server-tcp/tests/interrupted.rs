//! The binary asked to stop as its operator stops it: ctrl-c, or `SIGINT`, and on Unix
//! `SIGTERM`, as a service manager sends it; on Windows ctrl-break, the one console event a
//! process group of its own can be sent.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::{Child, Command};

/// How long the server has to start and to stop before the test fails rather than hangs: a
/// guard, not a time it is held to.
const HANG: Duration = Duration::from_secs(60);

/// The binary serving over a home of its own under `name`, in a process group of its own,
/// once it says it serves.
async fn serving(name: &str) -> (Child, PathBuf) {
    let home = std::env::temp_dir().join(format!("virtx-server-tcp-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let config = home.join("config.json");
    let oidc = serde_json::json!({ "issuer": "https://idp.invalid", "client_id": "virtx" });
    let value = serde_json::json!({ "listen": "127.0.0.1:0", "home": home, "oidc": oidc });
    std::fs::write(&config, value.to_string()).unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_virtx-server-tcp"));
    command
        .arg("serve")
        .arg("--config")
        .arg(&config)
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // A process group of its own, which ctrl-break can be sent to alone.
    #[cfg(windows)]
    command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
    let mut server = command.spawn().unwrap();
    let mut said = BufReader::new(server.stderr.take().unwrap()).lines();
    let serving = tokio::time::timeout(HANG, async {
        while let Some(line) = said.next_line().await.unwrap() {
            if line.starts_with("serving at ") {
                return;
            }
        }
        panic!("the server ended before it served");
    });
    serving.await.expect("the server serves");
    (server, home)
}

#[tokio::test]
async fn an_interrupted_server_stops_and_exits_as_it_should() {
    let (mut server, home) = serving("interrupted").await;
    let pid = server.id().unwrap();
    #[cfg(unix)]
    {
        let interrupted = Command::new("kill")
            .args(["-INT", &pid.to_string()])
            .status();
        assert!(interrupted.await.unwrap().success());
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
        // SAFETY: a plain call, naming the group the server leads.
        let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
        assert_ne!(sent, 0, "{}", std::io::Error::last_os_error());
    }
    let exited = tokio::time::timeout(HANG, server.wait()).await;
    let status = exited.expect("the server stops").unwrap();
    assert!(status.success(), "{status:?}");
    let _ = std::fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[tokio::test]
async fn a_terminated_server_stops_and_exits_as_it_should() {
    let (mut server, home) = serving("terminated").await;
    let pid = server.id().unwrap();
    let terminated = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status();
    assert!(terminated.await.unwrap().success());
    let exited = tokio::time::timeout(HANG, server.wait()).await;
    let status = exited.expect("the server stops").unwrap();
    assert!(status.success(), "{status:?}");
    let _ = std::fs::remove_dir_all(&home);
}
