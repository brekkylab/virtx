//! A process's life in a sandbox: its output, its stdin, its end by signal, timeout or
//! removal, or both at once, its log read again, several side by side, and a command that
//! cannot start.

use std::time::Duration;

use futures_util::TryStreamExt;
use virtx_core::{
    LogEntry, SandboxCreateArgs, SandboxExecArgs, SandboxJobEntry, testing::LOG_LIMIT,
};

use crate::harness::{ALPINE, Run, end, exec, refused, sandbox, sandbox_log, sleep};

scenarios![processes:
    Any a_process_writes_its_output_then_ends,
    Any stdin_written_comes_back_until_it_is_closed,
    Any a_signal_a_timeout_or_a_removal_ends_a_process,
    Any a_timeout_and_a_signal_at_once_end_a_process_once,
    Any processes_in_a_sandbox_run_side_by_side,
    Any a_log_read_again_starts_after_the_entry_it_names,
    Any a_command_that_cannot_start_makes_no_job,
    Test a_log_past_the_limit_is_truncated_but_keeps_its_end,
];

async fn a_timeout_and_a_signal_at_once_end_a_process_once(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    // Beside them, a process neither is for, which is left running.
    let bystander = client.exec_sandbox(&id, &sleep()).await.unwrap();
    let timed = SandboxExecArgs {
        timeout_ms: Some(100),
        ..sleep()
    };
    // The signal comes from before the timeout runs out to after it, a round each.
    for delay in (50..=150).step_by(10) {
        let job = client.exec_sandbox(&id, &timed).await.unwrap();
        tokio::time::sleep(Duration::from_millis(delay)).await;
        let signalled = client.signal_sandbox_job(&id, &job.id, "SIGTERM").await;
        let log = sandbox_log(&client, &id, &job.id).await;
        let ends = log
            .iter()
            .filter(|entry| matches!(entry, SandboxJobEntry::End(_)))
            .count();
        let Some(SandboxJobEntry::End(ended)) = log.last() else {
            panic!("after {delay} ms, a log with no end: {log:?}")
        };
        assert_eq!(ends, 1, "after {delay} ms: {log:?}");
        let read = client.get_sandbox_job(&id, &job.id).await.unwrap();
        assert_eq!(read.code, Some(ended.code), "after {delay} ms");
        match signalled {
            // Sent, which the timeout may yet beat.
            Ok(()) => assert!(
                [128 + 9, 128 + 15].contains(&ended.code),
                "after {delay} ms: {}",
                ended.code
            ),
            // Refused, the timeout having ended it first.
            refusal => {
                assert_eq!(refused(refusal), (409, "ended".into()), "after {delay} ms");
                assert_eq!(ended.code, 128 + 9, "after {delay} ms");
            }
        }
    }
    let bystander = client.get_sandbox_job(&id, &bystander.id).await.unwrap();
    assert!(bystander.running);
    client.remove_sandbox(&id).await.unwrap();
    client.close().await;
}

async fn a_process_writes_its_output_then_ends(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let job = client
        .exec_sandbox(&id, &exec(&["echo", "a", "b"]))
        .await
        .unwrap();
    assert_eq!(
        sandbox_log(&client, &id, &job.id).await,
        [SandboxJobEntry::Stdout("a b".into()), end(0)]
    );
    let read = client.get_sandbox_job(&id, &job.id).await.unwrap();
    assert!(!read.running);
    assert_eq!(read.code, Some(0));
    assert_eq!(read.cmd, ["echo", "a", "b"]);
    client.remove_sandbox(&id).await.unwrap();
    client.close().await;
}

async fn stdin_written_comes_back_until_it_is_closed(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let args = SandboxExecArgs {
        stdin: true,
        ..exec(&["cat"])
    };
    let job = client.exec_sandbox(&id, &args).await.unwrap();
    client
        .write_sandbox_job_stdin(&id, &job.id, Some("one\ntwo\n"), false)
        .await
        .unwrap();
    client
        .write_sandbox_job_stdin(&id, &job.id, None::<String>, true)
        .await
        .unwrap();
    assert_eq!(
        sandbox_log(&client, &id, &job.id).await,
        [
            SandboxJobEntry::Stdout("one".into()),
            SandboxJobEntry::Stdout("two".into()),
            end(0)
        ]
    );
    let again = client
        .write_sandbox_job_stdin(&id, &job.id, Some("three\n"), false)
        .await;
    assert_eq!(refused(again), (409, "ended".into()));
    client.remove_sandbox(&id).await.unwrap();
    client.close().await;
}

