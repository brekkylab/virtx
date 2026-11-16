//! `virtx-uvm-guest` -- the agent inside each of virtx-uvm's machines.
//!
//! libkrun's init has already built the root -- the image under the machine's ext4, the shares
//! mounted -- and execs this standing where the image said. What is left is to open the
//! channel, say the guest is up, and run the commands the host sends side by side, each in a
//! process group of its own, sending what they write a line at a time.
//!
//! Beside them it does what the host asks of the guest's files on the I/O channel, one request
//! at a time. What the host sees is the root as the guest's processes see it, volumes and all,
//! but for the kernel's own filesystems and the agent's; and a link, a device or a pipe is not
//! there as far as it is told: none is listed or followed at the end of a path, and none is
//! removed.
//!
//! Each channel is a named virtio-console port, not stdin and stdout: those are the guest's
//! console, where the kernel prints, and one kernel warning would corrupt a frame there.
//!
//! Before any of it, it adds to its own environment, which every command inherits, what
//! `/alib` holds for this image ([`alib_env`]): only from in here is the image's root seen.

use std::fs::{self, File};
use std::io::{self, BufRead as _, BufReader, Read, Seek as _, SeekFrom, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::UNIX_EPOCH;

#[path = "../../virtx-uvm/src/machine/wire.rs"]
mod wire;

use wire::{
    CtrlReq, CtrlResp, IO_PORT_NAME, IoFailure, IoMeta, IoReq, IoResp, MAX_DATA, PORT_NAME,
};

/// The port's writing half, shared by everything that says something on it, so two messages
/// never interleave.
type Out = Arc<Mutex<File>>;

/// The most of a line one message carries: escaped as JSON, a byte is at most six, well within
/// a frame.
const LINE_LIMIT: u64 = 64 << 10;

/// What the host does not see at the root: the kernel's filesystems, which are no files to
/// read or write, and the agent's.
const HIDDEN: [&str; 4] = ["proc", "sys", "dev", ".virtx"];

/// Where the libraries built against each C library are, and venus's manifest in each.
const ALIB_MUSL: &str = "/alib/musl";
const ALIB_GLIBC: &str = "/alib/glibc";
const ALIB_ICD: &str = "virtio_icd.json";

fn main() -> ExitCode {
    // SAFETY: nothing else runs yet; the threads start in `serve`.
    unsafe { alib_env() };
    match serve() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // To the console, which the host keeps as the machine's log.
            eprintln!("{}: {e}", env!("CARGO_BIN_NAME"));
            ExitCode::FAILURE
        }
    }
}

/// When the machine has a GPU, put `/alib`'s libraries for the image's C library on
/// `LD_LIBRARY_PATH`, after whatever the image set, and venus as the Vulkan driver when the
/// image names none. Without a GPU there is no driver to give, and `/alib`'s loader, searched
/// before the image's own directories, would only shadow a loader the image brings, such as
/// lavapipe's. Nothing without an `/alib`.
///
/// # Safety
///
/// Changes this process's environment: only while it has one thread.
unsafe fn alib_env() {
    let gpu = fs::read_dir("/dev/dri").is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with("renderD"))
    });
    if !gpu {
        return;
    }
    // The C library `/bin/sh` runs on, by the dynamic linker it names, rather than whether one
    // is installed: an image on glibc may have musl beside it.
    let musl = fs::read("/bin/sh").is_ok_and(|sh| sh.windows(8).any(|w| w == b"ld-musl-"));
    let dir = Path::new(if musl { ALIB_MUSL } else { ALIB_GLIBC });
    if !dir.is_dir() {
        return;
    }
    let path = match std::env::var("LD_LIBRARY_PATH") {
        Ok(set) if !set.is_empty() => format!("{set}:{}", dir.display()),
        _ => dir.display().to_string(),
    };
    let named = ["VK_DRIVER_FILES", "VK_ICD_FILENAMES"]
        .iter()
        .any(|var| std::env::var_os(var).is_some());
    // SAFETY: the caller's.
    unsafe {
        std::env::set_var("LD_LIBRARY_PATH", path);
        if !named {
            // Both names: loaders built before 1.3.207 read only the second.
            let icd = dir.join(ALIB_ICD);
            std::env::set_var("VK_DRIVER_FILES", &icd);
            std::env::set_var("VK_ICD_FILENAMES", &icd);
        }
    }
}

