//! End to end: the typed client against a server across a real transport, each scenario a flow
//! of requests over images, snapshots, volumes, sandboxes and their jobs.
//!
//! The server is the [`testing`](virtx_core::testing) one: by default this test binary, run
//! again as a child process over stdio, or with `VIRTX_E2E_TCP=1` served on the loopback over
//! TCP. `VIRTX_E2E_SERVER` names a server to spawn instead, its arguments split on whitespace,
//! and `VIRTX_E2E_URL` one to connect to, with `VIRTX_E2E_TOKEN` its bearer token. Against those
//! only the scenarios every server passes run: they assert nothing `PROTOCOL.md` leaves to the
//! server, run no command but `echo`, `cat` and `sleep`, and name what they make uniquely. The
//! scenarios of several clients share one server: the one at the URL, or else a test server
//! over TCP; a spawned server carries one client, so they are passed by.
//!
//! Against another server, what a scenario made is removed once it ends, passed or not, and a
//! `sleep` it started is killed at its timeout if it is not. Each name it removes is its own,
//! `e2e-...`; the one thing it changes that is not is the tag `alpine:3.20`, which a pull moves
//! to what its registry has now. A prune deletes every image nothing holds, so the scenario that
//! prunes runs against another server only with `VIRTX_E2E_ALLOW_PRUNE=1`.
//!
//! The scenarios are grouped by what they go through, a module each, and named
//! `<group>::<scenario>`. Without libtest's harness, which writes its own lines to stdout: over
//! stdio, stdout is the server's HTTP/2 connection and nothing else. An argument runs the
//! scenarios whose names hold it, `images::` a group.

use std::{
    panic::{self, AssertUnwindSafe},
    process::ExitCode,
    time::Duration,
};

use hyper::service::Service as _;
use hyper_util::service::TowerToHyperService;
use virtx_core::{serve_stdio, testing::router};

#[macro_use]
mod harness;
mod images;
mod processes;
mod sandboxes;
mod shared;
mod storage;

use harness::{Needs, Run, SERVE, Server};

fn main() -> ExitCode {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    if std::env::var_os(SERVE).is_some() {
        let service = TowerToHyperService::new(router());
        let handle = move |request| {
            let service = service.clone();
            async move { service.call(request).await.unwrap_or_else(|e| match e {}) }
        };
        let served = serve_stdio(handle, tokio::io::stdin(), tokio::io::stdout());
        runtime.block_on(served).unwrap();
        return ExitCode::SUCCESS;
    }

    let server = Server::from_env();
    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|arg| !arg.starts_with('-'))
        .collect();
    // A real server builds and pulls for real.
    let limit = Duration::from_secs(if server.is_test() { 30 } else { 900 });
    let groups = [
        images::SCENARIOS,
        storage::SCENARIOS,
        processes::SCENARIOS,
        sandboxes::SCENARIOS,
        shared::SCENARIOS,
    ];

    let prune = std::env::var("VIRTX_E2E_ALLOW_PRUNE").is_ok_and(|v| v == "1");
    let (mut passed, mut ignored, mut failed) = (0, 0, Vec::new());
    for scenario in groups.into_iter().flatten() {
        let name = scenario.name;
        if !filters.is_empty() && !filters.iter().any(|f| name.contains(f.as_str())) {
            continue;
        }
        let runs = match scenario.needs {
            Needs::Any => true,
            Needs::Test => server.is_test(),
            Needs::Spawned => server.is_spawned(),
            Needs::Prune => server.is_test() || prune,
            Needs::Shared => server.is_shared(),
        };
        if !runs {
            eprintln!("test {name} ... ignored");
            ignored += 1;
            continue;
        }
        let run = Run::new(server.clone());
        let ran = panic::catch_unwind(AssertUnwindSafe(|| {
            runtime
                .block_on(async { tokio::time::timeout(limit, (scenario.run)(run.clone())).await })
        }));
        // The test server's state goes with its process; another server keeps what it is left.
        if !server.is_test() {
            let cleaned = panic::catch_unwind(AssertUnwindSafe(|| {
                runtime.block_on(async {
                    let client = run.client().await;
                    tokio::time::timeout(limit, run.clean(&client)).await.ok();
                    client.close().await;
                })
            }));
            if cleaned.is_err() {
                eprintln!("test {name}: what it made could not all be removed");
            }
        }
        match ran {
            Ok(Ok(())) => {
                eprintln!("test {name} ... ok");
                passed += 1;
            }
            Ok(Err(_)) => {
                eprintln!("test {name} ... FAILED: still running after {limit:?}");
                failed.push(name);
            }
            Err(_) => {
                eprintln!("test {name} ... FAILED");
                failed.push(name);
            }
        }
    }

    let result = if failed.is_empty() { "ok" } else { "FAILED" };
    eprintln!(
        "\ntest result: {result}. {passed} passed; {} failed; {ignored} ignored",
        failed.len()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        for name in &failed {
            eprintln!("    {name}");
        }
        ExitCode::FAILURE
    }
}
