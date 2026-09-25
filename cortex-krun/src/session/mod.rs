//! Answering a console session: the wire, and what is run at the far end of it.

mod build;
mod halves;
mod machine;

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use cortex::console::stdio::StdioServer;
use cortex::console::{
    Call, Error, ExecCall, InitCall, InitResp, MAX_PAYLOAD, Message, MountSpec, Notification,
    Response, Server, SnapshotResp,
};
use cortex_krun::session::{Ext4, Network};
use cortex_krun_host::{ABIN_DIR, GUEST_DIR, guest_program};

use machine::{HOST_PORTS_ENV, NETWORK_ENV};

use machine::{Machine, Mount, Shape};

/// A machine that has been booted and told what the session is, with the guest's own answer
/// to where it stands -- see [`InitResp::cwd`].
type Booted = Pin<Box<dyn Future<Output = anyhow::Result<(Machine, Option<String>)>>>>;

/// How to boot another. Held rather than the machine alone, because a `stop` hands one back
/// and the call after it takes a second -- on the same disk, which is what makes it the same
/// session.
type Factory = Box<dyn Fn() -> Booted>;

/// What a session is on this side, once a client has said what it is.
///
/// **The disk is the session**, and the other two are what anything but an `exec` needs to
/// know about it: how to put a machine back over it, and which of the paths on it are this
/// backend's own rather than the session's work.
struct Session {
    boot: Factory,
    /// Where the session's writes are. Outlives every machine started on it, and is what a
    /// `snapshot` reads.
    upper: Ext4,
    /// Guest paths a snapshot must not carry, as the guest spells them: the guest binary's
    /// share, and every mount point the boot made to hang a tree on. Making a directory is a
    /// write, and a write lands on the upper -- so without these a snapshot would claim
    /// directories nobody asked for. See [`Ext4::to_blob`].
    skip: Vec<String>,
}

/// Answer a console session on stdin and stdout, until the client ends it.
pub async fn run() -> anyhow::Result<()> {
    // Taken for the life of the process: from here on stdout carries frames and nothing else,
    // which is why every diagnostic goes to stderr. Nothing enforces that -- it is a rule,
    // and this is where it starts applying.
    let mut server = StdioServer::stdio()?;

    sweep();

    // What the session is, once a client has said. None until then, which is what every
    // other call is answered against.
    let mut session: Option<Session> = None;
    // Whatever it holds is dropped on `stop` and on the way out of this function whichever
    // way it leaves, which takes the machine down every time.
    let mut machine: Option<Machine> = None;

    while let Some(message) = server.recv().await? {
        match message {
            Message::Request {
                id,
                call: Call::Init(_),
            } if session.is_some() => {
                server
                    .respond(
                        id,
                        Response::Error(Error::new(
                            Error::INVALID_REQUEST,
                            "this session has already been told what it is",
                        )),
                    )
                    .await?;
            }

            Message::Request {
                id,
                call: Call::Init(init),
            } => {
                let answered = match start(init).await {
                    Ok((told, started, answer)) => {
                        session = Some(told);
                        machine = Some(started);
                        Response::Init(answer)
                    }
                    Err(e) => Response::Error(refused(&e)),
                };
                server.respond(id, answered).await?;
            }

            // **The one call the guest does not answer.** What a snapshot is here is the
            // disk, and the disk is this end's -- one root up from the guest, which is why
            // the guest refuses this and says so. See [`snapshot`].
            Message::Request {
                id,
                call: Call::Snapshot(_),
            } => {
                let answered = match snapshot(&mut machine, &session).await {
                    Ok(blob) => Response::Snapshot(SnapshotResp { blob }),
                    Err(e) => Response::Error(refused(&e)),
                };
                server.respond(id, answered).await?;
            }

            // Everything else is the guest's to answer.
            Message::Request { id, call } => {
                let (answered, lost) = match up(&mut machine, &session).await {
                    // Whatever the guest said, verbatim.
                    Ok(up) => match up.call(call).await {
                        Ok(answer) => (answer, false),
                        // No answer is coming, and none will for anything else on this
                        // channel either.
                        Err(e) => (Response::Error(refused(&e)), true),
                    },
                    Err(e) => (Response::Error(refused(&e)), false),
                };
                // Releasing it is what makes the next call a fresh boot rather than a second
                // failure. After the borrow above and not inside it.
                if lost {
                    machine = None;
                }
                server.respond(id, answered).await?;
            }

            // Only ever after a `stop`, since `init` leaves a machine up: what this is worth
            // is taking one back before the call that would otherwise have paid for it.
            // Nothing answers this, so a failure is only said here -- the next call that
            // needs a machine tries again and tells whoever asked for it.
            Message::Notification(Notification::Start) => {
                if let Err(e) = up(&mut machine, &session).await {
                    eprintln!("{}: booting: {e:#}", env!("CARGO_BIN_NAME"));
                }
            }

            // **Not merely dropping the machine.** A machine is stopped by killing it, and
            // a guest killed with writes still in its page cache never put them on the
            // disk -- so a bare drop here would lose whatever the session wrote last, both
            // to the `start` that carries it on and to the `snapshot` that reads it. So
            // the guest is asked to flush first, and the disk is left saying it was left
            // cleanly. Nothing answers a notification, so a failure is only said here.
            Message::Notification(Notification::Stop) => {
                if let Err(e) = quiesce(&mut machine, &session).await {
                    eprintln!("{}: stopping: {e:#}", env!("CARGO_BIN_NAME"));
                }
            }

            // The session is over.
            Message::Notification(Notification::Quit) => break,

            // A response answers a request, and this end makes none of its own on the channel
            // it reads.
            Message::Response { id, .. } => eprintln!(
                "{}: a response arrived for request {id}, which nobody made",
                env!("CARGO_BIN_NAME")
            ),
        }
    }

    // The machine first, because what it is holding is under what goes next, and then the
    // session's own directory -- the disk a `stop` was careful to leave. A session that ends
    // any way that runs this leaves nothing behind; one killed outright is the next server's
    // sweep to reclaim.
    drop(machine);
    let _ = std::fs::remove_dir_all(sessions().join(std::process::id().to_string()));
    Ok(())
}