fn serve() -> io::Result<()> {
    // One descriptor, split in two: the driver allows a single opener per port.
    let mut input = File::options()
        .read(true)
        .write(true)
        .open(find(PORT_NAME)?)?;
    let out: Out = Arc::new(Mutex::new(input.try_clone()?));
    let io = File::options()
        .read(true)
        .write(true)
        .open(find(IO_PORT_NAME)?)?;
    thread::spawn(move || {
        if let Err(e) = files(io) {
            eprintln!("{}: the I/O channel: {e}", env!("CARGO_BIN_NAME"));
        }
    });

    send(&out, &CtrlResp::Ready(Vec::new()));
    while let Some(message) = wire::recv(&mut input)? {
        match message {
            CtrlReq::Exec { cmd } => exec(&out, cmd),
            // SAFETY: `kill` takes plain values; a group that is gone is an error it returns.
            CtrlReq::Signal { id, signal } => unsafe {
                libc::kill(-(id as libc::pid_t), signal);
            },
            // As libkrun's init ends the machine once its workload has.
            // SAFETY: neither takes anything but plain values.
            CtrlReq::Shutdown => unsafe {
                libc::sync();
                libc::reboot(libc::RB_AUTOBOOT);
            },
        }
    }
    Ok(())
}

/// The virtio-console port named `name`.
fn find(name: &str) -> io::Result<PathBuf> {
    let by_name = PathBuf::from("/dev/virtio-ports").join(name);
    if by_name.exists() {
        return Ok(by_name);
    }
    for entry in fs::read_dir("/sys/class/virtio-ports")?.flatten() {
        let named = fs::read_to_string(entry.path().join("name")).unwrap_or_default();
        if named.trim() == name {
            return Ok(PathBuf::from("/dev").join(entry.file_name()));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no virtio-console port named `{name}`"),
    ))
}

/// Start `cmd` in a process group of its own, answer the host with its pid -- before anything
/// it writes, which goes after -- and send how it ended once it has.
fn exec(out: &Out, cmd: Vec<String>) {
    let started = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn();
    let mut child = match started {
        Ok(child) => child,
        Err(e) => {
            send(out, &CtrlResp::Started(Err(format!("{}: {e}", cmd[0]))));
            return;
        }
    };
    let id = child.id();
    send(out, &CtrlResp::Started(Ok(id)));

    let stdout = lines(out, child.stdout.take(), move |line| CtrlResp::Stdout {
        id,
        line,
    });
    let stderr = lines(out, child.stderr.take(), move |line| CtrlResp::Stderr {
        id,
        line,
    });
    let out = Arc::clone(out);
    thread::spawn(move || {
        // Everything it wrote first, so its end is the last thing said of it.
        let _ = (stdout.join(), stderr.join());
        send(
            &out,
            &CtrlResp::Exited {
                id,
                code: code(&mut child),
            },
        );
    });
}

/// Each line `read` gives, as `wrapped` makes it a message, until it ends. One longer than
/// [`LINE_LIMIT`] goes as several, so no frame is past what the host reads.
fn lines(
    out: &Out,
    read: Option<impl Read + Send + 'static>,
    wrapped: impl Fn(String) -> CtrlResp + Send + 'static,
) -> thread::JoinHandle<()> {
    let out = Arc::clone(out);
    thread::spawn(move || {
        let Some(read) = read else { return };
        let mut read = BufReader::new(read);
        let mut line = Vec::new();
        loop {
            line.clear();
            match (&mut read).take(LINE_LIMIT).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            send(&out, &wrapped(String::from_utf8_lossy(&line).into_owned()));
        }
    })
}

/// How `child` ended: its exit code, `128 + signal` for one a signal ended.
fn code(child: &mut Child) -> i32 {
    match child.wait() {
        Ok(status) => status
            .code()
            .or(status.signal().map(|signal| 128 + signal))
            .unwrap_or(-1),
        Err(_) => -1,
    }
}

