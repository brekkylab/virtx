//! A build cancelled while a step runs takes the step's machine with it.
//!
//! A file of its own, so its binary runs alone: it looks for machines by the fixture's home, and
//! another test's machines running beside it would be found too.

use std::{env, process::Command, process::Stdio, thread, time::Duration};

use virtx::{ImageJobEntry, Recipe, Step};

mod common;

use common::ALPINE;

#[test]
fn a_cancelled_build_takes_its_machine_with_it() {
    let client = common::client();
    common::alpine(&client);
    let reference = format!("{}:latest", common::unique("never"));
    let recipe = Recipe::new(ALPINE).step(Step::run("echo started; sleep infinity"));
    let job = client.build_image(recipe, &reference).wait().unwrap();
    let mut log = job.log(None);
    loop {
        match log.wait_next().unwrap().unwrap().entry {
            ImageJobEntry::Log(line) if line == "started" => break,
            ImageJobEntry::End(end) => panic!("{end:?}"),
            _ => {}
        }
    }
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

    job.remove().wait().unwrap();
    for _ in 0..50 {
        if !running() {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("the build's machine outlived its job");
}
