//! Answering a console session by running commands in this guest.
//!
//! What arrives here has come a long way -- a client's `exec`, framed on the server, handed
//! to a virtio-console port, read out of a character device in here -- and none of that is
//! visible below. A [`Server`] over the port brings requests in and puts responses out, and
//! what is left is what is actually this end's: running a command, keeping track of where the
//! session stands, and reading and writing the files a command works with.
//!
//! Which is the same list a host-local console server has, answered the same way. That is
//! worth stating rather than hiding: the two backends differ in *where* a command runs and in
//! nothing else, and the code below being recognisable is the shape of that fact rather than
//! an accident.
//!
//! # Running the command
//!
//! [`Command`], captured: spawned, both pipes drained, and one response when it ends. An
//! execution is one request and one answer, so there is nothing to interleave and nothing to
//! hold -- which is what makes [`execute`] a function of the request rather than something
//! threaded through the channel.
//!
//! # Where the session stands
//!
//! It starts where libkrun's init stood this process, which is where the base image said --
//! and moves only through [`cd`](Session::change_dir), which this process answers itself: `cd`
//! is a shell builtin rather than a program, so an `exec` carrying an argv has no other way
//! to offer it, and the shell a person would be talking to is the session. A `cd` inside a
//! command (`sh -c 'cd x'`) moves that command's own process and nothing else, exactly as a
//! subshell does in a terminal.
//!
//! Nothing is reported about it either way. A `cd` is answered with a code and no output, the
//! way a terminal answers one, and a client that wants to know where it is runs `pwd` --
//! which lands here as an ordinary command, in the directory this session is holding.
//!
//! # Files
//!
//! `read` and `write` reach this guest's filesystem directly, at the path as given -- the
//! same namespace a command here resolves its own relative paths in, since a command here is
//! an ordinary process of this one's, started in the same working directory.
//!
//! Which is the whole reason the protocol makes every call boot first. On a host-local
//! backend that rule buys nothing, because the filesystem is there either way; here there is
//! no file to name until a guest exists to hold it.
//!
//! # What this does not do
//!
//! A `read` or a `write` is not confined to anywhere. The path is used as it arrives, so a
//! client can name any file in the guest -- which is a smaller claim than it sounds, since
//! the guest is one session's overlay and the client is the one who asked for it. The one
//! path a `write` is refused at is one inside a tree the client itself asked to be
//! read-only, and the share behind it is read-only at the device besides.
//!
//! `exec.timeout_ms` is enforced by a kill; `init` carries no default. An `exec` that names
//! no timeout is therefore unbounded -- a command that never ends is one this process waits on
//! forever, and the client waits with it.
//!
//! What a timed-out command started is killed with it, and this process is the guest's PID 1:
//! whatever those processes were parented to, their corpses come here, and nothing here reaps
//! a child it did not spawn -- a general `waitpid(-1)` would take statuses tokio is waiting
//! on. A guest that times out many commands is a guest with that many zombie entries, which a
//! session's lifetime bounds.

use std::ffi::OsString;
use std::io::{self, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Output, Stdio};
use std::time::Duration;

use cortex::protocol::stdio::StdioServer;
use cortex::protocol::{
    Call, Error, ExecCall, ExecResp, InitCall, InitResp, MAX_PAYLOAD, Message, MountSpec,
    Notification, ReadCall, ReadResp, Response, Server, VersionResp, WriteCall, WriteResp,
};
use cortex_krun_host::{ABIN_DIR, HANDSHAKE};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::process::{Child, Command};

/// A command we found but could not start, and one we could not find at all.
///
/// Both are codes an ordinary command can reach by its own `exit()`, which is why neither
/// goes back *as* a code: they travel as an [`Error`], which cannot be mistaken for something
/// a command chose.
const NOT_EXECUTABLE: i32 = 126;
const NOT_FOUND: i32 = 127;

/// How much of a file one response may carry.
///
/// A frame is capped at [`MAX_PAYLOAD`], and a `read`'s answer is its `data` plus the members
/// around it -- `jsonrpc`, `id`, `result`, and the `size` beside the bytes. A kibibyte covers
/// those several times over.
const MAX_DATA: u64 = MAX_PAYLOAD as u64 - 1024;