/// Tell the host `message`. A port that will not take it is a host that is gone, which hears
/// nothing more either way.
fn send(out: &Out, message: &CtrlResp) {
    let _ = wire::send(&mut *out.lock().unwrap(), message);
}

/// Do what the host asks of the files on `port`, each request answered before the next is read,
/// until it stops asking.
fn files(mut port: File) -> io::Result<()> {
    while let Some(request) = wire::recv::<IoReq>(&mut port)? {
        let data = match request {
            IoReq::Write { .. } => wire::recv_data(&mut port)?,
            _ => Vec::new(),
        };
        let mut read = Vec::new();
        let answer = file_io(request, &data, &mut read).unwrap_or_else(IoResp::Failed);
        wire::send(&mut port, &answer)?;
        if answer == IoResp::Data {
            wire::send_data(&mut port, &read)?;
        }
    }
    Ok(())
}

/// Do `request`, with `data` to write, and put what it read in `read`.
fn file_io(request: IoReq, data: &[u8], read: &mut Vec<u8>) -> Result<IoResp, IoFailure> {
    match request {
        IoReq::Stat { path } => {
            let meta = fs::symlink_metadata(shown(&path)?).map_err(failed)?;
            described(&meta)
                .map(IoResp::Meta)
                .ok_or(IoFailure::NotFound)
        }
        IoReq::ReadDir { path } => {
            let path = shown(&path)?;
            let at_root = path == Path::new("/");
            let mut entries = Vec::new();
            for entry in fs::read_dir(&path).map_err(failed)? {
                let entry = entry.map_err(failed)?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if at_root && HIDDEN.contains(&name.as_str()) {
                    continue;
                }
                // Not followed: a link is left out, whatever it points at.
                if let Some(meta) = entry.metadata().ok().as_ref().and_then(described) {
                    entries.push((name, meta));
                }
            }
            Ok(IoResp::Entries(entries))
        }
        IoReq::Read { path, offset, len } => {
            let mut file = open_file(
                &shown(&path)?,
                File::options().read(true),
                IoFailure::NotFound,
            )?;
            file.seek(SeekFrom::Start(offset)).map_err(failed)?;
            file.take(len.min(MAX_DATA as u64))
                .read_to_end(read)
                .map_err(failed)?;
            Ok(IoResp::Data)
        }
        IoReq::Write { path, offset } => {
            let mut file = open_file(
                &shown(&path)?,
                File::options().write(true).create(true),
                IoFailure::Forbidden,
            )?;
            match offset {
                Some(offset) => file.seek(SeekFrom::Start(offset)).map(drop),
                None => file.set_len(0),
            }
            .map_err(failed)?;
            file.write_all(data).map_err(failed)?;
            Ok(IoResp::Done)
        }
        IoReq::CreateDir { path } => {
            fs::create_dir(shown(&path)?).map_err(failed)?;
            Ok(IoResp::Done)
        }
        IoReq::Remove { path } => {
            let path = below_root(&path)?;
            match fs::symlink_metadata(&path).map_err(failed)? {
                meta if meta.is_dir() => fs::remove_dir_all(&path),
                meta if meta.is_file() => fs::remove_file(&path),
                _ => return Err(IoFailure::NotFound),
            }
            .map_err(failed)?;
            Ok(IoResp::Done)
        }
        IoReq::Rename { from, to, keep } => {
            let (from, to) = (below_root(&from)?, below_root(&to)?);
            if described(&fs::symlink_metadata(&from).map_err(failed)?).is_none() {
                return Err(IoFailure::NotFound);
            }
            if keep
                && let Ok(there) = fs::symlink_metadata(&to)
                && there.is_file()
            {
                fs::set_permissions(&from, fs::Permissions::from_mode(there.mode()))
                    .map_err(failed)?;
                std::os::unix::fs::lchown(&from, Some(there.uid()), Some(there.gid()))
                    .map_err(failed)?;
            }
            fs::rename(&from, &to).map_err(failed)?;
            Ok(IoResp::Done)
        }
        IoReq::Copy { from, to } => {
            let mut from = open_file(
                &shown(&from)?,
                File::options().read(true),
                IoFailure::NotFound,
            )?;
            let mut to = open_file(
                &below_root(&to)?,
                File::options().write(true).create(true),
                IoFailure::Forbidden,
            )?;
            let (source, there) = (
                from.metadata().map_err(failed)?,
                to.metadata().map_err(failed)?,
            );
            if (source.dev(), source.ino()) == (there.dev(), there.ino()) {
                return Err(IoFailure::Forbidden);
            }
            to.set_len(0).map_err(failed)?;
            io::copy(&mut from, &mut to).map_err(failed)?;
            to.set_permissions(source.permissions()).map_err(failed)?;
            Ok(IoResp::Done)
        }
    }
}

