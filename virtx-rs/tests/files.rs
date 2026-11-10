//! The files of a volume, a snapshot and a sandbox, against the fixture's `virtx-stdio`.

use virtx::{Files, MountSource, MountType, SandboxCreateArgs};

mod common;

use common::ALPINE;

/// Each of a handle's calls on its files, in `base`: a directory of them, or its root.
fn every_call(files: &impl Files, base: &str) {
    let at = |path: &str| match base {
        "" => path.to_owned(),
        base => format!("{base}/{path}"),
    };
    files.create_dir(at("dir")).wait().unwrap();
    files
        .write_file(at("dir/a.txt"), "hello world", None)
        .wait()
        .unwrap();
    files.create_dir_all(at("deep/er/dir")).wait().unwrap();
    files.create_dir_all(at("deep/er/dir")).wait().unwrap();
    assert!(files.stat_file(at("deep/er/dir")).wait().unwrap().is_dir);

    let metadata = files.stat_file(at("dir/a.txt")).wait().unwrap();
    assert!(!metadata.is_dir && metadata.size == 11, "{metadata:?}");
    let entries = files.read_dir(at("dir")).wait().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "a.txt");
    assert_eq!(entries[0].metadata, metadata);

    let read = files.read_file(at("dir/a.txt"), 0, None).wait().unwrap();
    assert_eq!(read, b"hello world");
    let read = files.read_file(at("dir/a.txt"), 6, Some(3)).wait().unwrap();
    assert_eq!(read, b"wor");

    // Text, an etag to write over and one gone stale, and what is there, told without an
    // error.
    files
        .write_file(at("dir/t.txt"), "텍스트", None)
        .wait()
        .unwrap();
    assert_eq!(files.read_text(at("dir/t.txt")).wait().unwrap(), "텍스트");
    let etag = files
        .stat_file(at("dir/t.txt"))
        .wait()
        .unwrap()
        .etag
        .unwrap();
    files
        .write_file(at("dir/t.txt"), "text 2", Some(&etag))
        .wait()
        .unwrap();
    let stale = files
        .write_file(at("dir/t.txt"), "text 3", Some(&etag))
        .wait()
        .unwrap_err();
    assert_eq!(stale.status(), Some(412), "{stale}");
    assert!(files.exists(at("dir/t.txt")).wait().unwrap());
    assert!(files.is_file(at("dir/t.txt")).wait().unwrap());
    assert!(!files.is_dir(at("dir/t.txt")).wait().unwrap());
    assert!(files.is_dir(at("dir")).wait().unwrap());
    files.remove_file(at("dir/t.txt"), false).wait().unwrap();
    assert!(!files.exists(at("dir/t.txt")).wait().unwrap());
    let missing = files
        .remove_file(at("dir/t.txt"), false)
        .wait()
        .unwrap_err();
    assert_eq!(missing.status(), Some(404), "{missing}");
    files.remove_file(at("dir/t.txt"), true).wait().unwrap();

    files
        .rename_file(at("dir/a.txt"), at("b.txt"), false)
        .wait()
        .unwrap();
    files
        .copy_file(at("b.txt"), at("c.txt"), false)
        .wait()
        .unwrap();
    let refused = files
        .copy_file(at("b.txt"), at("c.txt"), false)
        .wait()
        .unwrap_err();
    assert_eq!(refused.status(), Some(412), "{refused}");
    files.remove_file(at("dir"), false).wait().unwrap();
    let gone = files.stat_file(at("dir")).wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");
    let read = files.read_file(at("c.txt"), 0, None).wait().unwrap();
    assert_eq!(read, b"hello world");
}

#[test]
fn a_volume_s_files_answer_every_call() {
    let client = common::client();
    let volume = client
        .create_volume(Some(&common::unique("files")))
        .wait()
        .unwrap();
    every_call(&volume, "");
    volume.remove().wait().unwrap();
}

#[test]
fn a_snapshot_s_files_answer_every_call() {
    let client = common::client();
    let snapshot = client
        .create_snapshot(Some(&common::unique("files")), None)
        .wait()
        .unwrap();
    every_call(&snapshot, "");
    snapshot.remove().wait().unwrap();
}

#[test]
fn a_sandbox_s_files_answer_every_call() {
    let client = common::client();
    common::alpine(&client);
    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE))
        .wait()
        .unwrap();
    every_call(&sandbox, "root");
    sandbox.remove().wait().unwrap();
}

