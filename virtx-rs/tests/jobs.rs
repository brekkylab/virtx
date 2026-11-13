//! Images and their jobs, and processes in sandboxes, against the fixture's `virtx-stdio`.

use std::{env, future, pin::Pin, thread, time::Duration};

use futures_core::Stream;
use virtx::{
    Error, ImageJobEnd, ImageJobEntry, ImageJobKind, ImageName, MountSource, Recipe, RegistryAuth,
    SandboxCreateArgs, SandboxExecArgs, SandboxJobEnd, SandboxJobEntry, Step,
};

mod common;

use common::ALPINE;

#[test]
fn a_process_run_in_a_sandbox_is_read_from_its_log() {
    let client = common::client();
    common::alpine(&client);
    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE))
        .wait()
        .unwrap();
    assert_eq!(
        client.get_sandbox(sandbox.id()).wait().unwrap().id(),
        sandbox.id()
    );
    let again = client.sandbox(sandbox.info().clone());
    assert_eq!(again.info(), sandbox.info());

    let job = sandbox
        .exec(&SandboxExecArgs::new(["echo", "hi"]))
        .wait()
        .unwrap();
    assert_eq!(job.sandbox_id(), sandbox.id());
    let read: Vec<_> = common::entries(job.log(None))
        .into_iter()
        .map(|entry| entry.entry)
        .collect();
    assert_eq!(
        read,
        [
            SandboxJobEntry::Stdout("hi".into()),
            SandboxJobEntry::End(SandboxJobEnd { code: 0 }),
        ]
    );
    let whole = common::entries(job.log(None));
    assert_eq!(common::entries(job.log(Some(whole[0].id))), whole[1..]);
    assert_eq!(job.get().wait().unwrap().info().code, Some(0));
    let held = sandbox.job_from(job.info().clone());
    assert_eq!(held.sandbox_id(), sandbox.id());
    assert_eq!(held.get().wait().unwrap().info().code, Some(0));
    let jobs = sandbox.jobs().wait().unwrap();
    assert!(
        jobs.iter()
            .any(|j| j.id() == job.id() && j.info().code == Some(0)),
        "{jobs:?}"
    );

    // `sleep` ends on the signal sent it.
    let sleep = sandbox
        .exec(&SandboxExecArgs::new(["sleep", "60"]))
        .wait()
        .unwrap();
    sleep.signal("SIGTERM").wait().unwrap();
    let ended = common::entries(sleep.log(None)).pop().unwrap().entry;
    assert_eq!(
        ended,
        SandboxJobEntry::End(SandboxJobEnd { code: 128 + 15 })
    );
    let again = sandbox.job(sleep.id()).wait().unwrap();
    assert!(!again.info().running, "{again:?}");
    again.remove().wait().unwrap();
    let gone = sleep.get().wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");

    sandbox.remove().wait().unwrap();
    let gone = client.get_sandbox(sandbox.id()).wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");
}

#[test]
#[ignore = "no server starts a process with a stdin to write to yet: `exec` with `stdin` is refused"]
fn stdin_written_comes_back_until_it_is_closed() {
    let client = common::client();
    common::alpine(&client);
    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE))
        .wait()
        .unwrap();
    let cat = sandbox
        .exec(&SandboxExecArgs::new(["cat"]).stdin(true))
        .wait()
        .unwrap();
    cat.write_stdin(Some("a\n"), true).wait().unwrap();
    let read: Vec<_> = common::entries(cat.log(None))
        .into_iter()
        .map(|entry| entry.entry)
        .collect();
    assert_eq!(
        read,
        [
            SandboxJobEntry::Stdout("a".into()),
            SandboxJobEntry::End(SandboxJobEnd { code: 0 }),
        ]
    );
    sandbox.remove().wait().unwrap();
}