async fn a_signal_a_timeout_or_a_removal_ends_a_process(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let sleep = sleep();

    let signalled = client.exec_sandbox(&id, &sleep).await.unwrap();
    client
        .signal_sandbox_job(&id, &signalled.id, "SIGTERM")
        .await
        .unwrap();
    let log = sandbox_log(&client, &id, &signalled.id).await;
    assert_eq!(log.last(), Some(&end(128 + 15)));
    let again = client
        .signal_sandbox_job(&id, &signalled.id, "SIGTERM")
        .await;
    assert_eq!(refused(again), (409, "ended".into()));

    let timed = SandboxExecArgs {
        timeout_ms: Some(200),
        ..sleep.clone()
    };
    let timed = client.exec_sandbox(&id, &timed).await.unwrap();
    let log = sandbox_log(&client, &id, &timed.id).await;
    assert_eq!(log.last(), Some(&end(128 + 9)));

    let removed = client.exec_sandbox(&id, &sleep).await.unwrap();
    client.remove_sandbox_job(&id, &removed.id).await.unwrap();
    assert_eq!(
        refused(client.get_sandbox_job(&id, &removed.id).await).0,
        404
    );
    client.remove_sandbox(&id).await.unwrap();
    client.close().await;
}

async fn processes_in_a_sandbox_run_side_by_side(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let sleep = sleep();
    let first = client.exec_sandbox(&id, &sleep).await.unwrap();
    let second = client.exec_sandbox(&id, &sleep).await.unwrap();
    let echo = client
        .exec_sandbox(&id, &exec(&["echo", "beside"]))
        .await
        .unwrap();
    assert_eq!(
        sandbox_log(&client, &id, &echo.id).await,
        [SandboxJobEntry::Stdout("beside".into()), end(0)]
    );
    let listed: Vec<_> = client
        .list_sandbox_job(&id)
        .await
        .unwrap()
        .into_iter()
        .map(|job| job.id)
        .collect();
    for job in [&first.id, &second.id, &echo.id] {
        assert!(listed.contains(job), "{listed:?}");
    }
    for job in [&first.id, &second.id] {
        assert!(client.get_sandbox_job(&id, job).await.unwrap().running);
    }
    client.remove_sandbox(&id).await.unwrap();
    client.close().await;
}

async fn a_log_read_again_starts_after_the_entry_it_names(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let job = client
        .exec_sandbox(&id, &exec(&["echo", "hi"]))
        .await
        .unwrap();
    let whole: Vec<LogEntry<SandboxJobEntry>> = client
        .sandbox_job_log(&id, &job.id, None)
        .try_collect()
        .await
        .unwrap();
    assert_eq!(whole.len(), 2, "{whole:?}");
    let rest: Vec<_> = client
        .sandbox_job_log(&id, &job.id, Some(whole[0].id))
        .try_collect()
        .await
        .unwrap();
    assert_eq!(rest, whole[1..]);
    client.remove_sandbox(&id).await.unwrap();
    client.close().await;
}

async fn a_command_that_cannot_start_makes_no_job(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let started = client
        .exec_sandbox(&id, &exec(&["e2e-no-such-command"]))
        .await;
    assert_eq!(refused(started), (422, "not_started".into()));
    assert!(client.list_sandbox_job(&id).await.unwrap().is_empty());
    client.remove_sandbox(&id).await.unwrap();
    client.close().await;
}

async fn a_log_past_the_limit_is_truncated_but_keeps_its_end(run: Run) {
    let client = run.client().await;
    let id = sandbox(&run, &client, SandboxCreateArgs::new(ALPINE)).await;
    let job = client
        .exec_sandbox(&id, &exec(&["lines", "20"]))
        .await
        .unwrap();
    let log = sandbox_log(&client, &id, &job.id).await;
    assert_eq!(log.len(), LOG_LIMIT + 2, "{log:?}");
    assert_eq!(log[LOG_LIMIT], SandboxJobEntry::Truncated {});
    assert_eq!(log.last(), Some(&end(0)));
    client.close().await;
}