/// `path` as the guest names it, refused where it leads anywhere the host is not shown: it is
/// absolute, and steps neither up nor into a hidden directory.
fn shown(path: &str) -> Result<PathBuf, IoFailure> {
    let Some(rest) = path.strip_prefix('/') else {
        return Err(IoFailure::Forbidden);
    };
    let mut shown = PathBuf::from("/");
    for (i, part) in rest.split('/').filter(|part| !part.is_empty()).enumerate() {
        if matches!(part, "." | "..") || part.contains('\0') {
            return Err(IoFailure::Forbidden);
        }
        if i == 0 && HIDDEN.contains(&part) {
            return Err(IoFailure::NotFound);
        }
        shown.push(part);
    }
    Ok(shown)
}

/// [`shown`], and not the root, which is neither removed nor moved.
fn below_root(path: &str) -> Result<PathBuf, IoFailure> {
    let path = shown(path)?;
    match path == Path::new("/") {
        true => Err(IoFailure::Forbidden),
        false => Ok(path),
    }
}

/// The file at `path` opened with `options`, not followed, and refused with `other` unless it
/// is a regular one.
///
/// Opened without blocking and only then checked, so a pipe or a device the guest made there
/// is neither waited on nor written: `open` would wait for a pipe's other end for good.
fn open_file(
    path: &Path,
    options: &mut fs::OpenOptions,
    other: IoFailure,
) -> Result<File, IoFailure> {
    let file = options
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| match e.raw_os_error() {
            // A pipe with no reader, or a device with nothing behind it.
            Some(libc::ENXIO | libc::ENODEV) => other.clone(),
            _ => failed(e),
        })?;
    let meta = file.metadata().map_err(failed)?;
    if meta.is_dir() {
        return Err(IoFailure::IsADirectory);
    }
    if !meta.is_file() {
        return Err(other);
    }
    // SAFETY: `fcntl` takes the descriptor `file` holds open, and plain flags.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, 0) } == -1 {
        return Err(failed(io::Error::last_os_error()));
    }
    Ok(file)
}

/// What `meta` is, if it is a file or a directory.
fn described(meta: &fs::Metadata) -> Option<IoMeta> {
    let kind = meta.file_type();
    if !kind.is_file() && !kind.is_dir() {
        return None;
    }
    Some(IoMeta {
        is_dir: kind.is_dir(),
        len: if kind.is_dir() { 0 } else { meta.len() },
        modified: meta
            .modified()
            .ok()
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok()),
    })
}

/// What failing with `e` is told as. A link at the end of a path, which `O_NOFOLLOW` will not
/// open, is one the host is not shown.
fn failed(e: io::Error) -> IoFailure {
    if e.raw_os_error() == Some(libc::ELOOP) {
        return IoFailure::Forbidden;
    }
    match e.kind() {
        io::ErrorKind::NotFound => IoFailure::NotFound,
        io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty => IoFailure::Exists,
        io::ErrorKind::NotADirectory => IoFailure::NotADirectory,
        io::ErrorKind::IsADirectory => IoFailure::IsADirectory,
        io::ErrorKind::PermissionDenied
        | io::ErrorKind::ReadOnlyFilesystem
        | io::ErrorKind::CrossesDevices
        | io::ErrorKind::InvalidInput => IoFailure::Forbidden,
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded => IoFailure::Full,
        _ => IoFailure::Other(e.to_string()),
    }
}