#[test]
fn an_image_build_is_a_job_with_a_log() {
    let client = common::client();
    common::alpine(&client);
    let reference = format!("{}:latest", common::unique("built"));
    let job = client
        .build_image(
            Recipe::new(ALPINE).step(Step::run(format!("echo {reference} > /built"))),
            &reference,
        )
        .wait()
        .unwrap();
    assert_eq!(job.info().kind, ImageJobKind::Build);
    let log = common::entries(job.log(None));
    let built = common::tagged(&client, &reference).unwrap();
    assert_eq!(
        log.last().unwrap().entry,
        ImageJobEntry::End(ImageJobEnd {
            digest: Some(built.clone()),
            error: None,
        })
    );
    assert_eq!(common::entries(job.log(Some(log[0].id))), log[1..]);
    let done = job.get().wait().unwrap();
    assert_eq!(done.info().digest.as_ref(), Some(&built), "{done:?}");
    let got = client.get_image_job(job.id()).wait().unwrap();
    assert_eq!(got.info().digest.as_ref(), Some(&built), "{got:?}");
    assert!(
        client
            .list_image_job()
            .wait()
            .unwrap()
            .iter()
            .any(|j| j.id() == job.id())
    );
    job.remove().wait().unwrap();
    let gone = job.get().wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");
    client.remove_image(&reference).wait().unwrap();
}

#[test]
fn a_started_job_is_read_as_its_log() {
    let client = common::client();
    common::alpine(&client);
    let reference = format!("{}:latest", common::unique("read"));
    let mut log = client
        .build_image(
            Recipe::new(ALPINE).step(Step::run(format!("echo {reference} > /built"))),
            &reference,
        )
        .log();
    let mut read = Vec::new();
    while let Some(entry) = log.wait_next() {
        read.push(entry.unwrap().entry);
    }
    let built = common::tagged(&client, &reference);
    assert!(built.is_some());
    assert_eq!(
        read.last(),
        Some(&ImageJobEntry::End(ImageJobEnd {
            digest: built,
            error: None,
        }))
    );
    client.remove_image(&reference).wait().unwrap();

    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE))
        .wait()
        .unwrap();
    let mut log = sandbox.exec(&SandboxExecArgs::new(["echo", "hi"])).log();
    assert!(log.job().is_none());
    let mut read = Vec::new();
    while let Some(entry) = log.wait_next() {
        read.push(entry.unwrap().entry);
        // The job is the log's once started, to act on mid-stream.
        assert_eq!(log.job().unwrap().info().cmd, ["echo", "hi"]);
    }
    assert_eq!(
        read,
        [
            SandboxJobEntry::Stdout("hi".into()),
            SandboxJobEntry::End(SandboxJobEnd { code: 0 }),
        ]
    );

    // Polled, as a runtime would.
    let job = common::block_on(sandbox.exec(&SandboxExecArgs::new(["echo", "a"]))).unwrap();
    assert_eq!(job.info().cmd, ["echo", "a"]);
    let mut log = sandbox.exec(&SandboxExecArgs::new(["echo", "b"])).log();
    let mut read = Vec::new();
    while let Some(entry) = common::block_on(future::poll_fn(|cx| Pin::new(&mut log).poll_next(cx)))
    {
        read.push(entry.unwrap().entry);
    }
    assert_eq!(log.job().unwrap().info().cmd, ["echo", "b"]);
    assert_eq!(
        read,
        [
            SandboxJobEntry::Stdout("b".into()),
            SandboxJobEntry::End(SandboxJobEnd { code: 0 }),
        ]
    );

    // A start refused is the log's one item.
    let mut log = sandbox
        .exec(&SandboxExecArgs::new(["test-no-such-command"]))
        .log();
    let refused = log.wait_next().unwrap().unwrap_err();
    assert_eq!(refused.status(), Some(422), "{refused}");
    assert!(log.wait_next().is_none());
    assert!(log.job().is_none());
    sandbox.remove().wait().unwrap();
}

#[test]
fn a_started_build_finished_is_the_job_as_it_ended() {
    let client = common::client();
    common::alpine(&client);
    // Each way to wait, on a build of its own.
    for blocking in [false, true] {
        let reference = format!("{}:latest", common::unique("finished"));
        let build = client.build_image(
            Recipe::new(ALPINE).step(Step::run(format!("echo {reference} > /built"))),
            &reference,
        );
        let job = match blocking {
            false => common::block_on(build.finished()),
            true => build.wait_finished(),
        }
        .unwrap();
        let digest = common::tagged(&client, &reference).unwrap();
        assert_eq!(job.info().kind, ImageJobKind::Build);
        assert!(!job.info().running, "{job:?}");
        assert_eq!(job.info().digest.as_ref(), Some(&digest));
        assert_eq!(job.info(), job.get().wait().unwrap().info());
        assert_eq!(
            common::entries(job.log(None)).pop().unwrap().entry,
            ImageJobEntry::End(ImageJobEnd {
                digest: Some(digest),
                error: None,
            })
        );
        client.remove_image(&reference).wait().unwrap();
    }
}