/// Take a session's shape, put its image where a boot can reach it, and start one.
///
/// What comes back is how to start another -- a `stop` hands the machine back and the call
/// after it takes a second one, on the same disk -- the one that is up, and what the client
/// has to know about it.
async fn start(init: InitCall) -> anyhow::Result<(Session, Machine, InitResp)> {
    // How far this session may reach, and which doors on this machine it is granted.
    // Settled before anything is built, because it is the one part of an `init` a client can
    // be told it got wrong while the answer still costs it nothing: a reach decides whether a
    // virtio-net device is attached, and a device is attached before a kernel comes up.
    let (network, host_ports) = reach(&init)?;

    // And how big a machine, for the same reason and at the same moment: a vCPU count and a
    // memory size are fixed when a machine is made, a GPU is a device attached before a kernel
    // comes up, and all three are cheaper to refuse now than after an image has been pulled.
    let shape = shape(&init)?;

    // And how much it may write, which is fixed when its disk is formatted.
    let size = disk(&init)?;

    let image = init.image.clone().ok_or_else(|| {
        coded(
            Error::INVALID_PARAMS,
            anyhow::anyhow!(
                "a session here runs in a micro-VM, which boots on an image, and this one \
                 named none"
            ),
        )
    })?;
    anyhow::ensure!(!image.base.is_empty(), "the image named no base");

    // What this session actually boots on. The base itself when that is all the client
    // declared, and otherwise an image built here over it -- which is a machine of its own,
    // run and finished before the session's is started. Either way what comes back is a name
    // in the cache, and everything after this point cannot tell the two apart.
    let booting = if image.steps.is_empty() {
        image.base.clone()
    } else {
        build::build(&image, network).await?
    };

    // Into the cache, so that the boot -- which asks no registry anything -- finds it. Done
    // here and not there because this is the end with a client to tell, and a name nobody can
    // pull is the one failure at `init` that is about what was asked for rather than about
    // this machine.
    pulled(&booting).await?;

    // Where each tree is on this side, against the one path in the guest that names it. Read
    // before anything is built, because a session whose trees can never be there is one in
    // which every later path is a lie -- see [`mounts`].
    let mounts = mounts(&init)?;

    // The disk the session's own writes go on, which outlives every machine started on it --
    // that file *is* the session. See [`crate::home`].
    //
    // Made here and not in the boot, because it is made **once**: a client that handed back
    // an earlier session's writes gets them written into this file now, and a `stop` and the
    // boot after it must find what is there rather than lay the layer down a second time.
    let disk = sessions()
        .join(std::process::id().to_string())
        .join("session.ext4");
    let upper = seeded(&disk, size, init.snapshot.as_deref())?;

    // What a snapshot must leave out: this backend's own two shares, and every directory the
    // boot made to hang one of the client's trees on.
    let skip = [GUEST_DIR.to_string(), ABIN_DIR.to_string()]
        .into_iter()
        .chain(mounts.iter().map(|mount| mount.at.clone()))
        .collect();

    // Cloned per boot rather than borrowed, because what this closure is for is outliving the
    // machine it makes.
    let at = upper.path.clone();
    let boot: Factory = Box::new(move || {
        let (booting, mounts, host_ports, at) = (
            booting.clone(),
            mounts.clone(),
            host_ports.clone(),
            at.clone(),
        );
        Box::pin(async move {
            let mut machine =
                Machine::boot(&booting, &at, &mounts, network, &host_ports, shape).await?;
            let cwd = configure(&mut machine, &mounts).await?;
            Ok((machine, cwd))
        })
    });

    // Where the session stands is the image's business and only the guest has read it. Where
    // the trees went is not answered at all: the client named the path each one appears at,
    // so there is nothing this end knows about them that the client did not say.
    let (started, cwd) = boot().await.map_err(|e| coded(Error::BOOT_FAILED, e))?;
    Ok((Session { boot, upper, skip }, started, InitResp { cwd }))
}