/// How much of one of a command's two streams a response may carry.
///
/// Half of [`MAX_DATA`] apiece, because an [`ExecResp`] carries both and either one can be
/// the large one. What is over it is dropped and [`ExecResp::truncated`] says so -- see
/// [`finished`].
const MAX_STREAM: usize = MAX_DATA as usize / 2;

/// Answer requests on `port` until the client says `quit` or the channel closes.
///
/// `port` is the virtio-console port, open read-write. It becomes two halves here, from one
/// `dup`: the driver refuses a second opener, and the framing wants to read and write through
/// borrows that do not have to take turns.
pub async fn serve(port: std::fs::File) -> anyhow::Result<()> {
    // SAFETY: `dup` hands back a fresh descriptor for the same open file, and the `File`
    // built from it is the only owner of that number.
    let duplicate = unsafe { libc::dup(port.as_raw_fd()) };
    if duplicate < 0 {
        anyhow::bail!(
            "duplicating the console port: {}",
            io::Error::last_os_error()
        );
    }
    let mut outgoing = unsafe { std::fs::File::from_raw_fd(duplicate) };

    // Before anything else on the wire, and before the framing owns the descriptor: the host
    // discards what it writes to a port nothing in here has opened, so this is what says a
    // reader exists. Eight bytes onto a queue nothing has filled yet, so the write being the
    // blocking one is a distinction without a wait.
    greet(&mut outgoing)?;

    // Blocking descriptors driven through tokio's blocking pool. A virtio port is pollable
    // and an `AsyncFd` would be the tighter fit, but this channel is read at exactly one
    // place and written at exactly one other -- a thread parked on the next request is what
    // waiting for the next request looks like either way.
    let mut server = StdioServer::new(
        tokio::fs::File::from_std(port),
        tokio::fs::File::from_std(outgoing),
    );

    let mut session = Session::new();

    while let Some(message) = server.recv().await? {
        match message {
            // The session is over. The host stops the guest right after this, so there is
            // nothing to release that the VM going away does not release.
            Message::Notification(Notification::Quit) => return Ok(()),

            // Booting early rather than under whichever call would have paid for it. Nothing
            // answers this, so a failure is only said here -- the next call that needs a boot
            // tries again and tells whoever asked for it.
            Message::Notification(Notification::Start) => {
                if let Err(e) = session.boot() {
                    eprintln!(
                        "{}: booting a session: {}",
                        env!("CARGO_BIN_NAME"),
                        e.message
                    );
                }
            }

            Message::Notification(Notification::Stop) => session.release(),

            Message::Request { id, call } => match call {
                // A session is taken or it is not: a tree this process cannot stand in is
                // refused rather than answered with a path, since a path is what every later
                // call is spelled in. See [`Session::configure`].
                Call::Init(init) => {
                    let answered = match session.configure(init) {
                        Ok(answer) => Response::Init(answer),
                        Err(e) => Response::Error(e),
                    };
                    server.respond(id, answered).await?;
                }

                Call::Exec(exec) => match session.boot() {
                    Err(e) => server.respond(id, Response::Error(e)).await?,
                    // `cd` is the session's own and not a program: see
                    // [`Session::change_dir`]. It is answered here rather than spawned, and
                    // it is the one thing that moves where the session stands.
                    Ok(()) => match cd_target(&exec) {
                        Some(argv) => {
                            let answered = session.change_dir(argv);
                            server.respond(id, answered).await?;
                        }
                        None => {
                            let answered = execute(&exec, &session).await;
                            server.respond(id, answered).await?
                        }
                    },
                },

                Call::Read(read) => {
                    let answered = match session.boot() {
                        Ok(()) => read_file(session.cwd(), &read).await,
                        Err(e) => Response::Error(e),
                    };
                    server.respond(id, answered).await?;
                }

                Call::Write(write) => {
                    let answered = match session.boot() {
                        Ok(()) => write_file(&session, &write).await,
                        Err(e) => Response::Error(e),
                    };
                    server.respond(id, answered).await?;
                }

                // **The one call that is not this end's to answer, and never reaches it.**
                //
                // A snapshot is the upperdir as a tar -- the files the session wrote and the
                // whiteouts for what it deleted. On a backend that stacks the overlay itself
                // that is a directory this process could walk. Here the stacking is libkrun's
                // init, one root up: the upperdir is on the ext4 the server attached, and once
                // the pivot is done it is under no path this process can reach.
                //
                // So the server answers it, off the disk, with the machine stopped -- which is
                // the only state that file is the whole of what the session wrote. It never
                // forwards one, and this is what anything talking to this process directly is
                // told.
                Call::Snapshot(_) => {
                    let answered = Response::Error(refused(
                        Error::METHOD_NOT_FOUND,
                        "this backend keeps a session on a disk the host attached, which is \
                         one root up from this process and not something it can read -- the \
                         server answers a snapshot off that disk instead",
                    ));
                    server.respond(id, answered).await?;
                }

                Call::Version(_) => {
                    let answered = Response::Version(VersionResp {
                        version: "1".into(),
                    });
                    server.respond(id, answered).await?;
                }

                Call::BuildImage(_) | Call::RemoveImage(_) | Call::ListImages(_) => {
                    let answered = Response::Error(refused(
                        Error::METHOD_NOT_FOUND,
                        "images are the host's cache and not this machine's -- the server \
                         answers these without forwarding them",
                    ));
                    server.respond(id, answered).await?;
                }
            },

            // A response answers a request, and this end makes none.
            Message::Response { id, .. } => eprintln!(
                "{}: a response arrived for request {id}, which nobody made",
                env!("CARGO_BIN_NAME")
            ),
        }
    }
    Ok(())
}