#[test]
fn a_started_command_finished_is_the_job_with_its_code() {
    let client = common::client();
    common::alpine(&client);
    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE))
        .wait()
        .unwrap();
    // Each way to wait, on a command of its own; a code that is not 0 is not an error.
    for blocking in [false, true] {
        let exec = sandbox.exec(&SandboxExecArgs::new(["sh", "-c", "echo hi; exit 3"]));
        let job = match blocking {
            false => common::block_on(exec.finished()),
            true => exec.wait_finished(),
        }
        .unwrap();
        assert_eq!(job.sandbox_id(), sandbox.id());
        assert!(!job.info().running, "{job:?}");
        assert_eq!(job.info().code, Some(3));
        assert_eq!(job.info(), job.get().wait().unwrap().info());
        let read: Vec<_> = common::entries(job.log(None))
            .into_iter()
            .map(|entry| entry.entry)
            .collect();
        assert_eq!(
            read,
            [
                SandboxJobEntry::Stdout("hi".into()),
                SandboxJobEntry::End(SandboxJobEnd { code: 3 })
            ]
        );
    }
    sandbox.remove().wait().unwrap();
}

#[test]
fn a_started_job_that_fails_is_its_error_when_finished() {
    let client = common::client();
    let missing = "registry.invalid/missing:latest";
    for blocking in [false, true] {
        let pull = client.pull_image(missing, None);
        let failed = match blocking {
            false => common::block_on(pull.finished()),
            true => pull.wait_finished(),
        }
        .unwrap_err();
        let Error::Job(why) = &failed else {
            panic!("{failed:?}")
        };
        assert!(!why.is_empty());
    }
}

#[test]
fn a_job_removed_while_it_is_waited_on_is_an_error() {
    let client = common::client();
    common::alpine(&client);
    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE))
        .wait()
        .unwrap();
    let exec = sandbox.exec(&SandboxExecArgs::new(["sleep", "30"]));
    let removing = {
        let sandbox = sandbox.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(500));
            let jobs = sandbox.jobs().wait().unwrap();
            let job = jobs.iter().find(|job| job.info().running).unwrap();
            job.remove().wait().unwrap();
        })
    };
    let ended = exec.wait_finished().unwrap_err();
    assert!(matches!(ended, Error::Call(_)), "{ended:?}");
    removing.join().unwrap();
    sandbox.remove().wait().unwrap();
}