/// The disk this session runs on: a fresh one, with `snapshot` already written into it when
/// the client handed one back.
///
/// **What an `init` carrying a snapshot means here.** The blob is an OCI layer archive --
/// the files an earlier session wrote, and, for each one it deleted, the whiteout that says
/// so -- which is exactly what this backend's upper layer holds, so there is nothing to
/// translate beyond the compression a client may have put on it. Gzip is recognised by what
/// it is rather than by anything the client says, because the protocol carries no media type
/// and both forms are a layer.
///
/// A blob that is not a layer is [`INVALID_PARAMS`](Error::INVALID_PARAMS): what has to
/// change is what was sent, not this server.
fn seeded(disk: &Path, size: u64, snapshot: Option<&[u8]>) -> anyhow::Result<Ext4> {
    // A file here is an earlier `init`'s that did not finish -- this runs only while the
    // session has not been told what it is, and the sweep took every other process's on the
    // way in. So it is nobody's session, and a client retrying an `init` that failed at the
    // boot must not be refused over what that attempt left.
    let _ = std::fs::remove_file(disk);

    let Some(blob) = snapshot else {
        return Ok(Ext4::at(disk, size)?);
    };

    /// What a gzip stream starts with, which is the whole of how one is told from a tar.
    const GZIP: &[u8] = &[0x1f, 0x8b];

    let mut tar = Vec::new();
    cortex_krun::image::decompress_blob(
        if blob.starts_with(GZIP) { "gzip" } else { "" },
        blob,
        &mut tar,
    )
    .map_err(|e| coded(Error::INVALID_PARAMS, anyhow::Error::new(e)))?;

    Ext4::seeded(disk, size, &mut std::io::Cursor::new(tar))
        .map_err(|e| coded(Error::INVALID_PARAMS, anyhow::Error::new(e)))
}