/// Put [`HANDSHAKE`] on the channel, through the same descriptor the frames will use.
///
/// Flushed on the way out, because what comes next is this end waiting for a request the host
/// will not send until these bytes arrive.
fn greet(outgoing: &mut std::fs::File) -> io::Result<()> {
    use std::io::Write as _;
    outgoing.write_all(HANDSHAKE)?;
    outgoing.flush()
}

/// What a session is in here: the trees it was given, where it stands, and whether it has
/// booted.
///
/// **The same shape a host-local backend has**, deliberately: a client cannot tell which one
/// answered it, so what a session *is* must not depend on which one did. What differs between
/// them is only where a command runs, which is the whole of what a backend is for.
struct Session {
    /// Every tree the session was given, as `init` named it -- and as it is mounted in here.
    ///
    /// The `init` this end hears is the console server's replay, and it names each tree by
    /// the guest's own mount point rather than by the host directory behind it. So what the
    /// client asked for, what that replay says, and what this end opens are all the one
    /// path.
    ///
    /// What a tree is *for* is the client's and is not here: these differ in where they
    /// appear and in whether a command may write in them, which is the whole of what this
    /// end has to know about one.
    mounts: Vec<Tree>,

    /// Where the session stands, which is what an execution runs in and what a relative path
    /// in a file call resolves against.
    ///
    /// Never absent: a process stands somewhere whatever it was told, and [`Session::home`]
    /// is where this one starts. An `Option` here would have put a state that cannot happen
    /// into every call that resolves a path.
    cwd: PathBuf,

    /// Where a session on this root starts, and what `cd` with no argument goes back to.
    ///
    /// **Where libkrun's init stood this process**, which is where the base image said --
    /// the init creates the directory the image named, the way `WORKDIR` does, and falls back
    /// to `/` for an image that named none or one it could not make. So the server's answer
    /// to `init` and this end's agree without either asking the other.
    home: PathBuf,

    /// Whether the trees `init` named have been found where the boot put them.
    ///
    /// A flag and not a resource, because the mounts are the *boot's* -- libkrun's init
    /// mounted the shares before this process started, from what the VM was told -- so what
    /// happens here is the check and not the mounting. What the flag earns is that the check
    /// happens once per boot rather than once per request, and that `stop` means the same
    /// thing here as on a backend where it hands something back.
    booted: bool,
}

impl Session {
    /// A session standing where this process was started, with no tree configured yet.
    fn new() -> Session {
        // The init stood us in the image's working directory. A guest whose kernel would not
        // even tell us where we are is one where `/` is the only answer left, and it is the
        // right one: it is the fallback the init itself uses.
        let home = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        Session {
            cwd: home.clone(),
            home,
            mounts: Vec::new(),
            booted: false,
        }
    }

