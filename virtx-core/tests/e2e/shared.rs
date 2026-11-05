//! One sandbox, several clients: what one makes, starts or removes there the others see as it
//! is, and a client leaving takes nothing from them.
//!
//! The clients have the same credentials, none against the test server and `VIRTX_E2E_TOKEN`
//! against the one at the URL: what is shared here is shared by right. Whether a client with
//! other credentials is kept from a sandbox is not tested.

use std::time::Duration;

use futures_util::{StreamExt, TryStreamExt};
use virtx_core::{LogEntry, SandboxCreateArgs, SandboxExecArgs, SandboxJobEntry};

use crate::harness::{ALPINE, Run, end, exec, refused, sandbox, sandbox_log, sleep};

scenarios![shared:
    Shared a_sandbox_one_client_made_another_uses,
    Shared processes_two_clients_start_at_once_keep_their_own_output,
    Shared the_clients_of_one_log_read_it_alike,
    Shared a_sandbox_one_client_removes_is_gone_for_the_other,
    Shared of_two_removals_at_once_one_takes_the_sandbox,
    Shared a_client_leaving_leaves_its_sandbox_to_the_other,
];

async fn a_sandbox_one_client_made_another_uses(run: Run) {
    let [maker, user] = run.clients().await;
    let id = sandbox(&run, &maker, SandboxCreateArgs::new(ALPINE)).await;
    let listed: Vec<_> = user
        .list_sandbox()
        .await
        .unwrap()
        .into_iter()
        .map(|sandbox| sandbox.id)
        .collect();
    assert!(listed.contains(&id), "{listed:?}");
    assert_eq!(
        user.get_sandbox(&id).await.unwrap(),
        maker.get_sandbox(&id).await.unwrap()
    );

    let job = user
        .exec_sandbox(&id, &exec(&["echo", "hi"]))
        .await
        .unwrap();
    let jobs: Vec<_> = maker
        .list_sandbox_job(&id)
        .await
        .unwrap()
        .into_iter()
        .map(|job| job.id)
        .collect();
    assert_eq!(jobs, std::slice::from_ref(&job.id));
    assert_eq!(
        sandbox_log(&maker, &id, &job.id).await,
        [SandboxJobEntry::Stdout("hi".into()), end(0)]
    );
    maker.remove_sandbox(&id).await.unwrap();
    maker.close().await;
    user.close().await;
}

async fn processes_two_clients_start_at_once_keep_their_own_output(run: Run) {
    let [a, b] = run.clients().await;
    let id = sandbox(&run, &a, SandboxCreateArgs::new(ALPINE)).await;
    let (echo_a, echo_b) = (exec(&["echo", "from a"]), exec(&["echo", "from b"]));
    let (from_a, from_b) = tokio::join!(a.exec_sandbox(&id, &echo_a), b.exec_sandbox(&id, &echo_b));
    let (from_a, from_b) = (from_a.unwrap(), from_b.unwrap());
    assert_ne!(from_a.id, from_b.id);
    // Each log is its own process's, whichever client reads it.
    for client in [&a, &b] {
        assert_eq!(
            sandbox_log(client, &id, &from_a.id).await,
            [SandboxJobEntry::Stdout("from a".into()), end(0)]
        );
        assert_eq!(
            sandbox_log(client, &id, &from_b.id).await,
            [SandboxJobEntry::Stdout("from b".into()), end(0)]
        );
    }
    a.remove_sandbox(&id).await.unwrap();
    a.close().await;
    b.close().await;
}