/// How much of the network this session gets, and which ports on this machine.
///
/// The client's if it said, and this server's own environment if it did not -- see
/// [`NETWORK_ENV`]. Every name the protocol defines is one this backend can answer, so the
/// only refusals are a name nobody defined and a grant that cannot mean anything.
fn reach(init: &InitCall) -> anyhow::Result<(Network, Vec<u16>)> {
    let named = |name: &str| {
        Network::parse(name).ok_or_else(|| {
            coded(
                Error::UNSUPPORTED_NETWORK,
                anyhow::anyhow!(
                    "{name}: no such reach -- this server answers none, host, public and full"
                ),
            )
        })
    };

    let (network, host_ports) = match init.network.as_ref() {
        Some(asked) => (named(&asked.reach)?, asked.host_ports.clone()),
        None => (
            named(&std::env::var(NETWORK_ENV).unwrap_or_else(|_| "none".into()))
                .map_err(|e| anyhow::anyhow!("{NETWORK_ENV}: {e}"))?,
            // Parsed rather than passed on as the string it was written as: a boot is told
            // ports and not a spelling of them, so a typo is this server's to report, by the
            // name of the variable that carried it.
            std::env::var(HOST_PORTS_ENV)
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|port| !port.is_empty())
                .map(|port| {
                    port.parse::<u16>().map_err(|_| {
                        coded(
                            Error::INVALID_PARAMS,
                            anyhow::anyhow!("{HOST_PORTS_ENV}: {port} is not a port number"),
                        )
                    })
                })
                .collect::<anyhow::Result<_>>()?,
        ),
    };

    // A door onto a machine the guest has no way to send a packet to is not a narrower
    // session, it is two settings that cannot both have been meant. Said now, while the
    // client can still pick which one it wanted.
    if network == Network::Disabled && !host_ports.is_empty() {
        return Err(coded(
            Error::INVALID_PARAMS,
            anyhow::anyhow!(
                "host_ports {host_ports:?} were granted to a session that asked for no \
                 network -- a port on this host needs a device to reach it through"
            ),
        ));
    }
    Ok((network, host_ports))
}

/// How big a machine this session gets, and whether it has a Vulkan device.
///
/// The client's where it said and this server's own environment where it did not -- see
/// [`Shape::from_env`] -- which is [`reach`]'s rule, and here for [`reach`]'s reason: the end
/// that knows what this host can spare is the one that started this process, and the end that
/// knows what the work needs is the one asking for it -- so the client's word stands where it
/// has one, member by member, and the operator's where it has none.
///
/// Two refusals, and the difference is what has to change. A machine nothing can run on --
/// no vCPU, or no memory -- is [`INVALID_PARAMS`](Error::INVALID_PARAMS), because the number
/// is what has to change; so is a GPU memory size beside no GPU, since the request describes a
/// device it did not ask for. A machine bigger than this host -- its RAM and its GPU's memory
/// together past the host's, which on a GPU sharing the host's memory is what both are drawn
/// from -- is [`UNSUPPORTED_MACHINE`](Error::UNSUPPORTED_MACHINE). A GPU this build cannot attach is
/// [`UNSUPPORTED_MACHINE`](Error::UNSUPPORTED_MACHINE), because the request is well formed and
/// what is missing is a boot built with `gpu` on a host with virglrenderer -- and it is said
/// here, at `init`, rather than left to a boot that would otherwise come up without the device
/// and never mention it.
fn shape(init: &InitCall) -> anyhow::Result<Shape> {
    // The client's zero and not the environment's: a frame asking for a machine nothing runs
    // on is a frame to answer, where an override nobody can act on costs the override and
    // nothing else -- see [`Shape::from_env`].
    for (what, given) in [
        ("vcpus", init.vcpus.map(u64::from)),
        ("memory_mib", init.memory_mib.map(u64::from)),
    ] {
        if given == Some(0) {
            return Err(coded(
                Error::INVALID_PARAMS,
                anyhow::anyhow!("{what}: 0 is a machine nothing can run on"),
            ));
        }
    }

    let default = Shape::from_env();
    let gpu = init.gpu.unwrap_or(default.gpu);
    // Asked of the boot and not of this build: the file that attaches the device is the one
    // that was built with it -- see [`machine::can_gpu`]. Asked only when something wants one,
    // so a session that does not pays nothing for the question.
    if gpu && !machine::can_gpu()? {
        return Err(coded(
            Error::UNSUPPORTED_MACHINE,
            anyhow::anyhow!(
                "a GPU was asked for and this boot has none to attach -- it is built with the \
                 `gpu` feature, on a host that has virglrenderer"
            ),
        ));
    }

    if init.gpu_memory_mib == Some(0) {
        return Err(coded(
            Error::INVALID_PARAMS,
            anyhow::anyhow!("gpu_memory_mib: 0 is a GPU that can hold nothing"),
        ));
    }
    // The client's size is refused without a GPU; the operator's is only a default, and
    // means nothing to a session that has none.
    if init.gpu_memory_mib.is_some() && !gpu {
        return Err(coded(
            Error::INVALID_PARAMS,
            anyhow::anyhow!("gpu_memory_mib: a size is of a GPU, and this session has none"),
        ));
    }
    let memory_mib = init.memory_mib.or(default.memory_mib);
    let gpu_memory_mib = gpu
        .then(|| init.gpu_memory_mib.or(default.gpu_memory_mib))
        .flatten();

    // Only a size that was named is held to the host: libkrun's default window is what every
    // GPU session had before there was a member to name one.
    if let (Some(gpu_mib), Some(host)) = (gpu_memory_mib, cortex_krun::session::host_memory()) {
        let ram_mib = memory_mib.unwrap_or(cortex_krun::session::Session::DEFAULT_RAM_MIB);
        let asked = (u64::from(ram_mib) + u64::from(gpu_mib)) << 20;
        if asked > host {
            return Err(coded(
                Error::UNSUPPORTED_MACHINE,
                anyhow::anyhow!(
                    "memory_mib {ram_mib} and gpu_memory_mib {gpu_mib} together are more than \
                     this host's {} MiB",
                    host >> 20
                ),
            ));
        }
    }

    Ok(Shape {
        vcpus: init.vcpus.or(default.vcpus),
        memory_mib,
        gpu,
        gpu_memory_mib,
    })
}