    /// Take a new shape, answering what the client has to know about it.
    ///
    /// Every URL is read before anything is let go of, so a session this process cannot take is
    /// one it has not taken. Otherwise the old boot goes: the trees are what a boot checked
    /// for, and a boot from before this is a boot that no longer matches.
    fn configure(&mut self, config: InitCall) -> Result<InitResp, Error> {
        let mounts = config
            .mounts
            .iter()
            .map(tree)
            .collect::<Result<Vec<Tree>, Error>>()?;

        self.release();
        self.mounts = mounts;
        // Back to where the image said, because an `init` is the session starting over and a
        // `cd` from the session before it is not something the new one asked for. The root is
        // the same root, so this is the same answer the init already stood in.
        self.cwd = self.home.clone();

        Ok(InitResp {
            cwd: self.named_cwd(),
        })
    }

    /// Where the session stands.
    fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The read-only tree `path` lands in, if it lands in one.
    ///
    /// **A write is refused where the path says it should be**, rather than left to the
    /// share to answer. The device is read-only either way -- the host's virtiofs server
    /// refuses the write and the kernel's `EROFS` would come back as the same code -- so
    /// what this earns is the sentence: a client is told which of its own trees it named,
    /// which is the thing it can act on.
    ///
    /// `starts_with` is by component, so a tree at `/work` does not claim `/workshop`. What
    /// is passed is the path already landed where the session stands and normalized on
    /// paper, since a mount point is spelled absolutely and a relative path is not one until
    /// it is.
    fn read_only_tree(&self, path: &Path) -> Option<&Path> {
        self.mounts
            .iter()
            .find(|tree| tree.read_only && path.starts_with(&tree.at))
            .map(|tree| tree.at.as_path())
    }

    /// The same, as the protocol can carry it -- `None` for a directory with no string form,
    /// which is a path the client could not have been told about anyway.
    fn named_cwd(&self) -> Option<String> {
        self.cwd().to_str().map(str::to_owned)
    }

    /// Forget that the trees were checked for.
    ///
    /// Which is the whole of what a `stop` gives back in here -- the shares are the boot's
    /// and stay mounted -- so this is not the release it is on the server side, where a whole
    /// guest goes away. It is still not a no-op: the next call that needs a session checks
    /// again, which is what a client that sent `stop` is asking for.
    fn release(&mut self) {
        self.booted = false;
    }

    /// Bring the session up if nothing has: the trees where `init` said they would be.
    ///
    /// They were mounted before this process started -- libkrun's init did it, from what the
    /// boot named -- so realizing them here is checking that they are there, which is what
    /// [`MOUNT_FAILED`](Error::MOUNT_FAILED) reports when they are not. A guest whose share
    /// failed to mount is a session described correctly in an environment that is wrong for
    /// it, which is exactly that code's case.
    fn boot(&mut self) -> Result<(), Error> {
        if self.booted {
            return Ok(());
        }

        // Every tree the session named, because the boot shared every one of them and a
        // client will send paths into any. The message names the one it is about: one code,
        // as many places as the session has trees.
        for tree in &self.mounts {
            let at = tree.at.as_path();
            match std::fs::metadata(at) {
                Ok(meta) if meta.is_dir() => {}
                Ok(_) => {
                    return Err(refused(
                        Error::MOUNT_FAILED,
                        format!("{}: not a directory in the guest", at.display()),
                    ));
                }
                Err(e) => {
                    return Err(refused(
                        Error::MOUNT_FAILED,
                        format!("{}: not mounted in the guest: {e}", at.display()),
                    ));
                }
            }
        }

        self.booted = true;
        Ok(())
    }

    /// Move where the session stands.
    ///
    /// **`cd` is the session's own because it is nobody else's.** It is a shell builtin and
    /// not a program, so an `exec` carrying an argv has no other way to offer it -- and the
    /// shell a person would be talking to is, here, the session itself.
    ///
    /// The answer is a result and not an error, including when it fails: `cd` behaves like
    /// the builtin it stands in for.
    ///
    /// **Logical, like a shell's own.** The path is normalized on paper and symlinks are left
    /// alone, so what goes back is spelled the way `init` answered.
    fn change_dir(&mut self, argv: &[String]) -> Response {
        let target = match argv {
            [] => self.home.clone(),
            [dir] => at(self.cwd(), dir),
            _ => return builtin_failed("cd: too many arguments"),
        };

        let moved = lexical(&target);
        match std::fs::metadata(&moved) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return builtin_failed(format!("cd: {}: not a directory", moved.display())),
            Err(e) => return builtin_failed(format!("cd: {}: {e}", moved.display())),
        }
        if moved.to_str().is_none() {
            return builtin_failed(format!(
                "cd: {}: has no name this protocol can carry",
                moved.display()
            ));
        }

