//! Jobs and timeouts against a real console server, named by `$VIRTX_CONSOLE` (`virtx-uvm`,
//! say): output seen while a command runs, output kept when a timeout kills it, and a console
//! that answers other calls while a job runs.
//!
//! Skipped, not failed, without a server. `$VIRTX_IMAGE` names the image the session boots
//! on, `alpine:3.21` when unset.
//!
//! ```sh
//! VIRTX_CONSOLE=virtx-uvm cargo test --test console_jobs -- --nocapture
//! ```

use std::{
    future::Future,
    task::Poll,
    time::{Duration, Instant},
};

use virtx::{
    console::ConsoleClient,
    image::ImageSource,
    protocol::{Error, Failure},
};

/// A console on the server `$VIRTX_CONSOLE` names, or `None` to skip.
async fn console() -> Option<ConsoleClient> {
    let Ok(server) = std::env::var("VIRTX_CONSOLE") else {
        eprintln!("skipped: set $VIRTX_CONSOLE to run against a console server");
        return None;
    };
    let image = std::env::var("VIRTX_IMAGE").unwrap_or_else(|_| "alpine:3.21".into());
    let console = ConsoleClient::builder()
        .cmd(&[server])
        .image(ImageSource::reference(image))
        .build()
        .await
        .expect("a console on $VIRTX_CONSOLE");
    Some(console)
}

/// A command printing once a second is seen once a second, not all at the end.
#[tokio::test]
async fn a_command_printing_once_a_second_is_seen_once_a_second() {
    let Some(mut console) = console().await else {
        return;
    };
    // Booted first, so the boot is not counted against the first tick.
    console.exec(["true"], None).await.unwrap();

    let mut stream = console
        .exec_stream(
            ["sh", "-c", "for i in 1 2 3; do echo tick $i; sleep 1; done"],
            None,
        )
        .await
        .unwrap();
    let mut ticks = Vec::new();
    let mut code = None;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        for line in String::from_utf8_lossy(&chunk.stdout).lines() {
            ticks.push((line.to_string(), Instant::now()));
        }
        code = code.or(chunk.code);
    }

    assert_eq!(
        ticks
            .iter()
            .map(|(line, _)| line.as_str())
            .collect::<Vec<_>>(),
        ["tick 1", "tick 2", "tick 3"]
    );
    for pair in ticks.windows(2) {
        let apart = pair[1].1 - pair[0].1;
        assert!(
            apart > Duration::from_millis(500),
            "{} and {} arrived {apart:?} apart",
            pair[0].0,
            pair[1].0
        );
    }
    assert_eq!(code, Some(0));
}

/// A command killed at its timeout says what it wrote up to the kill.
#[tokio::test]
async fn a_timeout_returns_the_output_up_to_the_kill() {
    let Some(mut console) = console().await else {
        return;
    };

    let Err(Failure::Refused(refused)) = console
        .exec(
            ["sh", "-c", "echo started; echo err >&2; sleep 5"],
            Some(1000),
        )
        .await
    else {
        panic!("a command outliving its timeout should be refused");
    };
    assert_eq!(refused.code, Error::TIMED_OUT);
    let output = refused.output().expect("the output up to the kill");
    assert_eq!(output.stdout, b"started\n");
    assert_eq!(output.stderr, b"err\n");

    // A job ends the same way, in the poll that reports its end.
    let job = console
        .spawn(["sh", "-c", "echo started; sleep 5"], Some(1000))
        .await
        .unwrap();
    let mut stdout = Vec::new();
    let refused = loop {
        match console.poll(job, Some(5000), None).await {
            Ok(chunk) => {
                assert!(chunk.code.is_none(), "{chunk:?}");
                stdout.extend(chunk.stdout);
            }
            Err(Failure::Refused(refused)) => break refused,
            Err(broken) => panic!("{broken}"),
        }
    };
    assert_eq!(refused.code, Error::TIMED_OUT);
    stdout.extend(refused.output().unwrap().stdout);
    assert_eq!(stdout, b"started\n");
}