/// How large this session's disk is, in bytes: the client's `disk_gib` if it said, and this
/// server's own default if it did not -- see [`machine::DISK_ENV`].
///
/// A ceiling on a sparse file rather than space taken from the host, so there is nothing to
/// hold it against here. `0` is [`INVALID_PARAMS`](Error::INVALID_PARAMS), for the reason a
/// vCPU count of zero is.
fn disk(init: &InitCall) -> anyhow::Result<u64> {
    match init.disk_gib {
        Some(0) => Err(coded(
            Error::INVALID_PARAMS,
            anyhow::anyhow!("disk_gib: 0 is a disk nothing can be written on"),
        )),
        Some(gib) => Ok(u64::from(gib) << 30),
        None => Ok(machine::disk_from_env()),
    }
}

/// Tell the guest what the session is, in the one spelling both ends use.
///
/// The client's `init` named its trees by host URL; the guest's copy names them by the guest
/// mount point, because that is where they are from in there and what every later call is
/// spelled in. The `ro` travels as it arrived -- the share is already read-only at the
/// device, and the guest is the end that can say so about a path before a write reaches one.
async fn configure(machine: &mut Machine, mounts: &[Mount]) -> anyhow::Result<Option<String>> {
    let replayed = InitCall {
        mounts: mounts
            .iter()
            .map(|mount| {
                let spec = MountSpec::new(format!("file://{}", mount.at), &mount.at)?;
                Ok(if mount.read_only {
                    spec.read_only()
                } else {
                    spec
                })
            })
            .collect::<Result<_, cortex::console::InvalidMount>>()?,
        // None of these reaches the guest: an image is what this end booted, a network is a
        // device that either is or is not attached by now, a snapshot is refused above, and
        // the machine's shape was spent on the boot -- the guest is running on the answer and
        // has nothing left to do with the question.
        image: None,
        network: None,
        snapshot: None,
        vcpus: None,
        memory_mib: None,
        gpu: None,
        gpu_memory_mib: None,
        disk_gib: None,
    };
    match machine.call(Call::Init(replayed)).await? {
        Response::Init(answer) => Ok(answer.cwd),
        Response::Error(e) => anyhow::bail!("the guest refused the session: {}", e.message),
        other => anyhow::bail!("the guest answered an init with {other:?}"),
    }
}

/// The machine that is up, booting one if a `stop` took the last.
async fn up<'a>(
    machine: &'a mut Option<Machine>,
    session: &Option<Session>,
) -> anyhow::Result<&'a mut Machine> {
    if machine.is_none() {
        let boot = session.as_ref().map(|told| &told.boot).ok_or_else(|| {
            coded(
                Error::INVALID_REQUEST,
                anyhow::anyhow!("this session has not been told what it is"),
            )
        })?;
        // The guest's `cwd` is dropped here and only here: what it is worth is the *first*
        // answer, which `init` already carried -- see [`InitResp::cwd`].
        let (started, _cwd) = boot().await.map_err(|e| coded(Error::BOOT_FAILED, e))?;
        *machine = Some(started);
    }
    Ok(machine.as_mut().expect("just booted"))
}