        self.cwd = moved;
        // Nothing on the result says where that was. A shell answers `cd` with nothing
        // either, and a client that wants to know runs `pwd`.
        result(ExecResp {
            code: 0,
            ..ExecResp::default()
        })
    }
}

/// The argv of an `exec` that is a `cd`, without the name -- or `None` for anything else.
fn cd_target(exec: &ExecCall) -> Option<&[String]> {
    match exec.split() {
        Some((program, args)) if program == "cd" => Some(args),
        _ => None,
    }
}

/// One tree of the session's: where it is mounted in this guest, and whether a command may
/// write in it.
struct Tree {
    at: PathBuf,
    read_only: bool,
}

/// The tree one of a session's mounts names, or why it names none this process can use.
///
/// `file://` and nothing else. Inside a guest that is not a limitation the way it is on the
/// host: whatever a tree is made of was realized before the VM started, and what reaches here
/// is the directory it was mounted at -- which is why the server's replay spells every one of
/// them that way.
fn tree(named: &MountSpec) -> Result<Tree, Error> {
    let Some(path) = named.file_path() else {
        return Err(refused(
            Error::UNSUPPORTED_MOUNT,
            format!(
                "{named}: a guest is handed a mounted directory, so file:// is the only kind \
                 it can be told about",
            ),
        ));
    };
    if !path.is_absolute() {
        return Err(refused(
            Error::INVALID_PARAMS,
            format!("{named}: a file:// tree needs an absolute path"),
        ));
    }
    Ok(Tree {
        at: path.to_path_buf(),
        read_only: named.is_read_only(),
    })
}

/// `path` with `.` dropped and `..` popped, on paper and without touching the filesystem.
///
/// What a shell does to keep a working directory readable, and here it also keeps it
/// comparable: every path this end reports has to be spelled under the one `init` answered
/// with, and resolving symlinks would produce a second spelling the client cannot relate to
/// the first.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            component => out.push(component),
        }
    }
    out
}

/// The file a path names, resolved where the session stands.
///
/// An absolute path is already the answer and `Path::join` says so by dropping the root it
/// was given; a relative one lands where a command would have looked for it. **A join, not
/// containment** -- what it buys is that the two halves of the protocol name the same file.
fn at(root: &Path, path: impl AsRef<Path>) -> PathBuf {
    root.join(path)
}

/// A builtin that ran and failed, the way the shell builtin it stands in for would: a code
/// and a line, and not a refusal of the call.
fn builtin_failed(message: impl Into<String>) -> Response {
    let mut stderr: Vec<u8> = message.into().into_bytes();
    stderr.push(b'\n');
    result(ExecResp {
        code: 1,
        stderr,
        ..ExecResp::default()
    })
}

