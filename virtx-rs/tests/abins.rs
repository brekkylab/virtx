//! virtx's own programs in a sandbox, against the fixture's `virtx-stdio`: `/abin`, the programs
//! a sandbox is made with, from `abin/` beside the server, and `/alib`, what they run, from
//! `alib/` beside it. The fixture always has `abin/`; it has `alib/` only once
//! `cargo xtask build alib` has run, so the tests that need it are ignored without it.

use virtx::{Client, ImageJobEnd, ImageJobEntry, SandboxCreateArgs};

mod common;

use common::{ALPINE, run};

/// The image Debian's tests run in, for a guest on glibc.
const DEBIAN: &str = "debian:bookworm-slim";

/// Pull `reference` onto the server unless it is there already.
fn pulled(client: &Client, reference: &str) {
    if common::tagged(client, reference).is_some() {
        return;
    }
    let mut log = client.pull_image(reference, None).log();
    let mut last = None;
    while let Some(entry) = log.wait_next() {
        last = Some(entry.unwrap().entry);
    }
    assert!(
        matches!(
            last,
            Some(ImageJobEntry::End(ImageJobEnd {
                digest: Some(_),
                ..
            }))
        ),
        "pulling {reference}"
    );
}

/// The programs a sandbox is made with are its `/abin`, read-only and first on `PATH`, with what
/// each program is beside it, and no others.
#[test]
fn a_sandbox_runs_the_programs_in_abin_by_name() {
    let client = common::client();
    common::alpine(&client);
    let sandbox = client
        .create_sandbox(
            &SandboxCreateArgs::new(ALPINE)
                .abin("index")
                .abin("docread")
                .abin("display"),
        )
        .wait()
        .unwrap();
    assert_eq!(sandbox.info().abins, ["index", "docread", "display"]);

    assert_eq!(
        run(&sandbox, &["sh", "-c", "command -v index"]),
        (vec!["/abin/index".into()], 0)
    );
    for program in ["index", "docread"] {
        let skill = format!("/abin/skills/{program}/SKILL.md");
        assert_eq!(run(&sandbox, &["test", "-f", &skill]).1, 0, "{skill}");
    }
    assert_ne!(
        run(&sandbox, &["touch", "/abin/x"]).1,
        0,
        "/abin is read-only"
    );
    assert_ne!(
        run(&sandbox, &["rm", "-f", "/abin/index"]).1,
        0,
        "/abin/index cannot be removed"
    );

    let (lines, code) = run(
        &sandbox,
        &[
            "sh",
            "-c",
            "mkdir -p /root/docs && echo 'the borrow checker' > /root/docs/a.md \
             && cd /root && index init notes.db && index ingest notes.db docs >/dev/null \
             && index search notes.db borrow",
        ],
    );
    assert_eq!(code, 0, "{lines:?}");
    assert!(
        lines.iter().any(|line| line.contains("docs/a.md")),
        "{lines:?}"
    );

    let (lines, code) = run(&sandbox, &["docread", "/root/docs/a.md"]);
    assert_eq!(code, 0, "{lines:?}");
    assert!(
        lines
            .iter()
            .any(|line| line.ends_with("\tthe borrow checker")),
        "{lines:?}"
    );
    sandbox.remove().wait().unwrap();

    let only = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE).abin("index"))
        .wait()
        .unwrap();
    let (lines, code) = run(&only, &["ls", "-R", "/abin"]);
    assert_eq!(code, 0, "{lines:?}");
    assert!(lines.iter().any(|line| line == "index"), "{lines:?}");
    assert!(!lines.iter().any(|line| line == "docread"), "{lines:?}");
    only.remove().wait().unwrap();

    let none = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE))
        .wait()
        .unwrap();
    assert_ne!(
        run(&none, &["test", "-e", "/abin/index"]).1,
        0,
        "a sandbox made with no abins has no /abin"
    );
    none.remove().wait().unwrap();
}

/// `/alib` is read-only, with the X server `display` starts and Vulkan for each C library; a
/// sandbox without a GPU is given none of it on `LD_LIBRARY_PATH`, and no Vulkan driver.
#[test]
#[ignore = "needs alib/ beside the server: `cargo xtask build alib`"]
fn a_sandbox_without_a_gpu_has_alib_and_is_given_none_of_it() {
    let client = common::client();
    common::alpine(&client);
    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE).abin("display"))
        .wait()
        .unwrap();
    assert_eq!(run(&sandbox, &["test", "-x", "/alib/Xvfb"]).1, 0);
    assert_ne!(
        run(&sandbox, &["touch", "/alib/x"]).1,
        0,
        "/alib is read-only"
    );
    let (lines, code) = run(
        &sandbox,
        &["display", "run", "--", "display", "shot", "/tmp/shot.png"],
    );
    assert_eq!(code, 0, "{lines:?}");
    sandbox.remove().wait().unwrap();

    for (image, libc) in [(ALPINE, "musl"), (DEBIAN, "glibc")] {
        pulled(&client, image);
        let sandbox = client
            .create_sandbox(&SandboxCreateArgs::new(image).gpu(false))
            .wait()
            .unwrap();
        assert_eq!(
            run(
                &sandbox,
                &[
                    "sh",
                    "-c",
                    "echo $LD_LIBRARY_PATH; echo ${VK_DRIVER_FILES:-none}"
                ],
            ),
            (vec![String::new(), "none".into()], 0),
            "{image}"
        );
        let vulkan = format!("/alib/{libc}/libvulkan.so.1");
        assert_eq!(run(&sandbox, &["test", "-f", &vulkan]).1, 0, "{image}");
        sandbox.remove().wait().unwrap();
    }
}

/// In a sandbox with a GPU, a command finds `/alib`'s libraries for the image's C library on
/// `LD_LIBRARY_PATH`, and Vulkan reaches the host's through their venus, in an image with no
/// driver of its own: Alpine's `vulkan-tools` brings the loader alone, and Debian bookworm's
/// Mesa has no venus.
#[test]
#[ignore = "needs alib/ beside the server and a GPU on the host"]
fn a_gpu_sandbox_runs_vulkan_with_no_driver_in_the_image() {
    let client = common::client();
    for (image, libc, install) in [
        (ALPINE, "musl", "apk add --no-cache vulkan-tools >/dev/null"),
        (
            DEBIAN,
            "glibc",
            "apt-get update -qq && apt-get install -y -qq vulkan-tools >/dev/null",
        ),
    ] {
        pulled(&client, image);
        let sandbox = client
            .create_sandbox(&SandboxCreateArgs::new(image).gpu(true))
            .wait()
            .unwrap();
        assert_eq!(
            run(&sandbox, &["sh", "-c", "echo $LD_LIBRARY_PATH"]),
            (vec![format!("/alib/{libc}")], 0),
            "{image}"
        );
        let (lines, code) = run(&sandbox, &["sh", "-c", install]);
        assert_eq!(code, 0, "{image}: {lines:?}");
        let (lines, code) = run(&sandbox, &["vulkaninfo", "--summary"]);
        assert_eq!(code, 0, "{image}: {lines:?}");
        assert!(
            lines.iter().any(|line| line.contains("Venus")),
            "{image}: {lines:?}"
        );
        sandbox.remove().wait().unwrap();
    }
}