/// What the guest is asked to run before a machine is taken down -- see the guest's own note
/// on it. The disk is read on this side, and a machine is stopped by killing it.
pub(super) const SYNC: &str = "sync";

/// Take the machine down, having asked the guest to put what it wrote on the disk first.
///
/// **The two halves of stopping, and neither is optional.** A machine is stopped by killing
/// it, and a guest killed with writes still in its page cache never put them on the disk --
/// so the guest is asked to flush before it goes. And the disk was never unmounted, because
/// it is the root the guest pivoted onto, so its superblock is still saying "mounted" about
/// a filesystem nothing is mounting; [`Ext4::settled`] is this end saying otherwise, which
/// the flush is what makes true.
///
/// Both are what makes the disk of a stopped session readable -- by the boot that carries it
/// on, and by the [`snapshot`] that reads it out.
///
/// Called with no machine up as well, because a stop that failed part-way leaves one down
/// and unsettled, and the second call is what finishes it. Settling twice is settling once.
async fn quiesce(machine: &mut Option<Machine>, session: &Option<Session>) -> anyhow::Result<()> {
    let Some(told) = session else {
        // Nothing has been told what it is, so there is no disk and nothing was written to
        // it. Whatever is here goes down all the same.
        *machine = None;
        return Ok(());
    };

    if let Some(up) = machine.as_mut() {
        let flushed = up
            .call(Call::Exec(ExecCall {
                cmd: vec![guest_program(), SYNC.into()],
                timeout_ms: None,
            }))
            .await;

        // Down whatever the flush said, and before it is read: a machine that would not
        // answer this is not one to keep, and the disk is not to be read with it alive.
        *machine = None;

        match flushed? {
            Response::Exec(done) if done.code == 0 => {}
            Response::Exec(done) => {
                anyhow::bail!("flushing the session's disk exited {}", done.code)
            }
            Response::Error(e) => anyhow::bail!("flushing the session's disk: {}", e.message),
            other => anyhow::bail!("the guest answered a flush with {other:?}"),
        }
    }

    Ok(told.upper.settled()?)
}

/// What this session has written, as the layer a client hands back to `init`.
///
/// **The machine goes down to answer this, every time.** What a snapshot is here is the
/// disk, and what is on the disk is only the whole of what the session wrote once the guest
/// has flushed and gone -- see [`quiesce`]. The call after this one boots again over the
/// same file, which is what `stop` and `start` already do and costs a boot; a client that
/// sent `stop` first pays nothing, there being no machine to take down.
///
/// An empty layer is an answer and not a failure: a session that wrote nothing has written
/// nothing, and an archive saying so is what that is. A session that has not been told what
/// it *is* is refused, because then there is no disk to have written on.
///
/// Gzipped, which is the form a layer travels in everywhere else, and the form the
/// compression a client may have sent one under is read as.
async fn snapshot(
    machine: &mut Option<Machine>,
    session: &Option<Session>,
) -> anyhow::Result<Vec<u8>> {
    let told = session.as_ref().ok_or_else(|| {
        coded(
            Error::INVALID_REQUEST,
            anyhow::anyhow!(
                "this session has not been told what it is, so there is no disk it could \
                 have written on"
            ),
        )
    })?;

    quiesce(machine, session).await?;

    // Onto a file beside the disk and not into memory, as a build's is: what comes off the
    // upper is the session's whole work uncompressed, which is bounded by how large that
    // disk may grow and not by what one frame carries. Only the gzip is held, and that is
    // the thing about to be answered with anyway.
    let tar = told.upper.path.with_extension("snapshot.tar");
    let blob = layer(&told.upper, &told.skip, &tar);
    let _ = std::fs::remove_file(&tar);
    let blob = blob?;

    // A result travels in one frame, and half a layer is not a smaller session's work but a
    // filesystem that cannot be read -- so this is said rather than shortened. The reserve
    // is for the rest of the frame: the method, the id and the keys around the blob.
    const RESERVE: usize = 64 * 1024;
    anyhow::ensure!(
        blob.len() + RESERVE <= MAX_PAYLOAD,
        "this session has written {} bytes, which is more than the {MAX_PAYLOAD} one frame \
         carries -- and a layer cut in half is not a layer",
        blob.len()
    );

    Ok(blob)
}