#[test]
fn a_volume_carries_its_client_to_its_files_and_mounts() {
    let client = common::client();
    let name = common::unique("carried");
    let volume = client.create_volume(Some(&name)).wait().unwrap();
    assert_eq!((volume.name(), volume.info().size), (name.as_str(), 0));
    volume.create_dir_all("in/dir").wait().unwrap();
    volume.write_file("in/a.txt", "hello", None).wait().unwrap();
    volume
        .rename_file("in/a.txt", "in/b.txt", false)
        .wait()
        .unwrap();
    volume.copy_file("in/b.txt", "c.txt", false).wait().unwrap();
    assert_eq!(volume.read_text("c.txt").wait().unwrap(), "hello");
    assert!(volume.is_dir("in/dir").wait().unwrap());
    let mut names: Vec<_> = volume
        .read_dir("in")
        .wait()
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect();
    names.sort();
    assert_eq!(names, ["b.txt", "dir"]);

    let mount = volume
        .mount(
            "notion",
            MountSource::Notion {
                api_key: "k".into(),
            },
        )
        .wait()
        .unwrap();
    assert_eq!(mount.mount_type, MountType::Notion);
    let fresh = client.get_volume(&name).wait().unwrap();
    assert_eq!(fresh.mounts().wait().unwrap(), [mount]);
    volume.unmount("notion").wait().unwrap();
    assert!(fresh.mounts().wait().unwrap().is_empty());

    assert_eq!(fresh.read_text("in/b.txt").wait().unwrap(), "hello");
    assert!(
        client
            .list_volume()
            .wait()
            .unwrap()
            .iter()
            .any(|v| v.name() == name)
    );
    volume.remove().wait().unwrap();
    let gone = client.get_volume(&name).wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");
    let gone = fresh.stat_file("c.txt").wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");
}

#[test]
fn a_snapshot_is_created_got_and_removed_and_refused_while_a_sandbox_holds_it() {
    let client = common::client();
    let name = common::unique("snapshot");
    let created = client.create_snapshot(Some(&name), None).wait().unwrap();
    assert_eq!(
        (created.name(), created.info().in_use),
        (name.as_str(), false),
        "{created:?}"
    );
    let got = client.get_snapshot(&name).wait().unwrap();
    assert_eq!(got.info(), created.info());
    // After the comparison: a write grows the size, what the snapshot takes on disk.
    created.write_file("a.txt", "kept", None).wait().unwrap();
    assert_eq!(got.read_text("a.txt").wait().unwrap(), "kept");

    common::alpine(&client);
    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE).snapshot(&name))
        .wait()
        .unwrap();
    let held = client.get_snapshot(&name).wait().unwrap();
    assert!(held.info().in_use);
    let refused = held.read_dir("").wait().unwrap_err();
    assert_eq!(refused.status(), Some(423), "{refused}");
    // One sandbox at a time on a snapshot.
    let second = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE).snapshot(&name))
        .wait()
        .unwrap_err();
    assert_eq!(second.status(), Some(409), "{second}");
    assert_eq!(
        common::run(&sandbox, &["sh", "-c", "echo kept > /root/kept"]).1,
        0
    );
    sandbox.remove().wait().unwrap();

    // What the sandbox wrote is in the snapshot, with no `sync`, and what is put there is in
    // it for the next one.
    assert_eq!(held.read_text("root/kept").wait().unwrap(), "kept\n");
    held.write_file("root/put", "put\n", None).wait().unwrap();
    let next = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE).snapshot(&name))
        .wait()
        .unwrap();
    let read = common::run(&next, &["cat", "/root/kept", "/root/put"]);
    assert_eq!(read, (vec!["kept".into(), "put".into()], 0));
    next.remove().wait().unwrap();

    created.remove().wait().unwrap();
    let gone = client.get_snapshot(&name).wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");
}

#[test]
fn a_sandbox_s_files_are_its_own_and_go_with_it() {
    let client = common::client();
    common::alpine(&client);
    let volume = client
        .create_volume(Some(&common::unique("handed")))
        .wait()
        .unwrap();
    let snapshot = client
        .create_snapshot(Some(&common::unique("handed")), None)
        .wait()
        .unwrap();
    let on_handles = client
        .create_sandbox(
            &SandboxCreateArgs::new(ALPINE)
                .snapshot(snapshot.name())
                .readonly_volume(volume.name(), "/data"),
        )
        .wait()
        .unwrap();
    assert_eq!(on_handles.info().snapshot.as_deref(), Some(snapshot.name()));
    on_handles.remove().wait().unwrap();
    snapshot.remove().wait().unwrap();
    volume.remove().wait().unwrap();

    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE))
        .wait()
        .unwrap();
    sandbox.create_dir_all("work/dir").wait().unwrap();
    sandbox
        .write_file("work/dir/a.txt", "hello", None)
        .wait()
        .unwrap();
    assert_eq!(sandbox.read_text("work/dir/a.txt").wait().unwrap(), "hello");
    assert_eq!(
        client
            .get_sandbox(sandbox.id())
            .wait()
            .unwrap()
            .read_file("work/dir/a.txt", 1, Some(3))
            .wait()
            .unwrap(),
        b"ell"
    );
    sandbox.remove_file("work/dir/a.txt", false).wait().unwrap();
    sandbox.remove().wait().unwrap();
    let gone = client.get_sandbox(sandbox.id()).wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");
    let gone = sandbox.read_file("", 0, None).wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");
}

