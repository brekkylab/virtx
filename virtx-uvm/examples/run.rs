//! Run a command in an OCI image, on a writable layer of its own.
//!
//!   cargo xtask run --example run
//!   cargo xtask run --example run -- alpine:3.21 'uname -srm'
//!
//! The image is taken from the cache, and pulled only if it is not there -- so this runs
//! an image a registry never served just as well as one it did:
//!
//!   cargo xtask run --example run -- localhost/virtx-uvm-build:<digest> 'jq --version'
//!
//! The first run of something not yet here pulls it and makes its root filesystem; later
//! ones find both in the cache. The session's writes go to `data/sessions/<image>.ext4`,
//! so a second run sees what the first one did:
//!
//!   cargo xtask run --example run -- alpine:3.21 'echo hello > /root/note'
//!   cargo xtask run --example run -- alpine:3.21 'cat /root/note'
//!
//! Deleting that file is how the session starts over.
//!
//! A third argument is a host directory the guest sees at `/mnt`, read-only:
//!
//!   cargo xtask run --example run -- alpine:3.21 'ls /mnt' ./scripts
//!
//! A session reaches nothing by default. `VIRTX_NET=1` gives it what this machine reaches,
//! its loopback included, and an image needs nothing in it for that:
//!
//!   VIRTX_NET=1 cargo xtask run --example run -- alpine:3.21 'wget -qO- http://example.com'
//!   VIRTX_NET=1 cargo xtask run --example run -- alpine:3.21 'apk add curl'
//!
//! `VIRTX_PUBLISH` is a comma-separated list of `<host>:<console>` ports, each a listener on
//! this machine's loopback that reaches the console port in the guest:
//!
//!   VIRTX_PUBLISH=8080:80 VIRTX_NET=1 cargo xtask run --example run -- \
//!       alpine:3.21 'apk add -q busybox-extras && httpd -f -p 80'
//!
//! `VIRTX_GPU=1` asks for guest Vulkan, which needs the `gpu` feature to be anything
//! more than a flag and an image carrying Mesa's venus driver:
//!
//!   VIRTX_NET=1 VIRTX_GPU=1 cargo xtask run --features gpu --example run -- alpine:latest \
//!       'apk add mesa-vulkan-virtio vulkan-loader vulkan-tools && vulkaninfo --summary'
//!
//! Nothing here is host-specific: [`machine`](virtx_uvm::machine) attaches the guest's
//! console itself, which is the one call that would otherwise differ between them. Running
//! it is, though, which is why it goes through `cargo xtask run`: on macOS that ad-hoc signs
//! the binary with `com.apple.security.hypervisor`, which Hypervisor.framework will not start
//! a VM without, and on Windows it puts `libkrunfw.dll` on `PATH`.
//!
//! **This boots a machine directly, which is not what a session does.** A console client
//! spawns `virtx-uvm`, which spawns the signed boot beside it; what this example shows is
//! the layer under both -- an image, a root filesystem, and a command -- with no protocol over it. So
//! there is no agent here and no `/abin`: those are the server's, not the library's.

use std::path::PathBuf;

use virtx_uvm::cache::Cache;
use virtx_uvm::filesystem::Tree;
use virtx_uvm::machine::{Machine, Mount, Upper};
use virtx_uvm::registry::{Reference, Registry};
use virtx_uvm::rootfs::Rootfs;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The machine is entered on this thread, so libkrun logs here rather than in a spawned
    // host: `KRUN_LOG=debug` is what makes a device that refuses to activate say why.
    virtx_uvm::machine::log_from_env();

    let mut args = std::env::args().skip(1);
    let reference = args.next().unwrap_or_else(|| "alpine:latest".to_string());
    let command = args.next();
    let share = args.next();

    // **The cache first, and a registry only for an image that is not in it.** The cache
    // holds images a registry never served -- what a build left behind is one -- and
    // reaching for a registry first would ask `localhost` for those and fail on an image
    // that is sitting right here.
    //
    // The pull is the only async thing here, and the only thing that touches the network.
    // A machine is entered on this thread, so the runtime is over before anything of
    // libkrun's starts.
    let cache = Cache::default();
    let parsed: Reference = reference.parse()?;
    let image = match cache.find(&parsed)? {
        Some(image) => image,
        None => {
            let runtime = tokio::runtime::Runtime::new()?;
            let pulled = runtime.block_on(Registry::default().pull(parsed, &cache))?;
            drop(runtime);
            pulled
        }
    };

    // Its root filesystem, made the first time those layers are booted and found after.
    let at = cache
        .rootfs_path(&image.layers)
        .ok_or("an image with no layers is not a filesystem to boot")?;
    let rootfs = match Rootfs::open(&at) {
        Some(rootfs) => rootfs,
        None => {
            let layers: Vec<_> = image.layers.iter().map(|l| cache.layer_path(l)).collect();
            Rootfs::create(&layers, &at)?
        }
    };

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

    // Found as it was left if an earlier run made it -- which is what makes a later run carry
    // on from this one -- and made here if not, `data/sessions` on the way. A session handed
    // no upper layer would get a throwaway instead, unlinked as soon as the machine had it.
    let upper = match Upper::open(&path) {
        Some(upper) => upper,
        None => Upper::create(&path, Upper::SIZE, Tree::new())?,
    };

    // What to run: a shell around whatever was asked for, and the image's own command
    // when nothing was.
    let mut session = Machine::new(rootfs, image.config).upper(upper);
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

    let network = std::env::var_os("VIRTX_NET").is_some_and(|on| on == "1");
    eprintln!("network: {}", if network { "on" } else { "off" });
    session = session.network(network);

    if let Ok(published) = std::env::var("VIRTX_PUBLISH") {
        for port in published
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            let (host, console) = port
                .split_once(':')
                .ok_or_else(|| format!("VIRTX_PUBLISH: {port} is not <host>:<console>"))?;
            let listener = std::net::TcpListener::bind(("127.0.0.1", host.parse::<u16>()?))?;
            eprintln!(
                "publish: 127.0.0.1:{} -> {console}",
                listener.local_addr()?.port()
            );
            session = session.publish(console.parse()?, listener);
        }
    }

    // Ignored by a build without the `gpu` feature, which is what makes it safe to read
    // an environment variable rather than gate the example on the feature.
    if std::env::var_os("VIRTX_GPU").is_some_and(|on| on == "1") {
        session = session.gpu(true);
    }

    // Does not return: the VMM exits this process with the guest's status.
    session.enter()?;
    unreachable!("a session that was entered does not come back")
}