/// Read `upper` out onto `at`, and answer with it gzipped.
///
/// Its own function so that the file it writes is removed however this leaves -- the caller
/// does that, and cannot if the failure is a `?` in the middle of it.
fn layer(upper: &Ext4, skip: &[String], at: &Path) -> anyhow::Result<Vec<u8>> {
    let skip: Vec<&str> = skip.iter().map(String::as_str).collect();

    let written = std::fs::File::create(at)?;
    upper.to_blob(&skip, &written)?;
    drop(written);

    let mut blob = Vec::new();
    cortex_krun::image::compress_blob(std::fs::File::open(at)?, &mut blob)?;
    Ok(blob)
}

/// Make sure `image` is in the cache, pulling it if it is not.
///
/// `provision` is the question and the answer at once: it stitches the layers into the disk a
/// boot attaches, and the one failure that means "nobody pulled this" is the one worth
/// reaching a registry over. Anything else it says is about this machine and is reported.
async fn pulled(image: &str) -> anyhow::Result<()> {
    use cortex_krun::image::{Error as ImageError, provision, pull};

    match provision(image, None) {
        Ok(_) => Ok(()),
        Err(ImageError::NotPulled(_)) => {
            pull(image, None, None)
                .await
                .map_err(|e| coded(Error::UNKNOWN_IMAGE, anyhow::Error::new(e)))?;
            provision(image, None)
                .map(|_| ())
                .map_err(|e| coded(Error::BOOT_FAILED, anyhow::Error::new(e)))
        }
        Err(e) => Err(coded(Error::BOOT_FAILED, anyhow::Error::new(e))),
    }
}

/// Where this server keeps its sessions: one directory apiece, named by the pid of the
/// process that owns it. See [`crate::home`] for what is under one.
fn sessions() -> PathBuf {
    crate::home().join("session")
}

/// Remove what an earlier run did not get to.
fn sweep() {
    let Ok(entries) = std::fs::read_dir(sessions()) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        // A pid comes round again, so one named for this process is a dead predecessor's --
        // and it is the one directory that must go, since this run is about to put a session
        // in it.
        //
        // Otherwise the question is whether anything is still running under that pid, and
        // the answer has to keep "gone" apart from "alive and not mine" -- a session under
        // another user is not ours to remove.
        let stale = pid == std::process::id() as i32 || !alive(pid);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Whether a process is still running under `pid`.
///
/// **"Gone" and not "not mine"**, which is the whole difficulty: a pid that answers is one
/// whose session directory has an owner, and a pid that is merely out of this user's reach
/// answers too. Getting that backwards would delete a live session's files.
///
/// `kill(pid, 0)` sends no signal and performs only the permission and existence check, so
/// `ESRCH` is the one answer that means gone and `EPERM` says alive under another user.
/// Windows has no such call: `OpenProcess` for the least it can be opened for either answers
/// a handle, or refuses with `ERROR_ACCESS_DENIED` for a process this user may not touch --
/// both of which are a process that is there. Only `ERROR_INVALID_PARAMETER`, which is what
/// it answers for a pid nothing is using, means gone.
///
/// Either way a pid can be reused, and then a dead owner looks alive and its directory is
/// left for a later run to meet again. That is the safe direction to be wrong in.
#[cfg(unix)]
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 performs only that check; it cannot affect this or any other process.
    unsafe { libc::kill(pid, 0) == 0 }
        || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(windows)]
fn alive(pid: i32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: a call that only asks for a handle. The handle is closed below and nothing is
    // done with the process itself.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32) };
    if !handle.is_null() {
        // SAFETY: `handle` was just opened and is not used again.
        unsafe { CloseHandle(handle) };
        return true;
    }
    io::Error::last_os_error().raw_os_error() != Some(ERROR_INVALID_PARAMETER as i32)
}

