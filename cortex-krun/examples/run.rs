//! Run a command in an OCI image, on a writable layer of its own.
//!
//!   cargo run --example run
//!   cargo run --example run -- alpine:3.21 'uname -srm'
//!
//! The image is taken from the cache, and pulled only if it is not there -- so this runs
//! an image a registry never served just as well as one it did:
//!
//!   cargo run --example run -- localhost/cortex-krun-build:<digest> 'jq --version'
//!
//! The first run of something not yet here pulls it and stitches it into a disk; later
//! ones find both in the cache. The session's writes go to `data/sessions/<image>.ext4`,
//! so a second run sees what the first one did:
//!
//!   cargo run --example run -- alpine:3.21 'echo hello > /root/note'
//!   cargo run --example run -- alpine:3.21 'cat /root/note'
//!
//! Deleting that file is how the session starts over.
//!
//! A third argument is a host directory the guest sees at `/mnt`, read-only:
//!
//!   cargo run --example run -- alpine:3.21 'ls /mnt' ./scripts
//!
//! A session reaches nothing by default. `CORTEX_NET` is how much of the network it
//! gets -- `none`, `host`, `public` or `full` -- and the guest is configured over DHCP
//! before the command runs, so an image needs nothing in it:
//!
//!   CORTEX_NET=public cargo run --example run -- alpine:3.21 'wget -qO- http://example.com'
//!   CORTEX_NET=public cargo run --example run -- alpine:3.21 'apk add curl'
//!
//! `CORTEX_HOST_PORTS` is a comma-separated grant of TCP ports on this machine, reached
//! at the gateway the guest's default route already names:
//!
//!   CORTEX_HOST_PORTS=8080 CORTEX_NET=host cargo run --example run -- \
//!       alpine:3.21 'wget -qO- http://10.0.2.1:8080'
//!
//! `CORTEX_GPU=1` asks for guest Vulkan, which needs the `gpu` feature to be anything
//! more than a flag and an image carrying Mesa's venus driver:
//!
//!   CORTEX_GPU=1 cargo run --features gpu --example run -- alpine:3.21 'vulkaninfo --summary'
//!
//! Nothing here is host-specific: [`session`](cortex_krun::session) attaches the guest's
//! console itself, which is the one call that would otherwise differ between them. The cargo
//! runner is, though, and both are registered in `.cargo/config.toml`: on macOS
//! `scripts/macos-run.sh` ad-hoc signs the binary with
//! `com.apple.security.hypervisor`, which Hypervisor.framework will not start a VM
//! without, and on Windows `scripts/windows-run.ps1` puts `libkrunfw.dll` on `PATH`.
//!
//! **This boots a machine directly, which is not what a session does.** A console client
//! spawns `cortex-krun`, which spawns the signed boot beside it; what this example shows is
//! the layer under both -- an image, a disk, and a command -- with no protocol over it. So
//! there is no agent here and no `/abin`: those are the server's, not the library's.

use std::path::PathBuf;

use cortex_krun::image;
use cortex_krun::session::{Ext4, Mount, Network, Session};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The machine is entered on this thread, so libkrun logs here rather than in a spawned
    // host: `KRUN_LOG=debug` is what makes a device that refuses to activate say why.
    cortex_krun::session::log_from_env();

    let mut args = std::env::args().skip(1);
    let reference = args.next().unwrap_or_else(|| "alpine:latest".to_string());
    let command = args.next();
    let share = args.next();

    // **Provision first, and pull only if that says the image was never here.** The
    // cache holds images a registry never served -- what a build left behind is one, and
    // so is anything `commit` filed -- and reaching for a registry first would ask
    // `localhost` for those and fail on an image that is sitting in the cache.
    //
    // `provision` is the question and the answer at once: it stitches the layers into the
    // disk a boot attaches, and the one failure that means "nobody pulled this" is the one
    // worth a round trip. Anything else it says is about this machine and is reported.
    //
    // The pull is the only async thing here, and the only thing that touches the network.
    // A machine is entered on this thread, so the runtime is over before anything of
    // libkrun's starts.
    match image::provision(reference.as_str(), None) {
        // Already here, and now stitched. The session below asks for the same disk and
        // finds it made.
        Ok(_) => {}
        Err(image::Error::NotPulled(_)) => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(image::pull(reference.as_str(), None, None))?;
            drop(runtime);
        }
        // Anything else is about this machine rather than about the image being absent,
        // and a pull would not answer it.
        Err(e) => return Err(e.into()),
    }

    // One session per image, kept between runs -- named after the image with the
    // characters a path cannot hold replaced, since a reference holds `/` and `:`.
    let named: String = reference
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                true => c,
                false => '_',
            },
        )
        .collect();
    // `data/` is at the repo root, a directory up from this package.
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let path = repo.join("data/sessions").join(format!("{named}.ext4"));

    eprintln!("session: {}", path.display());

    // Formatted here if this is the first run over that path, and found as it was left if
    // it is not -- which is what makes a later run carry on from this one. `data/sessions`
    // is made on the way. A session handed no rootfs would get a throwaway instead,
    // unlinked as soon as the machine had it.
    let rootfs = Ext4::at(path, Ext4::SIZE)?;

    // What to run: a shell around whatever was asked for, and the image's own command
    // when nothing was.
    let mut session = Session::new().image(reference.as_str()).rootfs(rootfs);
    if let Some(script) = command {
        session = session.command(["/bin/sh".to_string(), "-c".to_string(), script]);
    }

    // Read-only, which the host enforces: the guest may read what is shared and the
    // virtiofs server refuses its writes. `/mnt` need not exist in the image -- the guest
    // makes it on the overlay.
    if let Some(dir) = share {
        eprintln!("mount: {dir} -> /mnt (read-only)");
        session = session.mount(Mount::new(dir, "/mnt").read_only());
    }

    // A level rather than a switch: what the guest may reach, and never how it reaches
    // it. An unknown word is refused rather than guessed at -- deciding a sandbox's
    // egress on the caller's behalf is the one thing this must not do.
    let asked = std::env::var("CORTEX_NET").unwrap_or_else(|_| "none".to_string());
    let network = match asked.as_str() {
        "none" => Network::Disabled,
        "host" => Network::HostOnly,
        "public" => Network::Public,
        "full" => Network::Full,
        other => {
            return Err(format!("CORTEX_NET={other} is not none, host, public or full").into());
        }
    };
    eprintln!("network: {}", network.as_str());
    session = session.network(network);

    if let Ok(granted) = std::env::var("CORTEX_HOST_PORTS") {
        let ports = granted
            .split(',')
            .map(str::trim)
            .filter(|port| !port.is_empty())
            .map(str::parse::<u16>)
            .collect::<Result<Vec<_>, _>>()?;
        eprintln!("host ports: {ports:?}");
        session = session.host_ports(ports);
    }

    // Ignored by a build without the `gpu` feature, which is what makes it safe to read
    // an environment variable rather than gate the example on the feature.
    if std::env::var_os("CORTEX_GPU").is_some_and(|on| on == "1") {
        session = session.gpu(true);
    }

    // Does not return: the VMM exits this process with the guest's status.
    session.enter()?;
    unreachable!("a session that was entered does not come back")
}