/// Run one command, and answer with everything it produced.
///
/// One request, one answer: the command is spawned, both of its pipes are drained, and what
/// comes back is how it ended.
///
/// `timeout_ms` is a kill, as the protocol promises: the command runs in a process group of
/// its own so that what a shell spawned goes with it, and expiry answers
/// [`TIMED_OUT`](Error::TIMED_OUT) with nothing of the partial output -- a killed command has
/// no result to report. No timeout is no limit, and the wait is the whole command's.
///
/// # Where it runs
///
/// Where the session stands, set per command rather than inherited. A session moves -- `cd`
/// is answered by [`Session::change_dir`] and the next command runs where it left off -- so
/// this process's own working directory would be a second answer to that question, right only
/// until the first `cd`. `read_file` and `write_file` resolve against the same value, which
/// is what makes a command and a file call name the same file.
async fn execute(exec: &ExecCall, session: &Session) -> Response {
    let Some((program, args)) = exec.split() else {
        return Response::Error(refused(Error::INVALID_PARAMS, "an empty command"));
    };

    let mut cmd = Command::new(program);
    cmd.args(args)
        // Piped and then read by `run_to_end`, which is what carries the output back. Input
        // is at EOF from the start, since an `exec` carries none.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Dropping `child` below kills and reaps the direct child, which is how a command
        // that timed out is cleaned up after the group kill has reached what it spawned --
        // and how one that ends this function early, by any other return, is not left
        // running.
        .kill_on_drop(true)
        // Its own process group, so a `killpg` here reaches everything the command started --
        // `sh -c 'a & b'` leaves two children of its own -- and so that the group is this
        // command's alone: the `killpg` below never reaches this process, which as the guest's
        // PID 1 is not a process a stray signal may end.
        .process_group(0)
        .current_dir(session.cwd());
    for (name, value) in environment(session) {
        cmd.env(name, value);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let code = match e.kind() {
                io::ErrorKind::NotFound => NOT_FOUND,
                _ => NOT_EXECUTABLE,
            };
            return Response::Error(refused(
                Error::NOT_EXECUTABLE,
                format!("{program}: {e} (a shell would report {code})"),
            ));
        }
    };
    // Taken now: a reaped child has no id, and the kill below wants the one it was born with
    // -- which is also its group's, by `process_group(0)`.
    let pid = child.id();

    let waited = match exec.timeout_ms {
        None => run_to_end(&mut child).await,
        Some(ms) => {
            // `run_to_end` borrows the child rather than taking it, so expiry leaves `child`
            // -- alive, unkilled and unreaped -- in this frame for the kill below.
            match tokio::time::timeout(Duration::from_millis(ms), run_to_end(&mut child)).await {
                Ok(output) => output,
                Err(_elapsed) => {
                    // Everything the command started is in the group the spawn put it in, so
                    // one signal to the group ends all of it, the direct child included.
                    // `kill_on_drop` is not what does the killing here -- dropping `child` on
                    // the way out is what reaps the leader once the signal has landed.
                    if let Some(pid) = pid {
                        // SAFETY: a plain libc call, and `pid` names this command's process
                        // group and no other. It is the group id this process created with
                        // `process_group(0)`, and it is still that group's: `timeout` only
                        // expires while `run_to_end` is pending, and while it is pending the
                        // direct child -- the group's leader -- has not been reaped (see
                        // `run_to_end`). An unreaped process keeps its pid, running or
                        // zombie, so the kernel cannot have handed this id to anything else.
                        unsafe {
                            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
                        }
                    }
                    return Response::Error(refused(
                        Error::TIMED_OUT,
                        format!("killed after {ms}ms"),
                    ));
                }
            }
        }
    };

    finished(waited)
}

/// Drain both pipes to EOF, then reap the child -- in that order, and the order is the point.
///
/// This is what `std`'s `wait_with_output` does and tokio's does not: tokio's waits and reads
/// concurrently, reaping the direct child the moment it exits while whatever it spawned may
/// still hold the pipes open. Reaping last keeps the direct child unreaped -- a zombie, once
/// it has exited -- for as long as anything is still writing, and an unreaped process keeps
/// its pid. Since that pid is also the id of the process group the command was spawned into,
/// the group id stays this command's until this future completes, which is what lets
/// [`execute`] aim a `killpg` at it after a timeout: the timeout can only fire while this is
/// pending, and while this is pending the leader is still there.
///
/// Both pipes are read at once because a command that fills a pipe nobody is reading stops
/// there, and read to EOF rather than to some size because what to keep of the result is
/// [`finished`]'s decision, not this function's.
async fn run_to_end(child: &mut Child) -> io::Result<Output> {
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let (stdout, stderr) = tokio::try_join!(read_to_end(&mut stdout), read_to_end(&mut stderr))?;
    let status = child.wait().await?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Everything a pipe has to give, or nothing from a pipe that was never opened.
async fn read_to_end<R: AsyncRead + Unpin>(pipe: &mut Option<R>) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    if let Some(pipe) = pipe {
        pipe.read_to_end(&mut buf).await?;
    }
    Ok(buf)
}

