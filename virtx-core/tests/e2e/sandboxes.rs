//! A sandbox's life: booted from what a build tagged or refused an image not there, torn down
//! under its work, and gone with the server its client closes.

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use virtx_core::{SandboxCreateArgs, SandboxJobEntry};

use crate::harness::{
    ALPINE, Made, Run, build, end, exec, refused, sandbox, sandbox_log, sleep, unique,
};

scenarios![sandboxes:
    Any a_sandbox_boots_the_image_a_build_tagged,
    Any a_sandbox_of_an_image_not_there_is_refused,
    Any removing_a_sandbox_ends_its_jobs_and_the_logs_read,
    Spawned closing_the_client_ends_its_server,
];

async fn a_sandbox_boots_the_image_a_build_tagged(run: Run) {
    let client = run.client().await;
    let reference = format!("{}:latest", unique("boot"));
    let digest = build(&run, &client, &reference).await;
    let sandbox = run
        .create_sandbox(&client, &SandboxCreateArgs::new(reference.as_str()))
        .await;
    assert_eq!(sandbox.image, digest);
    let job = client
        .exec_sandbox(&sandbox.id, &exec(&["echo", "hi"]))
        .await
        .unwrap();
    assert_eq!(
        sandbox_log(&client, &sandbox.id, &job.id).await,
        [SandboxJobEntry::Stdout("hi".into()), end(0)]
    );
    client.remove_sandbox(&sandbox.id).await.unwrap();
    client.remove_image(&reference).await.unwrap();
    client.close().await;
}

async fn a_sandbox_of_an_image_not_there_is_refused(run: Run) {
    let client = run.client().await;
    let missing = format!("{}:latest", unique("missing"));
    let made = client
        .create_sandbox(&SandboxCreateArgs::new(missing.as_str()))
        .await;
    if let Ok(sandbox) = &made {
        run.made(Made::Sandbox(sandbox.id.clone()));
    }
    assert_eq!(refused(made), (404, "not_found".into()));
    client.close().await;
}

async fn removing_a_sandbox_ends_its_jobs_and_the_logs_read(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let job = client.exec_sandbox(&id, &sleep()).await.unwrap();
    // The log is being read when the sandbox goes: its stream ends, whatever it ends with.
    let reading = client
        .sandbox_job_log(&id, &job.id, None)
        .collect::<Vec<_>>();
    let removing = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        client.remove_sandbox(&id).await
    };
    let (_, removed) = tokio::join!(reading, removing);
    removed.unwrap();
    assert_eq!(refused(client.get_sandbox(&id).await).0, 404);
    assert_eq!(refused(client.get_sandbox_job(&id, &job.id).await).0, 404);
    client.close().await;
}

async fn closing_the_client_ends_its_server(run: Run) {
    let client = run.client().await;
    sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let started = Instant::now();
    client.close().await;
    // The server exits once its stdin closes, well before it would be killed.
    assert!(started.elapsed() < Duration::from_secs(5));
}