/// Every tree the session named, as a host directory against the guest path it appears at.
///
/// Two refusals, and the difference between them is what has to change. A scheme this build
/// has no provider for is [`UNSUPPORTED_MOUNT`](Error::UNSUPPORTED_MOUNT), because which
/// kinds of tree can be realized is a fact about the *build*. A URL or a mount point this
/// server cannot act on is [`INVALID_PARAMS`](Error::INVALID_PARAMS), because the request is
/// what has to change. Each names the entry it is about, since a session names several and a
/// client hearing only what was wrong cannot tell which.
///
/// In the order they were given, which the protocol makes meaningful: a tree at `/work/out`
/// that follows one at `/work` lands inside it, so nothing here sorts or merges the list.
fn mounts(init: &InitCall) -> anyhow::Result<Vec<Mount>> {
    let mut mounts: Vec<Mount> = Vec::with_capacity(init.mounts.len());
    for spec in &init.mounts {
        // A scheme and not a spelling: what this server can realize is a directory the
        // hypervisor shares, and no URL written another way would make one of those.
        let from = spec.file_path().ok_or_else(|| {
            coded(
                Error::UNSUPPORTED_MOUNT,
                anyhow::anyhow!(
                    "{spec}: a guest is handed a directory over virtio-fs, so this server \
                     realizes file:// and nothing else"
                ),
            )
        })?;
        // Where the scheme is one this server has, and the URL is not one it can act on. The
        // fix is the client's spelling rather than a different server, which is what parts it
        // from the refusal above.
        if !from.is_absolute() {
            return Err(coded(
                Error::INVALID_PARAMS,
                anyhow::anyhow!("{spec}: a file:// tree needs an absolute path"),
            ));
        }

        let at = spec.guest_path();
        // The root is the image's, stacked with the session's own disk under it, and a share
        // over it would leave a guest with neither.
        if at == Path::new("/") {
            return Err(coded(
                Error::INVALID_PARAMS,
                anyhow::anyhow!("{spec}: / is the session's own root and is not a mount point"),
            ));
        }
        // The guest binary's share lives there, and it is what the guest execs -- a tree over
        // it is a machine that comes up with nothing to run. `/abin` is the same answer for a
        // different reason: it is what this backend undertakes to put on `PATH`, and a tree
        // over it is that undertaking quietly not held.
        for reserved in [GUEST_DIR, ABIN_DIR] {
            if at.starts_with(reserved) {
                return Err(coded(
                    Error::INVALID_PARAMS,
                    anyhow::anyhow!("{spec}: {reserved} is this backend's own and is not free"),
                ));
            }
        }
        if mounts.iter().any(|mount| Path::new(&mount.at) == at) {
            return Err(coded(
                Error::INVALID_PARAMS,
                anyhow::anyhow!("{spec}: two trees cannot appear at the same path"),
            ));
        }

        mounts.push(Mount {
            at: at.to_string_lossy().into_owned(),
            from: from.to_path_buf(),
            read_only: spec.is_read_only(),
        });
    }
    Ok(mounts)
}

/// A failure that names the code the protocol has for it.
///
/// Everything under a request hands back an `anyhow::Error`, so a code travels the way a
/// message already does -- carried by the failure itself and read off it where the response is
/// made -- rather than by a second return type every step in between would have to thread
/// through. A failure that names none is [`INTERNAL_ERROR`](Error::INTERNAL_ERROR), which is
/// what "something went wrong at this end" means on the wire.
#[derive(Debug)]
struct Refusal {
    code: i64,
    said: anyhow::Error,
}

impl std::fmt::Display for Refusal {
    /// What was said, and not the code: the code is read off the value, and a sentence with
    /// the number in it would carry it twice wherever this is shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.said)
    }
}

impl std::error::Error for Refusal {}

/// `said`, named as `code` -- the failure kept whole and only labelled.
///
/// What a client reads is still the sentence written where the thing went wrong. A code says
/// which kind of failure this is, and nothing this end could add about it would be a better
/// sentence than the one the layer that hit it already wrote.
fn coded(code: i64, said: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(Refusal { code, said })
}

/// Anything that went wrong, as the one thing the protocol carries.
fn refused(e: &anyhow::Error) -> Error {
    // The whole chain and not the outermost failure, because a code is stated where the case
    // that has one is recognised, and context added on the way out here is what makes the
    // message readable rather than what decides which failure it is.
    let code = e
        .chain()
        .find_map(|cause| cause.downcast_ref::<Refusal>())
        .map_or(Error::INTERNAL_ERROR, |named| named.code);
    Error {
        code,
        message: format!("{e:#}"),
        data: None,
    }
}