/// The environment variables an execution is given: this process's own, less the plumbing,
/// plus the one thing that has to agree with where the command is standing.
///
/// **This process's own is the image's.** libkrun's init execs this process with what the boot
/// named, and what the boot named is what the image stated -- its `PATH`, its `LANG`,
/// whatever a Debian-based Python image puts there -- with the session's own additions over
/// the top. So there is nothing to replay: a command that inherits this inherits the image.
///
/// What is dropped is the two families that are not the image's. `KRUN_` is how the init was
/// told what root to build, and a command that read one would be reading a fact about a VM it
/// is not supposed to know it is in.
///
/// `PWD` is what a shell reads to answer `pwd`, and a command spawned in the session's
/// directory would otherwise be told it was standing wherever this process is. Set to match,
/// which is what a shell does for itself when it moves.
///
/// What is added is [`ABIN_DIR`], **prepended** -- see below.
///
/// Per command rather than per process, for the same reason a host-local console does it that
/// way: `std::env::set_var` is unsound with any other thread running, and this program has
/// one.
fn environment(session: &Session) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(name, _)| !name.as_encoded_bytes().starts_with(b"KRUN_"))
        .filter(|(name, _)| name != "PWD")
        .collect();
    with_abin(&mut env);
    env.push(("PWD".into(), session.cwd().into()));
    env
}

/// Put [`ABIN_DIR`] in front of whatever `PATH` the image stated.
///
/// **Prepended, not appended.** `/abin` is what cortex undertakes to provide, and an image
/// that happens to carry a program of the same name would make that undertaking untrue -- a
/// session would get the image's `index` rather than the one it was promised, and nothing
/// would say which it got.
///
/// Guarded on the directory being there, because a session whose host had no executables to
/// give it gets none: the boot leaves `--abin` off and there is nothing at that path. The
/// mount is the init's, so by the time this runs it has either happened or was never asked
/// for.
///
/// A `PATH` the image did not state is not invented here. This process's environment is the
/// one libkrun's init execed it with, which is the image's with the session's over the top,
/// and the default for an image that states none was already put there by the host -- so an
/// empty one at this point would be a session that was told to have none.
fn with_abin(env: &mut [(OsString, OsString)]) {
    if !std::path::Path::new(ABIN_DIR).is_dir() {
        return;
    }
    let Some((_, path)) = env.iter_mut().find(|(name, _)| name == "PATH") else {
        return;
    };
    let mut first = OsString::from(ABIN_DIR);
    first.push(":");
    first.push(&*path);
    *path = first;
}

/// A command that ended, as the answer to whatever asked for it.
fn finished(output: io::Result<Output>) -> Response {
    let out = match output {
        Ok(out) => out,
        Err(e) => {
            return Response::Error(refused(
                Error::INTERNAL_ERROR,
                format!("waiting for a command: {e}"),
            ));
        }
    };

    // A response travels in one frame under `MAX_PAYLOAD`, so a command that writes without
    // limit has to be cut off somewhere: an uncut one makes a frame this protocol refuses to
    // send, which is the whole session lost rather than one answer shortened. Both streams,
    // halved, because either of them alone can be the large one.
    let mut stdout = out.stdout;
    let mut stderr = out.stderr;
    let truncated = stdout.len() > MAX_STREAM || stderr.len() > MAX_STREAM;
    stdout.truncate(MAX_STREAM);
    stderr.truncate(MAX_STREAM);

    result(ExecResp {
        code: exit_code(&out.status),
        stdout,
        stderr,
        // Saying so is the point: an agent reading output it does not know is partial draws a
        // conclusion from it, and a wrong answer is worse than a short one.
        truncated,
    })
}

