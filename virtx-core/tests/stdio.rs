//! Both ends of stdio, the server a child process: this test binary, run again as one, serving
//! the [`testing`](virtx_core::testing) router.
//!
//! Without libtest's harness, which writes its own lines to stdout: over stdio, stdout is the
//! server's HTTP/2 connection and nothing else.

use futures_util::TryStreamExt;
use hyper::service::Service as _;
use hyper_util::service::TowerToHyperService;
use virtx_core::{
    Client, LogEntry, SandboxCreateArgs, SandboxExecArgs, SandboxJobEnd, SandboxJobEntry,
    serve_stdio, testing::router,
};

/// Set in the environment of this binary when it runs as the server.
const SERVER: &str = "VIRTX_CORE_STDIO_SERVER";
/// A file the server writes as it exits on its own, its stdin closed: one killed writes none.
const EXITED: &str = "VIRTX_CORE_STDIO_EXITED";

fn main() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    if std::env::var_os(SERVER).is_some() {
        let service = TowerToHyperService::new(router());
        let handle = move |request| {
            let service = service.clone();
            async move { service.call(request).await.unwrap_or_else(|e| match e {}) }
        };
        let served = serve_stdio(handle, tokio::io::stdin(), tokio::io::stdout());
        runtime.block_on(served).unwrap();
        if let Some(exited) = std::env::var_os(EXITED) {
            std::fs::write(exited, "exited").unwrap();
        }
        return;
    }
    runtime.block_on(a_child_process_answers_requests_and_logs());
    eprintln!("test result: ok");
}

async fn a_child_process_answers_requests_and_logs() {
    let exited = std::env::temp_dir().join(format!("virtx-stdio-exited-{}", std::process::id()));
    let _ = std::fs::remove_file(&exited);
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command.env(SERVER, "1").env(EXITED, &exited);
    let client = Client::spawn(command).unwrap();

    let volume = client.create_volume(Some("dev")).await.unwrap();
    assert_eq!(volume.name, "dev");

    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new("alpine:3.20"))
        .await
        .unwrap();
    let job = client
        .exec_sandbox(&sandbox.id, &SandboxExecArgs::new(["echo", "hi"]))
        .await
        .unwrap();
    let log: Vec<_> = client
        .sandbox_job_log(&sandbox.id, &job.id, None)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(
        log,
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

    client.close().await;
    // The server exits once its stdin closes, on its own: not killed, as one that does not is.
    assert_eq!(
        std::fs::read_to_string(&exited).ok().as_deref(),
        Some("exited")
    );
    let _ = std::fs::remove_file(&exited);
}
