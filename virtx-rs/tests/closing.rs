//! A client of `virtx-stdio` closed takes its sandboxes' machines with it.
//!
//! A file of its own, so its binary runs alone: it looks for machines by the fixture's home, and
//! another test's machines running beside it would be found too.

use std::{env, process::Command, process::Stdio, thread, time::Duration};

use virtx::SandboxCreateArgs;

mod common;

use common::ALPINE;

#[test]
fn a_client_closed_takes_its_sandboxes_machines_with_it() {
    let client = common::client();
    common::alpine(&client);
    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new(ALPINE))
        .wait()
        .unwrap();
    // A machine's process names the home on its command line, in its rootfs's path.
    let home = env::var("VIRTX_HOME").expect("the fixture names its home");
    let running = || {
        Command::new("pgrep")
            .arg("-f")
            .arg(&home)
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success()
    };
    assert!(running());

    // The sandbox holds the session too: the last hold let go of ends it.
    drop(sandbox);
    client.close().wait().unwrap();
    for _ in 0..50 {
        if !running() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("a sandbox's machine outlived its client");
}