#[test]
fn an_image_is_tagged_downloaded_removed_and_uploaded_back() {
    let client = common::client();
    let digest = common::alpine(&client);
    let reference = format!("{}:1", common::unique("copy"));
    client.tag_image(ALPINE, &reference).wait().unwrap();
    assert_eq!(common::tagged(&client, &reference).as_ref(), Some(&digest));

    let path = env::temp_dir().join(format!("virtx-rs-{}-copy.tar", std::process::id()));
    assert_eq!(
        common::output(client.download_image(&reference, &path)).digest,
        digest
    );
    client.remove_image(&reference).wait().unwrap();
    assert_eq!(common::tagged(&client, &reference), None);
    assert_eq!(
        common::output(client.upload_image(&path, &reference)).digest,
        digest
    );
    assert_eq!(common::tagged(&client, &reference).as_ref(), Some(&digest));

    client.untag_image(&reference).wait().unwrap();
    assert_eq!(common::tagged(&client, &reference), None);
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn a_push_that_fails_ends_with_its_error_and_shows_no_credentials() {
    let client = common::client();
    common::alpine(&client);
    // A registry that does not resolve, so the push fails before it reaches anyone.
    let reference = format!("registry.invalid/{}:latest", common::unique("pushed"));
    client.tag_image(ALPINE, &reference).wait().unwrap();
    let auth = RegistryAuth::Bearer {
        token: "test-secret".into(),
    };
    let job = client.push_image(&reference, Some(auth)).wait().unwrap();
    assert_eq!(job.info().kind, ImageJobKind::Push);
    let log = common::entries(job.log(None));
    let Some(ImageJobEntry::End(end)) = log.last().map(|entry| &entry.entry) else {
        panic!("a log with no end: {log:?}")
    };
    assert!(end.digest.is_none() && end.error.is_some(), "{end:?}");
    let got = job.get().wait().unwrap();
    let shown = serde_json::to_string(&(got.info(), &log)).unwrap();
    assert!(!shown.contains("test-secret"), "{shown}");
    client.remove_image(&reference).wait().unwrap();

    let missing = format!("{}:latest", common::unique("unpushed"));
    let job = client.push_image(&missing, None).wait().unwrap();
    let end = common::entries(job.log(None)).pop().unwrap().entry;
    assert!(
        matches!(&end, ImageJobEntry::End(end) if end.error.is_some()),
        "{end:?}"
    );
}

#[test]
fn a_mount_at_a_path_taken_is_refused() {
    let client = common::client();
    let volume = client
        .create_volume(Some(&common::unique("mounted")))
        .wait()
        .unwrap();
    let source = || MountSource::Notion {
        api_key: "secret".into(),
    };
    volume.mount("notion", source()).wait().unwrap();
    let again = volume.mount("notion", source()).wait().unwrap_err();
    volume.unmount("notion").wait().unwrap();
    assert!(volume.mounts().wait().unwrap().is_empty());
    volume.remove().wait().unwrap();
    assert_eq!(again.status(), Some(409), "{again}");
}

#[test]
fn a_built_image_runs_with_what_its_steps_made() {
    let client = common::client();
    common::alpine(&client);
    let reference = format!("{}:latest", common::unique("steps"));
    let recipe = Recipe::new(ALPINE)
        .step(Step::run("apk add --no-cache jq"))
        .step(Step::env("GREETING", "hello"))
        .step(Step::workdir("/work"))
        .step(Step::run("echo \"$GREETING\" > note && rm /etc/motd"));
    let job = client.build_image(recipe, &reference).wait().unwrap();
    let log = common::entries(job.log(None));
    let Some(ImageJobEntry::End(ImageJobEnd {
        digest: Some(digest),
        ..
    })) = log.last().map(|entry| entry.entry.clone())
    else {
        panic!("{log:?}")
    };

    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new(reference.as_str()))
        .wait()
        .unwrap();
    let made = "jq -n 1 && echo $GREETING && pwd && cat note && test ! -e /etc/motd";
    let ran = common::run(&sandbox, &["sh", "-c", made]);
    let expected = ["1", "hello", "/work", "hello"].map(String::from).to_vec();
    assert_eq!(ran, (expected, 0));
    sandbox.remove().wait().unwrap();
    let by_digest = SandboxCreateArgs::new(ImageName::Digest { digest });
    client
        .create_sandbox(&by_digest)
        .wait()
        .unwrap()
        .remove()
        .wait()
        .unwrap();

    // A step that fails fails the build, and names nothing.
    let failed = format!("{}:latest", common::unique("failed"));
    let recipe = Recipe::new(reference.as_str()).step(Step::run("exit 3"));
    let job = client.build_image(recipe, &failed).wait().unwrap();
    let end = common::entries(job.log(None)).pop().unwrap().entry;
    assert!(
        matches!(&end, ImageJobEntry::End(end) if end.error.is_some()),
        "{end:?}"
    );
    let refused = client
        .create_sandbox(&SandboxCreateArgs::new(failed.as_str()))
        .wait()
        .unwrap_err();
    assert_eq!(refused.status(), Some(404), "{refused}");
    client.remove_image(&reference).wait().unwrap();
}

#[test]
fn a_pull_or_a_build_of_what_no_registry_has_ends_with_its_error_and_tags_nothing() {
    let client = common::client();
    // A registry that does not resolve, so each fails before it reaches anyone.
    let missing = "registry.invalid/missing:latest";
    let pull = client.pull_image(missing, None).wait().unwrap();
    assert_eq!(pull.info().kind, ImageJobKind::Pull);
    let built = format!("{}:latest", common::unique("unbuilt"));
    let build = client
        .build_image(Recipe::new(missing), &built)
        .wait()
        .unwrap();
    for job in [pull, build] {
        let end = common::entries(job.log(None)).pop().unwrap().entry;
        let ImageJobEntry::End(end) = end else {
            panic!("a log that ends with {end:?}")
        };
        assert!(end.digest.is_none() && end.error.is_some(), "{end:?}");
        assert_eq!(job.get().wait().unwrap().info().error, end.error);
    }
    assert_eq!(common::tagged(&client, &built), None);
}
