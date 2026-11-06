//! Credentials: a request of each kind without valid ones is refused, as itself.
//!
//! That a client sends them with every kind of request needs no scenario of its own: the test
//! server over TCP refuses any request without its made-up token and project, so every
//! scenario against it fails where one is left off. It runs against the test server alone, the
//! one server here given credentials.

use futures_util::StreamExt;
use http::HeaderMap;
use virtx_core::{Auth, Client, FileRoot, SandboxCreateArgs};

use crate::harness::{ALPINE, Made, PROJECT_HEADER, Run, exec, refused, sandbox, unique};

scenarios![access:
    Credentials a_request_of_each_kind_without_valid_credentials_is_refused,
];

/// The status each kind of request by `client` is refused with, in order: a resource, a job's
/// log, a volume's files, and the images.
async fn refusals(client: &Client, sandbox: &str, job: &str, volume: &str) -> [u16; 4] {
    let log = client.sandbox_job_log(sandbox, job, None).next().await;
    [
        refused(client.get_sandbox(sandbox).await).0,
        refused(log.expect("a log, or why not")).0,
        refused(client.read_dir(&FileRoot::Volume(volume.into()), "").await).0,
        refused(client.list_image().await).0,
    ]
}

async fn a_request_of_each_kind_without_valid_credentials_is_refused(run: Run) {
    let reach = run.reach().await;
    let client = reach.client();
    let volume = unique("volume");
    run.made(Made::Volume(volume.clone()));
    client.create_volume(Some(&volume)).await.unwrap();
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let job = client
        .exec_sandbox(&id, &exec(&["echo", "hi"]))
        .await
        .unwrap();

    let anonymous = reach.with(Auth::None, reach.headers.clone());
    let stranger = Auth::Bearer {
        token: unique("token"),
    };
    let stranger = reach.with(stranger, reach.headers.clone());
    for (who, other) in [
        ("no token", &anonymous),
        ("a token not the server's", &stranger),
    ] {
        let refused = refusals(other, &id, &job.id, &volume).await;
        assert_eq!(refused, [401; 4], "{who}");
    }
    // The test server's token, reaching a project not its own.
    if run.is_test() {
        let mut elsewhere = HeaderMap::new();
        elsewhere.insert(PROJECT_HEADER, "elsewhere".parse().unwrap());
        let other = reach.with(reach.auth.clone(), elsewhere);
        assert_eq!(refusals(&other, &id, &job.id, &volume).await, [403; 4]);
        other.close().await;
    }

    client.remove_sandbox(&id).await.unwrap();
    client.remove_volume(&volume).await.unwrap();
    for client in [client, anonymous, stranger] {
        client.close().await;
    }
}