/// The console answers other calls while a job runs, and the job is still there after.
#[tokio::test]
async fn a_read_succeeds_while_a_job_runs() {
    let Some(mut console) = console().await else {
        return;
    };

    let job = console
        .spawn(
            ["sh", "-c", "echo begun > /tmp/job.log; sleep 3; echo done"],
            None,
        )
        .await
        .unwrap();

    let started = Instant::now();
    // Written by the job, so read once it is there: a shell creates the file before it
    // writes to it.
    let read = loop {
        match console.read("/tmp/job.log", None, None).await {
            Ok(read) if !read.data.is_empty() => break read,
            Ok(_) => {}
            Err(Failure::Refused(e)) if e.code == Error::NOT_FOUND => {}
            Err(other) => panic!("{other}"),
        }
    };
    assert_eq!(read.data, b"begun\n");
    let ran = console.exec(["echo", "meanwhile"], None).await.unwrap();
    assert_eq!(ran.stdout, b"meanwhile\n");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the console waited for the job"
    );

    let mut stdout = Vec::new();
    let code = loop {
        let chunk = console.poll(job, Some(5000), None).await.unwrap();
        stdout.extend(chunk.stdout);
        if let Some(code) = chunk.code {
            break code;
        }
    };
    assert_eq!((code, stdout.as_slice()), (0, b"done\n".as_slice()));

    // Reported once, and gone after.
    let Err(Failure::Refused(gone)) = console.poll(job, None, None).await else {
        panic!("a job whose end was reported should be unknown");
    };
    assert_eq!(gone.code, Error::UNKNOWN_JOB);
}

/// A stream dropped before its job ended has the job killed: it is gone by the next call.
#[tokio::test]
async fn dropping_a_stream_kills_its_job() {
    let Some(mut console) = console().await else {
        return;
    };

    let mut stream = console
        .exec_stream(["sh", "-c", "echo serving; sleep 60"], None)
        .await
        .unwrap();
    let job = stream.job();
    assert_eq!(stream.next().await.unwrap().unwrap().stdout, b"serving\n");
    drop(stream);

    let started = Instant::now();
    console.exec(["true"], None).await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(10));
    let Err(Failure::Refused(gone)) = console.poll(job, None, None).await else {
        panic!("a job whose stream was dropped should have been killed and collected");
    };
    assert_eq!(gone.code, Error::UNKNOWN_JOB);
}

/// A stream dropped while its poll waits on a silent command has the job killed too, and the
/// answer that poll was owed does not reach the next call.
#[tokio::test]
async fn dropping_a_stream_mid_poll_kills_its_job() {
    let Some(mut console) = console().await else {
        return;
    };

    let mut stream = console
        .exec_stream(["sh", "-c", "echo serving; sleep 60"], None)
        .await
        .unwrap();
    let job = stream.job();
    assert_eq!(stream.next().await.unwrap().unwrap().stdout, b"serving\n");
    {
        let mut next = std::pin::pin!(stream.next());
        let polled = std::future::poll_fn(|cx| Poll::Ready(next.as_mut().poll(cx))).await;
        assert!(
            polled.is_pending(),
            "a silent command's poll should still be waiting"
        );
    }
    drop(stream);

    let started = Instant::now();
    let echoed = console.exec(["echo", "next"], None).await.unwrap();
    assert_eq!(echoed.stdout, b"next\n");
    assert!(started.elapsed() < Duration::from_secs(10));
    let Err(Failure::Refused(gone)) = console.poll(job, None, None).await else {
        panic!("a job whose stream was dropped mid-poll should have been killed and collected");
    };
    assert_eq!(gone.code, Error::UNKNOWN_JOB);
}

/// An id held across a `stop` names nothing in the boot after it, even where the new boot
/// starts a job of its own.
#[tokio::test]
async fn a_job_from_before_a_stop_is_not_the_one_after_it() {
    let Some(mut console) = console().await else {
        return;
    };

    let before = console.spawn(["sleep", "60"], None).await.unwrap();
    console.stop().await.unwrap();
    let after = console
        .spawn(["sh", "-c", "echo after; sleep 60"], None)
        .await
        .unwrap();
    assert_ne!(before, after);

    let Err(Failure::Refused(gone)) = console.poll(before, None, None).await else {
        panic!("a job from before the stop should be unknown");
    };
    assert_eq!(gone.code, Error::UNKNOWN_JOB);
    let chunk = console.poll(after, Some(5000), None).await.unwrap();
    assert_eq!(chunk.stdout, b"after\n");
    console.discard(after).await.unwrap();
}