async fn the_clients_of_one_log_read_it_alike(run: Run) {
    let [writer, reader] = run.clients().await;
    let id = sandbox(&run, &writer, SandboxCreateArgs::new(ALPINE)).await;
    let args = SandboxExecArgs {
        stdin: true,
        ..exec(&["cat"])
    };
    let job = writer.exec_sandbox(&id, &args).await.unwrap();
    // The reader follows the log while both write its stdin, each a line, and one closes it.
    let reading = reader
        .sandbox_job_log(&id, &job.id, None)
        .try_collect::<Vec<LogEntry<SandboxJobEntry>>>();
    let writing = async {
        writer
            .write_sandbox_job_stdin(&id, &job.id, Some("one\n"), false)
            .await
            .unwrap();
        reader
            .write_sandbox_job_stdin(&id, &job.id, Some("two\n"), false)
            .await
            .unwrap();
        writer
            .write_sandbox_job_stdin(&id, &job.id, None::<String>, true)
            .await
            .unwrap();
    };
    let (read, ()) = tokio::join!(reading, writing);
    let read = read.unwrap();
    let entries: Vec<_> = read.iter().map(|entry| entry.entry.clone()).collect();
    assert_eq!(
        entries,
        [
            SandboxJobEntry::Stdout("one".into()),
            SandboxJobEntry::Stdout("two".into()),
            end(0)
        ]
    );
    let written: Vec<_> = writer
        .sandbox_job_log(&id, &job.id, None)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(written, read);
    // An entry's id is the same to every client: read again from it, the rest is the same.
    let rest: Vec<_> = writer
        .sandbox_job_log(&id, &job.id, Some(read[0].id))
        .try_collect()
        .await
        .unwrap();
    assert_eq!(rest, read[1..]);
    writer.remove_sandbox(&id).await.unwrap();
    writer.close().await;
    reader.close().await;
}

async fn a_sandbox_one_client_removes_is_gone_for_the_other(run: Run) {
    let [remover, other] = run.clients().await;
    let id = sandbox(&run, &remover, SandboxCreateArgs::new(ALPINE)).await;
    let job = other.exec_sandbox(&id, &sleep()).await.unwrap();
    // The other reads its process's log when the sandbox goes: its stream ends.
    let reading = other
        .sandbox_job_log(&id, &job.id, None)
        .collect::<Vec<_>>();
    let removing = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        remover.remove_sandbox(&id).await
    };
    let (_, removed) = tokio::join!(reading, removing);
    removed.unwrap();
    assert_eq!(refused(other.get_sandbox(&id).await).0, 404);
    assert_eq!(refused(other.get_sandbox_job(&id, &job.id).await).0, 404);
    let late = other.exec_sandbox(&id, &exec(&["echo", "late"])).await;
    assert_eq!(refused(late).0, 404);
    let listed = other.list_sandbox().await.unwrap();
    assert!(!listed.iter().any(|sandbox| sandbox.id == id), "{listed:?}");
    remover.close().await;
    other.close().await;
}

async fn of_two_removals_at_once_one_takes_the_sandbox(run: Run) {
    let [a, b] = run.clients().await;
    let id = sandbox(&run, &a, SandboxCreateArgs::new(ALPINE)).await;
    b.get_sandbox(&id).await.unwrap();
    let (by_a, by_b) = tokio::join!(a.remove_sandbox(&id), b.remove_sandbox(&id));
    let removed = [by_a, by_b];
    assert_eq!(
        removed.iter().filter(|removed| removed.is_ok()).count(),
        1,
        "{removed:?}"
    );
    let refusal = removed.into_iter().find(Result::is_err).unwrap();
    assert_eq!(refused(refusal).0, 404);
    a.close().await;
    b.close().await;
}

async fn a_client_leaving_leaves_its_sandbox_to_the_other(run: Run) {
    let [leaving, staying] = run.clients().await;
    let id = sandbox(&run, &leaving, SandboxCreateArgs::new(ALPINE)).await;
    let job = leaving.exec_sandbox(&id, &sleep()).await.unwrap();
    leaving.close().await;

    assert!(staying.get_sandbox_job(&id, &job.id).await.unwrap().running);
    staying
        .signal_sandbox_job(&id, &job.id, "SIGTERM")
        .await
        .unwrap();
    assert_eq!(
        sandbox_log(&staying, &id, &job.id).await.last(),
        Some(&end(128 + 15))
    );
    staying.remove_sandbox(&id).await.unwrap();
    staying.close().await;
}
