//! The Rust package against `virtx-uvm`: the server `Client::spawn_virtx` starts, which
//! `$VIRTX_UVM` points at the one built here. `cargo xtask e2e` sets it, with
//! `$VIRTX_CORE_LIB` and `$VIRTX_HOME`.

use std::{
    env,
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    process::{Command, ExitCode},
};

use virtx_rs::{
    Client, ImageJobEntry, SandboxCreateArgs, SandboxExecArgs, SandboxJobEnd, SandboxJobEntry,
};

const IMAGE: &str = "alpine:3.20";

fn a_pulled_image_boots_a_sandbox_that_runs_a_command() {
    let client = Client::spawn_virtx([] as [&str; 0]).unwrap();
    let pull = client.pull_image(IMAGE, None).wait().unwrap();
    let mut log = client.image_job_log(&pull.id, None);
    let end = loop {
        if let ImageJobEntry::End(end) = log.wait_next().unwrap().unwrap().entry {
            break end;
        }
    };
    let digest = end.digest.unwrap_or_else(|| panic!("{:?}", end.error));
    assert!(digest.starts_with("sha256:"), "{digest}");
    let images = client.list_image().wait().unwrap();
    assert!(
        images.iter().any(|image| image.digest == digest),
        "{images:?}"
    );

    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new(IMAGE))
        .wait()
        .unwrap();
    assert!(sandbox.running);
    assert_eq!(sandbox.image, digest);
    let job = client
        .exec_sandbox(
            &sandbox.id,
            &SandboxExecArgs::new(["sh", "-c", "echo one; echo two >&2; exit 3"]),
        )
        .wait()
        .unwrap();
    let mut log = client.sandbox_job_log(&sandbox.id, &job.id, None);
    let mut entries = Vec::new();
    while let Some(entry) = log.wait_next() {
        entries.push(entry.unwrap().entry);
    }
    let end = entries.pop();
    // stdout and stderr are read apart, so one's lines may come before the other's.
    entries.sort_by_key(|entry| format!("{entry:?}"));
    assert_eq!(
        entries,
        [
            SandboxJobEntry::Stderr("two".into()),
            SandboxJobEntry::Stdout("one".into()),
        ]
    );
    assert_eq!(end, Some(SandboxJobEntry::End(SandboxJobEnd { code: 3 })));

    client.remove_sandbox(&sandbox.id).wait().unwrap();
    let gone = client.get_sandbox(&sandbox.id).wait().unwrap_err();
    assert!(gone.to_string().contains("404"), "{gone}");
}

fn the_command_pulls_through_the_server_it_starts() {
    // This binary is in `target/<profile>/deps`, the CLI one up.
    let exe = env::current_exe().unwrap();
    let cli: PathBuf = exe
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("virtx{}", env::consts::EXE_SUFFIX));
    let pulled = Command::new(cli)
        .args(["pull", "-q", IMAGE])
        .output()
        .unwrap();
    assert!(
        pulled.status.success(),
        "{}",
        String::from_utf8_lossy(&pulled.stderr)
    );
    let stdout = String::from_utf8(pulled.stdout).unwrap();
    let mut words = stdout.split_whitespace();
    assert_eq!(words.next(), Some(IMAGE));
    assert!(
        words
            .next()
            .is_some_and(|digest| digest.starts_with("sha256:")),
        "{stdout}"
    );
}

fn main() -> ExitCode {
    let missing: Vec<_> = ["VIRTX_CORE_LIB", "VIRTX_UVM", "VIRTX_HOME"]
        .into_iter()
        .filter(|name| env::var_os(name).is_none_or(|value| value.is_empty()))
        .collect();
    if !missing.is_empty() {
        eprintln!(
            "{} unset: run these with `cargo xtask e2e`",
            missing.join(", ")
        );
        return ExitCode::FAILURE;
    }
    virtx_rs::load(env::var_os(virtx_rs::LIBRARY_ENV).unwrap()).unwrap();

    let tests: [(&str, fn()); 2] = [
        (
            "a_pulled_image_boots_a_sandbox_that_runs_a_command",
            a_pulled_image_boots_a_sandbox_that_runs_a_command,
        ),
        (
            "the_command_pulls_through_the_server_it_starts",
            the_command_pulls_through_the_server_it_starts,
        ),
    ];
    let filter = env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let mut failed = 0;
    for (name, test) in tests {
        if filter
            .as_deref()
            .is_some_and(|filter| !name.contains(filter))
        {
            continue;
        }
        let passed = panic::catch_unwind(AssertUnwindSafe(test)).is_ok();
        println!("test {name} ... {}", if passed { "ok" } else { "FAILED" });
        failed += usize::from(!passed);
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