#[test]
fn a_sandbox_s_files_are_what_its_processes_see() {
    let client = common::client();
    common::alpine(&client);
    let volume = client
        .create_volume(Some(&common::unique("seen")))
        .wait()
        .unwrap();
    let args = SandboxCreateArgs::new(ALPINE).readonly_volume(volume.name(), "/data");
    let sandbox = client.create_sandbox(&args).wait().unwrap();
    let names = |path: &str| -> Vec<String> {
        let entries = sandbox.read_dir(path).wait().unwrap();
        entries.into_iter().map(|entry| entry.name).collect()
    };
    let status = |refused: Result<(), virtx::Error>| refused.unwrap_err().status();

    // The root as a process sees it, but for the kernel's filesystems and the agent's.
    let listed = names("");
    for shown in ["etc", "root", "data"] {
        assert!(listed.iter().any(|name| name == shown), "{listed:?}");
    }
    for hidden in ["proc", "sys", "dev", ".virtx"] {
        assert!(!listed.iter().any(|name| name == hidden), "{listed:?}");
    }
    let gone = sandbox.stat_file("proc/self").wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");

    // What is written here a process reads at once, and the other way round.
    sandbox
        .write_file("root/put", "put\n", None)
        .wait()
        .unwrap();
    assert_eq!(
        common::run(&sandbox, &["cat", "/root/put"]),
        (vec!["put".into()], 0)
    );
    assert_eq!(
        common::run(&sandbox, &["sh", "-c", "echo made > /root/made"]).1,
        0
    );
    assert_eq!(sandbox.read_text("root/made").wait().unwrap(), "made\n");

    // A file larger than one request carries goes and comes back whole.
    let big: Vec<u8> = (0..3_500_000u32).map(|i| (i % 251) as u8).collect();
    sandbox.write_file("root/big", &big, None).wait().unwrap();
    assert_eq!(
        common::run(&sandbox, &["wc", "-c", "/root/big"]).0,
        ["3500000 /root/big"]
    );
    let read = sandbox.read_file("root/big", 0, None).wait().unwrap();
    assert!(read == big, "{} bytes read back", read.len());

    // A file written over keeps its mode, and runs as it did.
    sandbox
        .write_file("root/run", "#!/bin/sh\necho one\n", None)
        .wait()
        .unwrap();
    assert_eq!(common::run(&sandbox, &["chmod", "755", "/root/run"]).1, 0);
    sandbox
        .write_file("root/run", "#!/bin/sh\necho two\n", None)
        .wait()
        .unwrap();
    assert_eq!(
        common::run(&sandbox, &["/root/run"]),
        (vec!["two".into()], 0)
    );

    // Directories are made, and files moved, copied and removed in them.
    sandbox.create_dir_all("work/a/b").wait().unwrap();
    sandbox
        .rename_file("root/put", "work/a/b/moved", false)
        .wait()
        .unwrap();
    sandbox
        .copy_file("work/a/b/moved", "work/copied", false)
        .wait()
        .unwrap();
    assert_eq!(names("work").len(), 2);
    sandbox.remove_file("work", false).wait().unwrap();
    assert_eq!(common::run(&sandbox, &["test", "-e", "/work"]).1, 1);

    // A link is not shown, a read-only volume takes no write, and the root is not removed.
    let link = ["ln", "-s", "/etc/hostname", "/root/link"];
    assert_eq!(common::run(&sandbox, &link).1, 0);
    assert!(!names("root").iter().any(|name| name == "link"));
    let unread = sandbox.read_file("root/link", 0, None).wait().unwrap_err();
    assert_eq!(unread.status(), Some(404), "{unread}");
    assert_eq!(
        status(sandbox.write_file("data/x", "x", None).wait()),
        Some(403)
    );
    assert_eq!(status(sandbox.remove_file("", false).wait()), Some(403));

    // A pipe is neither read nor written, and holds up nothing asked after it.
    assert_eq!(common::run(&sandbox, &["mkfifo", "/root/fifo"]).1, 0);
    assert_eq!(
        status(sandbox.copy_file("root/run", "root/fifo", true).wait()),
        Some(403)
    );
    let unread = sandbox.read_file("root/fifo", 0, None).wait().unwrap_err();
    assert_eq!(unread.status(), Some(404), "{unread}");
    assert_eq!(sandbox.read_text("root/made").wait().unwrap(), "made\n");

    sandbox.remove().wait().unwrap();
    let gone = sandbox.read_dir("").wait().unwrap_err();
    assert_eq!(gone.status(), Some(404), "{gone}");
    volume.remove().wait().unwrap();
}
