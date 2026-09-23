//! Answering a console session: the wire, and what is run at the far end of it.

mod halves;
mod machine;

use std::future::Future;
use std::pin::Pin;
use std::{io, path::PathBuf};

use cortex::console::stdio::StdioServer;
use cortex::console::{
    Call, Error, InitCall, InitResp, Message, Notification, Response, Server, TreeMount, TreeSource,
};
use cortex_krun::session::Network;
use cortex_krun_host::{ARTIFACTS_PATH, CONTEXT_PATH};

use machine::{HOST_PORTS_ENV, NETWORK_ENV};

use machine::{Machine, Mount};

/// A machine that has been booted and told what the session is, with the guest's own answer
/// to where it stands -- see [`InitResp::cwd`].
type Booted = Pin<Box<dyn Future<Output = anyhow::Result<(Machine, Option<String>)>>>>;

/// How to boot another. Held rather than the machine alone, because a `stop` hands one back
/// and the call after it takes a second -- on the same disk, which is what makes it the same
/// session.
type Factory = Box<dyn Fn() -> Booted>;

/// Answer a console session on stdin and stdout, until the client ends it.
pub async fn run() -> anyhow::Result<()> {
    // Taken for the life of the process: from here on stdout carries frames and nothing else,
    // which is why every diagnostic goes to stderr. Nothing enforces that -- it is a rule,
    // and this is where it starts applying.
    let mut server = StdioServer::stdio()?;

    sweep();

    // What to boot, once a client has said what.
    let mut factory: Option<Factory> = None;
    // Whatever it holds is dropped on `stop` and on the way out of this function whichever
    // way it leaves, which takes the machine down every time.
    let mut machine: Option<Machine> = None;

    while let Some(message) = server.recv().await? {
        match message {
            Message::Request {
                id,
                call: Call::Init(_),
            } if factory.is_some() => {
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
                    Ok((boot, started, answer)) => {
                        factory = Some(boot);
                        machine = Some(started);
                        Response::Init(answer)
                    }
                    Err(e) => Response::Error(refused(&e)),
                };
                server.respond(id, answered).await?;
            }

            // Everything else is the guest's to answer.
            Message::Request { id, call } => {
                let (answered, lost) = match up(&mut machine, &factory).await {
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
                if let Err(e) = up(&mut machine, &factory).await {
                    eprintln!("{}: booting: {e:#}", env!("CARGO_BIN_NAME"));
                }
            }

            Message::Notification(Notification::Stop) => machine = None,

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
async fn start(init: InitCall) -> anyhow::Result<(Factory, Machine, InitResp)> {
    // How far this session may reach, and which doors on this machine it is granted.
    // Settled before anything is built, because it is the one part of an `init` a client can
    // be told it got wrong while the answer still costs it nothing: a reach decides whether a
    // virtio-net device is attached, and a device is attached before a kernel comes up.
    let (network, host_ports) = reach(&init)?;

    // **A session here is its disk**, which is a file this server keeps rather than bytes a
    // frame carries -- see the guest's answer to `snapshot`. A client handing one back is
    // describing a session this backend has no way to start from.
    if init.snapshot.is_some() {
        return Err(coded(
            Error::INVALID_PARAMS,
            anyhow::anyhow!(
                "this server keeps a session in its own disk image, so it has no way to \
                 start from a layer somebody else's session wrote"
            ),
        ));
    }

    let rootfs = init.rootfs.clone().ok_or_else(|| {
        coded(
            Error::INVALID_PARAMS,
            anyhow::anyhow!(
                "a session here runs in a micro-VM, which boots on a rootfs, and this one \
                 named none"
            ),
        )
    })?;
    if !rootfs.steps.is_empty() {
        return Err(coded(
            Error::UNSUPPORTED_IMAGE,
            anyhow::anyhow!(
                "this server boots the base it is given and builds nothing over it: {} \
                 step(s) were declared",
                rootfs.steps.len()
            ),
        ));
    }
    anyhow::ensure!(!rootfs.base.is_empty(), "the rootfs named no base");

    // Into the cache, so that the boot -- which asks no registry anything -- finds it. Done
    // here and not there because this is the end with a client to tell, and a name nobody can
    // pull is the one failure at `init` that is about what was asked for rather than about
    // this machine.
    pulled(&rootfs.base).await?;

    let context = at(init.context.as_ref(), "context", Error::UNSUPPORTED_CONTEXT)?;
    let artifacts = at(
        init.artifacts.as_ref(),
        "artifacts",
        Error::UNSUPPORTED_ARTIFACTS,
    )?;

    // Where each tree is on this side, against the one path in the guest that names it. Fixed
    // for the session, which is why the answer below can say them before anything is mounted.
    let mounts: Vec<Mount> = [
        (CONTEXT_PATH, context.as_ref()),
        (ARTIFACTS_PATH, artifacts.as_ref()),
    ]
    .into_iter()
    .filter_map(|(at, from)| {
        from.map(|from| Mount {
            at: at.to_string(),
            from: from.clone(),
        })
    })
    .collect();

    // Cloned per boot rather than borrowed, because what this closure is for is outliving the
    // machine it makes.
    let image = rootfs.base.clone();
    let told = init.clone();
    let boot: Factory = Box::new(move || {
        let (image, mounts, told, host_ports) = (
            image.clone(),
            mounts.clone(),
            told.clone(),
            host_ports.clone(),
        );
        Box::pin(async move {
            let mut machine = Machine::boot(&image, &mounts, network, &host_ports).await?;
            let cwd = configure(&mut machine, &told, &mounts).await?;
            Ok((machine, cwd))
        })
    });

    // Where the session stands is the image's business and only the guest has read it; where
    // the trees went is this end's, because it is what attached them.
    let (started, cwd) = boot().await.map_err(|e| coded(Error::BOOT_FAILED, e))?;
    Ok((
        boot,
        started,
        InitResp {
            context: context.as_ref().map(|_| TreeMount {
                path: CONTEXT_PATH.to_string(),
            }),
            artifacts: artifacts.as_ref().map(|_| TreeMount {
                path: ARTIFACTS_PATH.to_string(),
            }),
            cwd,
        },
    ))
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

/// Tell the guest what the session is, in the one spelling both ends use.
///
/// The client's `init` named its trees by host URL; the guest's copy names them by the guest
/// mount point, because that is where they are from in there and what every later call is
/// spelled in. Everything else about the call is the client's and travels as it arrived.
async fn configure(
    machine: &mut Machine,
    told: &InitCall,
    mounts: &[Mount],
) -> anyhow::Result<Option<String>> {
    let placed = |at: &str| {
        mounts
            .iter()
            .find(|mount| mount.at == at)
            .map(|_| TreeSource::new(format!("file://{at}")))
    };
    let replayed = InitCall {
        context: placed(CONTEXT_PATH),
        artifacts: placed(ARTIFACTS_PATH),
        // Neither reaches the guest: a rootfs is what this end booted, and a network is a
        // device that either is or is not attached by now.
        rootfs: None,
        network: None,
        snapshot: None,
        ..told.clone()
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
    factory: &Option<Factory>,
) -> anyhow::Result<&'a mut Machine> {
    if machine.is_none() {
        let boot = factory.as_ref().ok_or_else(|| {
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
        // Otherwise `kill(pid, 0)` sends no signal and only reports whether the pid could be
        // signalled: `ESRCH` -- no such process -- is the one answer that means the directory
        // is abandoned, where `EPERM` says the pid is alive under another user and its files
        // are not ours to remove.
        //
        // SAFETY: signal 0 performs only that permission and existence check; it cannot
        // affect this or any other process.
        let stale = pid == std::process::id() as i32
            || (unsafe { libc::kill(pid, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH));
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// A host directory a tree names, and `None` for one the session did not name.
///
/// `unsupported` is the code for the tree in `role`, which the protocol gives one of each: a
/// client refused at `init` has to know *which* of the two this server cannot take, and a
/// session names up to two.
fn at(named: Option<&TreeSource>, role: &str, unsupported: i64) -> anyhow::Result<Option<PathBuf>> {
    let Some(named) = named else {
        return Ok(None);
    };
    // A scheme and not a spelling: which kinds of tree can be realized is a fact about this
    // build, and no path written any other way would make this one of them.
    let path = named.file_path().ok_or_else(|| {
        coded(
            unsupported,
            anyhow::anyhow!(
                "{role}: a guest is handed a directory, so this server realizes file:// and \
                 nothing else"
            ),
        )
    })?;
    // Where the scheme is one this server has, and the URL is not one it can act on. The fix
    // is the client's spelling rather than a different server, which is what parts it from
    // the refusal above.
    if !path.is_absolute() {
        return Err(coded(
            Error::INVALID_PARAMS,
            anyhow::anyhow!(
                "{role}: a file:// tree needs an absolute path, and {} is not",
                path.display()
            ),
        ));
    }
    Ok(Some(path.to_path_buf()))
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