/// Hand back part of a file, as the answer to the `read` that asked for it.
///
/// The size is taken before the bytes are, so a file that grows between the two is reported
/// as the shorter one it was -- and one that shrinks is answered with however much was still
/// there. Either way `data` is what was read and `size` is what the file measured, which is
/// the pair a requester needs to know whether to ask again.
async fn read_file(root: &Path, read: &ReadCall) -> Response {
    let path = at(root, &read.path);
    let path = path.as_path();

    let size = match tokio::fs::metadata(path).await {
        Ok(meta) if meta.is_dir() => {
            return Response::Error(refused(
                Error::IS_A_DIRECTORY,
                format!("{}: is a directory", read.path),
            ));
        }
        Ok(meta) => meta.len(),
        Err(e) => return Response::Error(file_error(e, &read.path)),
    };

    let mut file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(e) => return Response::Error(file_error(e, &read.path)),
    };

    // Starting past the end asks for nothing, which is an empty answer rather than an error:
    // `size` is there to say what the offset was past.
    let offset = read.offset.unwrap_or(0);
    let left = size.saturating_sub(offset);
    let want = read.len.unwrap_or(left).min(left).min(MAX_DATA) as usize;

    // The handle is this call's alone, so a cursor is as good as a position on every read:
    // nothing else can move it.
    if let Err(e) = file.seek(SeekFrom::Start(offset)).await {
        return Response::Error(file_error(e, &read.path));
    }

    let mut data = vec![0u8; want];
    let mut filled = 0;
    while filled < want {
        match file.read(&mut data[filled..]).await {
            // The file lost the bytes its size promised, so what is here is all there is.
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Response::Error(file_error(e, &read.path)),
        }
    }
    data.truncate(filled);

    Response::Read(ReadResp { data, size })
}

/// Put bytes in a file, as the answer to the `write` that sent them.
///
/// No offset means the file is to *be* `data`, which is what `create` does: made if it was
/// not there, cut to nothing if it was. An offset means only those bytes are being spoken
/// for, so the file is opened without truncating and whatever lies past them stays.
async fn write_file(session: &Session, write: &WriteCall) -> Response {
    let path = at(session.cwd(), &write.path);
    let path = path.as_path();

    // A tree the client asked to be read-only, refused with the code a read-only filesystem
    // already answers a write with -- see [`Session::read_only_tree`]. Normalized for the
    // question and not for the answer: the file is still opened at the path as it arrived,
    // the way a `read` opens one, so nothing here changes which file a write lands in.
    if let Some(tree) = session.read_only_tree(&lexical(path)) {
        return Response::Error(refused(
            Error::IO_FAILED,
            format!(
                "{}: {} is mounted read-only in this session",
                write.path,
                tree.display()
            ),
        ));
    }

    let file = match write.offset {
        None => tokio::fs::File::create(path).await,
        Some(_) => {
            tokio::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .await
        }
    };

    let mut file = match file {
        Ok(file) => file,
        Err(e) => return Response::Error(file_error(e, &write.path)),
    };

    // Seeking past the end and writing there is what leaves zeroes in the gap, the same as a
    // positioned write would.
    if let Err(e) = file.seek(SeekFrom::Start(write.offset.unwrap_or(0))).await {
        return Response::Error(file_error(e, &write.path));
    }

    if let Err(e) = file.write_all(&write.data).await {
        return Response::Error(file_error(e, &write.path));
    }

    // Before the size is asked for, because these writes are buffered and a size taken over a
    // buffer that has not gone out is the size the file used to be.
    if let Err(e) = file.flush().await {
        return Response::Error(file_error(e, &write.path));
    }

    match file.metadata().await {
        Ok(meta) => Response::Write(WriteResp { size: meta.len() }),
        Err(e) => Response::Error(file_error(e, &write.path)),
    }
}

/// What went wrong with a file, as one of the codes a requester branches on.
///
/// Everything that is not the path being absent or being a directory is
/// [`IO_FAILED`](Error::IO_FAILED): permissions, a full overlay, a name too long. The message
/// carries what the guest kernel said, since that is the part worth reading.
fn file_error(e: io::Error, path: &str) -> Error {
    let code = match e.kind() {
        io::ErrorKind::NotFound => Error::NOT_FOUND,
        io::ErrorKind::IsADirectory => Error::IS_A_DIRECTORY,
        _ => Error::IO_FAILED,
    };
    refused(code, format!("{path}: {e}"))
}

/// An execution that ended, as the answer to the `exec` that asked for it.
fn result(ended: ExecResp) -> Response {
    Response::Exec(ended)
}

/// The code to report for a command that ran, the way a shell would.
///
/// The one part of an [`ExitStatus`] that is not just `status.code()`: a command killed by a
/// signal has no code of its own, and the convention is `128 + signal`.
fn exit_code(status: &ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|s| 128 + s))
        .unwrap_or(NOT_EXECUTABLE)
}

/// A refusal, as the `error` a response carries instead of a result.
fn refused(code: i64, message: impl Into<String>) -> Error {
    Error::new(code, message)
}
