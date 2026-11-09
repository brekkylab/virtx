//! A disk restored lazily: its file holds what has been fetched of it, and the rest is fetched as
//! it is first read (virtx-cloud's docs/fast-resume.md, phase 3).
//!
//! Beside the disk's file `<disk>`:
//!
//! - `<disk>.base`, its [`Sidecar`]: the chunk size, and the chunks it was restored from or last
//!   kept as, by name (`null`, a chunk of zeroes): what it is, but for the chunks written since.
//! - `<disk>.written`: a bit per chunk, set once a chunk is written after the base: what a save
//!   reads and hashes, the rest named as the base names them, unread.
//! - `<disk>.lazy`, while it is lazy -- some chunks not fetched yet -- and `<disk>.present`, a bit
//!   per chunk set once its data is in the file, fetched or written whole.
//!
//! - `<disk>.base.next`, for a moment: what a machine's disk was just kept as while it ran,
//!   to be its base once its machine says so ([`Lazy::rebase`]).
//!
//! A chunk not present reads as its base: zeroes for `null`, else fetched from a
//! [`ChunkSource`] first. Once every chunk is present the disk is lazy no longer
//! ([`is_lazy`]), and keeps its base.
//!
//! [`LazyDisk`] is the disk as a machine's block device reads and writes it, through imago, for
//! any disk with a base. Every other reader of a disk on the host has it [`materialize`]d first;
//! a writer [`forget`]s the base.

use std::{
    collections::HashSet,
    fmt, fs,
    io::{self, Write as _},
    os::unix::fs::FileExt as _,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use imago::{
    Storage, StorageOpenOptions,
    io_buffers::{IoVector, IoVectorMut},
    storage::drivers::CommonStorageHelper,
};
use serde::{Deserialize, Serialize};

/// How long a chunk is tried for before a read of it fails.
const FETCH_DEADLINE: Duration = Duration::from_secs(10);
/// How many chunks [`LazyDisk::fill`] fetches at once.
pub const FILL_AT_ONCE: usize = 8;
/// How many chunks of its boot set a lazy disk fetches at once, as it boots.
pub const BOOT_AT_ONCE: usize = 32;
/// How long after a disk is opened what its guest touches is its boot set, at most: it ends
/// sooner, as its machine's agent says it is ready ([`Lazy::booted`]) -- the boot alone, not what
/// a first command then reads.
pub const BOOT_WINDOW: Duration = Duration::from_secs(10);
/// The most chunks a boot set holds.
pub const BOOT_MOST: usize = 512;
/// The chunk a disk with no base notes its boot set in: as the node keeps disks, 4 MiB.
pub const UNBASED_CHUNK: u64 = 4 << 20;

/// Where a lazy disk's chunks come from: their bytes, by name, as they were kept.
pub trait ChunkSource: Send + Sync {
    /// The chunk named `name`, whole: `chunk` bytes, or fewer for a disk's last.
    fn fetch(&self, name: &str) -> io::Result<Vec<u8>>;
}

/// What a disk was restored from or last kept as: `<disk>.base`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sidecar {
    /// The bytes in a chunk.
    pub chunk: u64,
    /// The chunks, in order: each by name, `None` a chunk of zeroes. Chunks past these, of a
    /// disk grown since, are zeroes too.
    pub base: Vec<Option<String>>,
    /// Its boot set: the chunks its last boot read or wrote first ([`BOOT_WINDOW`]), fetched
    /// ahead of the rest as a lazy disk boots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub boot: Vec<usize>,
    /// The cell whose bucket its chunks are in, when not this one's: a disk moved from there,
    /// not yet kept here (virtx-cloud's docs/fast-resume.md, 1b).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

impl Sidecar {
    /// The name of chunk `index`, if it is not zeroes.
    fn named(&self, index: usize) -> Option<&str> {
        self.base.get(index).and_then(|name| name.as_deref())
    }
}

fn sidecar_path(disk: &Path) -> PathBuf {
    with_suffix(disk, ".base")
}

fn lazy_path(disk: &Path) -> PathBuf {
    with_suffix(disk, ".lazy")
}

fn written_path(disk: &Path) -> PathBuf {
    with_suffix(disk, ".written")
}

fn next_path(disk: &Path) -> PathBuf {
    with_suffix(disk, ".base.next")
}

/// What `disk`, which a machine runs on, was just kept as -- a copy of it made at a
/// [`mark`](Lazy::mark) -- put beside it, for its machine to make its base ([`Lazy::rebase`]).
pub fn stage_base(disk: &Path, sidecar: &Sidecar) -> io::Result<()> {
    write_whole(&next_path(disk), &serde_json::to_vec(sidecar)?)
}

/// Every file beside `disk` that says what it is: the sidecar first.
fn beside(disk: &Path) -> [PathBuf; 5] {
    [
        sidecar_path(disk),
        lazy_path(disk),
        present_path(disk),
        written_path(disk),
        boot_path(disk),
    ]
}

fn boot_path(disk: &Path) -> PathBuf {
    with_suffix(disk, ".boot")
}

/// The chunks `disk`'s last boot touched first, as it recorded them: none if it has not.
pub fn boot_set(disk: &Path) -> io::Result<Vec<usize>> {
    match fs::read(boot_path(disk)) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn present_path(disk: &Path) -> PathBuf {
    with_suffix(disk, ".present")
}

fn with_suffix(disk: &Path, suffix: &str) -> PathBuf {
    let mut name = disk.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

/// Whether `disk` is lazy: some of its chunks not fetched yet.
pub fn is_lazy(disk: &Path) -> bool {
    lazy_path(disk).exists() && sidecar_path(disk).exists()
}

/// Whether `disk` has a base: what it was restored from or last kept as.
pub fn has_base(disk: &Path) -> bool {
    sidecar_path(disk).exists()
}

/// `disk`'s sidecar, if it has a base.
pub fn sidecar(disk: &Path) -> io::Result<Option<Sidecar>> {
    match fs::read(sidecar_path(disk)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Which of `disk`'s chunks are present, if it has a base: as its `.present` says while it is
/// lazy, every one once it is not.
pub fn present(disk: &Path) -> io::Result<Option<Vec<bool>>> {
    let Some(sidecar) = sidecar(disk)? else {
        return Ok(None);
    };
    let chunks = chunks_of(fs::metadata(disk)?.len(), sidecar.chunk);
    Ok(Some(match is_lazy(disk) {
        true => read_present(&present_path(disk), chunks)?,
        false => vec![true; chunks],
    }))
}

/// Which of `disk`'s chunks were written since its base, if it has one.
pub fn written(disk: &Path) -> io::Result<Option<Vec<bool>>> {
    let Some(sidecar) = sidecar(disk)? else {
        return Ok(None);
    };
    let chunks = chunks_of(fs::metadata(disk)?.len(), sidecar.chunk);
    Ok(Some(read_present(&written_path(disk), chunks)?))
}

/// `disk`'s base made `sidecar` -- what it was just kept as -- and none of it written since.
/// Whether it is lazy stays as it is. For a disk nothing writes meanwhile.
pub fn set_base(disk: &Path, sidecar: &Sidecar) -> io::Result<()> {
    if sidecar.chunk == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a disk's chunks are of 0 bytes",
        ));
    }
    match fs::remove_file(written_path(disk)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    write_whole(&sidecar_path(disk), &serde_json::to_vec(sidecar)?)
}

fn chunks_of(size: u64, chunk: u64) -> usize {
    size.div_ceil(chunk) as usize
}

fn read_present(path: &Path, chunks: usize) -> io::Result<Vec<bool>> {
    let bits = match fs::read(path) {
        Ok(bits) => bits,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e),
    };
    Ok((0..chunks)
        .map(|i| {
            bits.get(i / 8)
                .is_some_and(|byte| byte & (1 << (i % 8)) != 0)
        })
        .collect())
}

/// `present` written to `path`, whole or not at all.
fn write_present(path: &Path, present: &[bool]) -> io::Result<()> {
    let mut bits = vec![0u8; present.len().div_ceil(8)];
    for (i, _) in present.iter().enumerate().filter(|(_, p)| **p) {
        bits[i / 8] |= 1 << (i % 8);
    }
    write_whole(path, &bits)
}

fn write_whole(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let partial = with_suffix(path, ".partial");
    let mut file = fs::File::create(&partial)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(partial, path)
}

/// `disk` made lazy: a sparse file of `size` bytes, its chunks those `sidecar` names, none of
/// them present yet. What was at `disk` is replaced.
pub fn make_lazy(disk: &Path, size: u64, sidecar: &Sidecar) -> io::Result<()> {
    if sidecar.chunk == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a lazy disk's chunks are of 0 bytes",
        ));
    }
    forget(disk)?;
    let file = fs::File::create(disk)?;
    file.set_len(size)?;
    file.sync_all()?;
    write_whole(&sidecar_path(disk), &serde_json::to_vec(sidecar)?)?;
    // Its boot set kept as its own until a boot of it writes another: one stopped before it
    // touched anything leaves it to the next.
    if !sidecar.boot.is_empty() {
        write_whole(&boot_path(disk), &serde_json::to_vec(&sidecar.boot)?)?;
    }
    write_whole(&lazy_path(disk), b"")
}

/// What says what `from` is -- its base, whether it is lazy, what is written -- moved to say so
/// of `to`, `from` moved there: and if `from` has none, whatever said so of `to` gone.
pub fn move_beside(from: &Path, to: &Path) -> io::Result<()> {
    forget(to)?;
    // The sidecar last: what has none has no base, whatever else is beside it.
    for (from, to) in beside(from).into_iter().zip(beside(to)).rev() {
        match fs::rename(from, to) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// What says what `from` is copied to say so of `to`, a copy of `from`'s file: a copy of a disk
/// is lazy, and written, as it was. If `from` has none, whatever said so of `to` gone.
pub fn copy_beside(from: &Path, to: &Path) -> io::Result<()> {
    forget(to)?;
    for (from, to) in beside(from).into_iter().zip(beside(to)).rev() {
        match fs::copy(from, to) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Whatever says what `disk` is gone -- its base first, so that what is left has none: as a
/// disk written on the host, or made whole there, is. Its next save reads it all.
pub fn forget(disk: &Path) -> io::Result<()> {
    let _ = fs::remove_file(next_path(disk));
    for path in beside(disk) {
        match fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// `disk` made whole, if it is lazy: every chunk not present fetched from `source` into it, and
/// then it is lazy no longer. For a reader of the disk other than its machine.
pub fn materialize(disk: &Path, source: Arc<dyn ChunkSource>) -> io::Result<()> {
    materialize_until(disk, source, &AtomicBool::new(false)).map(|_| ())
}

/// As [`materialize`], until `stop` is set: whether `disk` is whole. Stopped, what was fetched
/// is kept, said present, for whatever opens the disk next.
pub fn materialize_until(
    disk: &Path,
    source: Arc<dyn ChunkSource>,
    stop: &AtomicBool,
) -> io::Result<bool> {
    if !is_lazy(disk) {
        return Ok(true);
    }
    LazyDisk::open(disk, source)?.fill_until(FILL_AT_ONCE, stop)
}

/// Where the machine of sandbox `name` asks for its disk's chunks: a Unix socket the server
/// listens on for that machine alone, as long as it runs.
pub fn chunks_socket(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("virtx-uvm-chunks-{name}.sock"))
}

/// A chunk's name, asked for over a chunks socket: a line.
pub fn ask(name: &str) -> Vec<u8> {
    format!("{name}\n").into_bytes()
}

/// The answer to a chunk asked for: a byte, 0 for its bytes or 1 for why not; their length, 8
/// bytes little-endian; and them.
pub fn answer(chunk: Result<&[u8], &str>) -> Vec<u8> {
    let (status, body) = match chunk {
        Ok(bytes) => (0u8, bytes),
        Err(why) => (1u8, why.as_bytes()),
    };
    let mut answer = Vec::with_capacity(9 + body.len());
    answer.push(status);
    answer.extend_from_slice(&(body.len() as u64).to_le_bytes());
    answer.extend_from_slice(body);
    answer
}

/// Chunks asked for over a chunks socket, a connection each: what a machine's process fetches
/// its disk's chunks from, the server fetching them where they are kept.
pub struct SocketSource {
    socket: PathBuf,
}

impl SocketSource {
    pub fn new(socket: impl Into<PathBuf>) -> SocketSource {
        SocketSource {
            socket: socket.into(),
        }
    }
}

impl ChunkSource for SocketSource {
    fn fetch(&self, name: &str) -> io::Result<Vec<u8>> {
        use std::io::Read as _;
        let mut stream = std::os::unix::net::UnixStream::connect(&self.socket)?;
        stream.write_all(&ask(name))?;
        let mut head = [0u8; 9];
        stream.read_exact(&mut head)?;
        let length = u64::from_le_bytes(head[1..].try_into().expect("8 bytes"));
        let mut body = vec![0u8; length as usize];
        stream.read_exact(&mut body)?;
        match head[0] {
            0 => Ok(body),
            _ => Err(io::Error::other(
                String::from_utf8_lossy(&body).into_owned(),
            )),
        }
    }
}

/// No chunks: what a disk with every chunk present is opened with, which never asks.
pub struct NoChunks;

impl ChunkSource for NoChunks {
    fn fetch(&self, name: &str) -> io::Result<Vec<u8>> {
        Err(io::Error::other(format!(
            "chunk {name} asked for, of a disk with nowhere to fetch it from"
        )))
    }
}

/// What [`LazyDisk`] guards: which chunks are present, which are being fetched, which written.
struct State {
    present: Vec<bool>,
    written: Vec<bool>,
    fetching: HashSet<usize>,
    /// The last [`mark`](Lazy::mark), and the chunks written since it.
    since: Option<(u64, Vec<bool>)>,
    /// The chunks a write is going to, by how many: said written, the data not in the file yet.
    writing: std::collections::HashMap<usize, u32>,
    /// Whether `present` or `written` changed since they were last written.
    changed: bool,
    /// What the guest touched within [`BOOT_WINDOW`] of the disk being opened; taken, once
    /// written as its boot set past the window.
    booting: Option<std::collections::BTreeSet<usize>>,
    /// How many of `booting` were last written: it is written again as it grows, so a machine
    /// stopped within its window keeps what it touched.
    boot_written: usize,
    /// How the boot went, said once its window is over: what the guest waited on, and the boot
    /// set's fetch.
    waits: BootWaits,
}

/// What a lazy disk's boot waited on.
#[derive(Default)]
struct BootWaits {
    /// Guest reads and writes that waited on a chunk's fetch, and for how long in all and at most.
    count: u32,
    total: Duration,
    longest: Duration,
    /// The boot set: how many of its chunks were fetched, and how long that took.
    fetched: usize,
    fetching: Duration,
}

/// A write going to a lazy disk's chunks, from [`Lazy::before_write`] until its data is in the
/// file: a [`mark`](Lazy::mark) meanwhile says them written since.
struct Writing<'a> {
    lazy: &'a Lazy,
    chunks: Vec<usize>,
}

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        let mut state = self.lazy.state.lock().unwrap();
        for index in &self.chunks {
            if let std::collections::hash_map::Entry::Occupied(mut going) =
                state.writing.entry(*index)
            {
                *going.get_mut() -= 1;
                if *going.get() == 0 {
                    going.remove();
                }
            }
        }
    }
}

/// A lazy disk as an imago storage: reads and writes of its file, each chunk they touch made
/// present first. Cloned, the same disk: one a machine's block device reads through, another
/// [`fill`](Lazy::fill)ing it.
#[derive(Clone)]
pub struct LazyDisk(Arc<Lazy>);

impl std::ops::Deref for LazyDisk {
    type Target = Lazy;

    fn deref(&self) -> &Lazy {
        &self.0
    }
}

/// What a [`LazyDisk`] is.
pub struct Lazy {
    path: PathBuf,
    file: imago::file::File,
    /// The same file, for writing fetched chunks in.
    data: fs::File,
    /// Its size, as it was opened: only a host grows a disk, never while it is open.
    size: u64,
    sidecar: Sidecar,
    source: Arc<dyn ChunkSource>,
    /// When it was opened: its boot set is what is touched within [`BOOT_WINDOW`] of this, or
    /// until it is `booted`.
    opened: Instant,
    booted: AtomicBool,
    /// Whether it was lazy as it was opened: its boot is then said ([`BootWaits`]).
    lazy_at_open: bool,
    /// Whether it has a base: what is written since is said only then ([`LazyDisk::open_unbased`]).
    /// Given one by a [`rebase`](Lazy::rebase).
    based: AtomicBool,
    state: Mutex<State>,
    fetched: Condvar,
}

impl fmt::Debug for LazyDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LazyDisk")
            .field("path", &self.path)
            .finish()
    }
}

impl fmt::Display for LazyDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lazy:{}", self.path.display())
    }
}

impl LazyDisk {
    /// The disk at `disk`, which has a base: its chunks not present fetched from `source`, and
    /// those written told apart.
    pub fn open(disk: &Path, source: Arc<dyn ChunkSource>) -> io::Result<LazyDisk> {
        let sidecar = sidecar(disk)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} has no base", disk.display()),
            )
        })?;
        Self::open_as(disk, sidecar, true, source)
    }

    /// The disk at `disk`, which has no base -- a new one -- read and written as it is, its boot
    /// set noted alone: the first save reads it whole, and the boot set goes with it, for its
    /// first lazy boot elsewhere.
    pub fn open_unbased(disk: &Path) -> io::Result<LazyDisk> {
        let sidecar = Sidecar {
            chunk: UNBASED_CHUNK,
            base: Vec::new(),
            boot: Vec::new(),
            from: None,
        };
        Self::open_as(disk, sidecar, false, Arc::new(NoChunks))
    }

    fn open_as(
        disk: &Path,
        sidecar: Sidecar,
        based: bool,
        source: Arc<dyn ChunkSource>,
    ) -> io::Result<LazyDisk> {
        let file = imago::file::File::open(StorageOpenOptions::new().write(true).filename(disk))?;
        let data = fs::OpenOptions::new().read(true).write(true).open(disk)?;
        let size = data.metadata()?.len();
        let chunks = chunks_of(size, sidecar.chunk);
        let present = match is_lazy(disk) {
            true => read_present(&present_path(disk), chunks)?,
            false => vec![true; chunks],
        };
        let written = read_present(&written_path(disk), chunks)?;
        Ok(LazyDisk(Arc::new(Lazy {
            path: disk.to_owned(),
            file,
            data,
            size,
            sidecar,
            source,
            based: AtomicBool::new(based),
            opened: Instant::now(),
            booted: AtomicBool::new(false),
            lazy_at_open: is_lazy(disk),
            state: Mutex::new(State {
                present,
                written,
                fetching: HashSet::new(),
                since: None,
                writing: Default::default(),
                changed: false,
                booting: Some(Default::default()),
                boot_written: 0,
                waits: BootWaits::default(),
            }),
            fetched: Condvar::new(),
        })))
    }
}

impl Lazy {
    /// The chunks `offset..offset + length` touches, each with whether it is touched whole.
    fn touched(&self, offset: u64, length: u64) -> impl Iterator<Item = (usize, bool)> + '_ {
        let (chunk, size) = (self.sidecar.chunk, self.size);
        let end = offset.saturating_add(length);
        let (first, last) = (offset / chunk, end.div_ceil(chunk));
        (first..last).filter(move |_| length > 0).map(move |i| {
            let (from, to) = (i * chunk, ((i + 1) * chunk).min(size));
            (i as usize, offset <= from && end >= to)
        })
    }

    /// Chunk `index` present: fetched and written in unless it is, or is zeroes. Waits on a
    /// fetch of it already going.
    fn ensure(&self, index: usize) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.present.get(index).copied().unwrap_or(true) {
                return Ok(());
            }
            if !state.fetching.contains(&index) {
                break;
            }
            state = self.fetched.wait(state).unwrap();
        }
        let Some(name) = self.sidecar.named(index).map(str::to_owned) else {
            // Zeroes: the file's hole is them already.
            state.present[index] = true;
            state.changed = true;
            return Ok(());
        };
        state.fetching.insert(index);
        drop(state);
        let written = self.fetch_into(index, &name);
        let mut state = self.state.lock().unwrap();
        state.fetching.remove(&index);
        if written.is_ok() {
            state.present[index] = true;
            state.changed = true;
        }
        self.fetched.notify_all();
        written
    }

    /// Chunk `index`, named `name`, fetched and written into the file: tried again until the
    /// deadline.
    fn fetch_into(&self, index: usize, name: &str) -> io::Result<()> {
        let started = Instant::now();
        let mut wait = Duration::from_millis(50);
        loop {
            match self.source.fetch(name) {
                Ok(bytes) if bytes.len() as u64 <= self.sidecar.chunk => {
                    return self
                        .data
                        .write_all_at(&bytes, index as u64 * self.sidecar.chunk);
                }
                Ok(bytes) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("chunk {name} is {} bytes, more than a chunk", bytes.len()),
                    ));
                }
                Err(e) if started.elapsed() + wait > FETCH_DEADLINE => {
                    log::error!("fetching chunk {name} of {}: {e}", self.path.display());
                    return Err(io::Error::other(format!("fetching chunk {name}: {e}")));
                }
                Err(e) => {
                    log::warn!(
                        "fetching chunk {name} of {}, again: {e}",
                        self.path.display()
                    );
                    std::thread::sleep(wait);
                    wait = (wait * 2).min(Duration::from_secs(1));
                }
            }
        }
    }

    /// The chunks `offset..offset + length` touches made present before they are read.
    fn before_read(&self, offset: u64, length: u64) -> io::Result<()> {
        let touched: Vec<_> = self.touched(offset, length).collect();
        self.booting(&touched);
        for (index, _) in touched {
            self.ensure_for_guest(index)?;
        }
        Ok(())
    }

    /// The chunks `offset..offset + length` touches made present before they are written, and
    /// said written: a chunk written whole is present without its old data, unless a fetch of
    /// it is going.
    fn before_write(&self, offset: u64, length: u64) -> io::Result<Writing<'_>> {
        let touched: Vec<_> = self.touched(offset, length).collect();
        self.booting(&touched);
        let mut writing = Writing {
            lazy: self,
            chunks: Vec::with_capacity(touched.len()),
        };
        for (index, whole) in touched {
            if !whole {
                self.ensure_for_guest(index)?;
            }
            let mut state = self.state.lock().unwrap();
            while state.fetching.contains(&index) {
                state = self.fetched.wait(state).unwrap();
            }
            let state = &mut *state;
            for bits in [&mut state.present, &mut state.written] {
                if let Some(bit) = bits.get_mut(index)
                    && !*bit
                {
                    *bit = true;
                    state.changed = true;
                }
            }
            if let Some(bit) = state
                .since
                .as_mut()
                .and_then(|(_, since)| since.get_mut(index))
            {
                *bit = true;
            }
            *state.writing.entry(index).or_default() += 1;
            writing.chunks.push(index);
        }
        Ok(writing)
    }

    /// `.written`, and `.present` while it is lazy, written, if they changed; and once every
    /// chunk is present, the disk lazy no longer.
    fn persist(&self) -> io::Result<()> {
        // The boot set: written as it grows, and for the last time once its window is over --
        // so a machine stopped within it keeps what it touched.
        let over = self.booting_over();
        let (booted, said) = {
            let mut state = self.state.lock().unwrap();
            let grown = state
                .booting
                .as_ref()
                .is_some_and(|b| b.len() > state.boot_written);
            let booted = match (over, grown) {
                (true, _) => state
                    .booting
                    .take()
                    .filter(|b| b.len() > state.boot_written),
                (false, true) => state.booting.clone(),
                (false, false) => None,
            };
            if let Some(booted) = &booted {
                state.boot_written = booted.len();
            }
            // Its boot said once, as its window closes.
            let said =
                (over && self.lazy_at_open && state.waits.count + state.waits.fetched as u32 > 0)
                    .then(|| {
                        let waits = std::mem::take(&mut state.waits);
                        format!(
                            "boot of {}, lazy: {} reads waited on a fetch, {} ms in all, {} ms at \
                         most; its boot set of {} fetched in {} ms",
                            self.path.display(),
                            waits.count,
                            waits.total.as_millis(),
                            waits.longest.as_millis(),
                            waits.fetched,
                            waits.fetching.as_millis(),
                        )
                    });
            (booted, said)
        };
        if let Some(said) = said {
            // Warned, as the host hears only that much of a machine's log.
            log::warn!("{said}");
        }
        if let Some(booted) = booted {
            let booted: Vec<usize> = booted.into_iter().take(BOOT_MOST).collect();
            write_whole(&boot_path(&self.path), &serde_json::to_vec(&booted)?)?;
        }
        let mut state = self.state.lock().unwrap();
        if !state.changed {
            return Ok(());
        }
        if self.based.load(Ordering::Acquire) {
            write_present(&written_path(&self.path), &state.written)?;
        }
        let lazy = lazy_path(&self.path);
        if lazy.exists() {
            match state.present.iter().all(|p| *p) {
                // The marker first: a disk not marked lazy is whole, whatever is beside it.
                true => {
                    fs::remove_file(&lazy)?;
                    let _ = fs::remove_file(present_path(&self.path));
                }
                false => write_present(&present_path(&self.path), &state.present)?,
            }
        }
        state.changed = false;
        Ok(())
    }

    /// As [`ensure`](Self::ensure), for the guest: a wait on a fetch noted while it boots.
    fn ensure_for_guest(&self, index: usize) -> io::Result<()> {
        let present = self
            .state
            .lock()
            .unwrap()
            .present
            .get(index)
            .copied()
            .unwrap_or(true);
        if present {
            return Ok(());
        }
        let waiting = Instant::now();
        let ensured = self.ensure(index);
        if !self.booting_over() {
            let waited = waiting.elapsed();
            let waits = &mut self.state.lock().unwrap().waits;
            waits.count += 1;
            waits.total += waited;
            waits.longest = waits.longest.max(waited);
        }
        ensured
    }

    /// Mark `mark`, as the disk is about to be copied while its machine runs: what is written
    /// from now on noted apart, for a [`rebase`](Self::rebase) onto what the copy is kept as.
    /// What was written before it is on disk first, in `.written`, for the copy to have.
    /// A mark before it is forgotten.
    pub fn mark(&self, mark: u64) -> io::Result<()> {
        let mut state = self.state.lock().unwrap();
        if self.based.load(Ordering::Acquire) {
            write_present(&written_path(&self.path), &state.written)?;
        }
        // A write already said written and not in the file yet may land after the copy: said
        // written since, too.
        let mut since = vec![false; state.written.len()];
        for index in state.writing.keys() {
            if let Some(bit) = since.get_mut(*index) {
                *bit = true;
            }
        }
        state.since = Some((mark, since));
        Ok(())
    }

    /// What the copy made at mark `mark` was kept as -- staged beside it ([`stage_base`]) --
    /// made its base, and what was written since the mark what is written since that: whether
    /// it was. Not if another mark came since, or there is none staged.
    ///
    /// A crash anywhere along leaves what it reads as written a superset of what is: `.written`
    /// all written since the base before -- or since its machine started, for a disk with none
    /// -- then the base, then `.written` what was written since the mark.
    pub fn rebase(&self, mark: u64) -> io::Result<bool> {
        let next = next_path(&self.path);
        let mut state = self.state.lock().unwrap();
        if state.since.as_ref().is_none_or(|(m, _)| *m != mark) {
            let _ = fs::remove_file(&next);
            return Ok(false);
        }
        let staged: Sidecar = match fs::read(&next) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e),
        };
        if staged.chunk != self.sidecar.chunk {
            let _ = fs::remove_file(&next);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} kept in chunks of {} bytes, not {}",
                    self.path.display(),
                    staged.chunk,
                    self.sidecar.chunk
                ),
            ));
        }
        write_present(&written_path(&self.path), &state.written)?;
        fs::rename(&next, sidecar_path(&self.path))?;
        let (_, since) = state.since.take().expect("marked");
        write_present(&written_path(&self.path), &since)?;
        state.written = since;
        self.based.store(true, Ordering::Release);
        Ok(true)
    }

    /// Its machine booted -- its agent ready: what is touched from now on is not its boot set,
    /// which is written at the next sync.
    pub fn booted(&self) {
        self.booted.store(true, Ordering::Release);
    }

    fn booting_over(&self) -> bool {
        self.booted.load(Ordering::Acquire) || self.opened.elapsed() >= BOOT_WINDOW
    }

    /// `touched` -- chunk indexes -- noted as its boot set, while it is being booted.
    fn booting(&self, touched: &[(usize, bool)]) {
        if self.booting_over() {
            return;
        }
        if let Some(booting) = self.state.lock().unwrap().booting.as_mut() {
            booting.extend(touched.iter().map(|(index, _)| *index));
        }
    }

    /// Its boot set fetched, `at_once` at a time, as it boots: the chunks its last boot
    /// touched first, ahead of a guest that is about to ask for them. A chunk that cannot be
    /// fetched is left to the guest's read, or the fill.
    pub fn fetch_boot_set(&self, at_once: usize) {
        let wanted: Vec<usize> = {
            let state = self.state.lock().unwrap();
            self.sidecar
                .boot
                .iter()
                .copied()
                .filter(|i| state.present.get(*i).is_some_and(|p| !p))
                .collect()
        };
        let (count, began) = (wanted.len(), Instant::now());
        let next = Mutex::new(wanted.into_iter());
        std::thread::scope(|scope| {
            for _ in 0..at_once.max(1) {
                scope.spawn(|| {
                    loop {
                        let Some(index) = next.lock().unwrap().next() else {
                            return;
                        };
                        let _ = self.ensure(index);
                    }
                });
            }
        });
        let waits = &mut self.state.lock().unwrap().waits;
        waits.fetched = count;
        waits.fetching = began.elapsed();
    }

    /// Whether every chunk is present.
    pub fn whole(&self) -> bool {
        self.state.lock().unwrap().present.iter().all(|p| *p)
    }

    /// Every chunk not present fetched, `at_once` at a time, the file synced and the disk made
    /// plain: returns once it is, or on the first chunk that could not be fetched.
    pub fn fill(&self, at_once: usize) -> io::Result<()> {
        self.fill_until(at_once, &AtomicBool::new(false))
            .map(|_| ())
    }

    /// As [`fill`](Self::fill), until `stop` is set: the chunks being fetched then written in,
    /// which are present then said so, and no more fetched. Whether the disk is whole.
    pub fn fill_until(&self, at_once: usize, stop: &AtomicBool) -> io::Result<bool> {
        let missing: Vec<usize> = {
            let state = self.state.lock().unwrap();
            (0..state.present.len())
                .filter(|i| !state.present[*i])
                .collect()
        };
        let next = Arc::new(Mutex::new(missing.into_iter()));
        let failed = Arc::new(Mutex::new(None));
        std::thread::scope(|scope| {
            for _ in 0..at_once.max(1) {
                let (next, failed) = (Arc::clone(&next), Arc::clone(&failed));
                scope.spawn(move || {
                    loop {
                        if failed.lock().unwrap().is_some() || stop.load(Ordering::Acquire) {
                            return;
                        }
                        let Some(index) = next.lock().unwrap().next() else {
                            return;
                        };
                        if let Err(e) = self.ensure(index) {
                            *failed.lock().unwrap() = Some(e);
                        }
                    }
                });
            }
        });
        // What was fetched, said present, whatever stopped it.
        self.data.sync_all()?;
        self.persist()?;
        if let Some(e) = failed.lock().unwrap().take() {
            return Err(e);
        }
        Ok(self.whole())
    }
}

impl Storage for LazyDisk {
    fn mem_align(&self) -> usize {
        self.file.mem_align()
    }

    fn req_align(&self) -> usize {
        self.file.req_align()
    }

    fn zero_align(&self) -> usize {
        self.file.zero_align()
    }

    fn discard_align(&self) -> usize {
        self.file.discard_align()
    }

    fn size(&self) -> io::Result<u64> {
        self.file.size()
    }

    fn get_filename(&self) -> Option<PathBuf> {
        Some(self.path.clone())
    }

    unsafe fn pure_readv(&self, bufv: IoVectorMut<'_>, offset: u64) -> io::Result<()> {
        self.before_read(offset, bufv.len())?;
        // SAFETY: as the caller promised of this call.
        unsafe { self.file.pure_readv(bufv, offset) }
    }

    unsafe fn pure_writev(&self, bufv: IoVector<'_>, offset: u64) -> io::Result<()> {
        let _writing = self.before_write(offset, bufv.len())?;
        // SAFETY: as the caller promised of this call.
        unsafe { self.file.pure_writev(bufv, offset) }
    }

    unsafe fn pure_write_zeroes(&self, offset: u64, length: u64) -> io::Result<()> {
        let _writing = self.before_write(offset, length)?;
        // SAFETY: as the caller promised of this call.
        unsafe { self.file.pure_write_zeroes(offset, length) }
    }

    unsafe fn pure_write_allocated_zeroes(&self, offset: u64, length: u64) -> io::Result<()> {
        let _writing = self.before_write(offset, length)?;
        // SAFETY: as the caller promised of this call.
        unsafe { self.file.pure_write_allocated_zeroes(offset, length) }
    }

    unsafe fn pure_discard(&self, offset: u64, length: u64) -> io::Result<()> {
        let _writing = self.before_write(offset, length)?;
        // SAFETY: as the caller promised of this call.
        unsafe { self.file.pure_discard(offset, length) }
    }

    fn flush(&self) -> io::Result<()> {
        self.file.flush()
    }

    /// The file synced, then which chunks are present: a chunk's bit is on disk no later than
    /// the writes to it a guest relied on.
    fn sync(&self) -> io::Result<()> {
        self.file.sync()?;
        self.persist()
    }

    unsafe fn invalidate_cache(&self) -> io::Result<()> {
        // SAFETY: as the caller promised of this call.
        unsafe { self.file.invalidate_cache() }
    }

    fn get_storage_helper(&self) -> &CommonStorageHelper {
        self.file.get_storage_helper()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::atomic::AtomicUsize};

    use imago::io_buffers::{IoVector, IoVectorMut};

    use super::*;

    const CHUNK: u64 = 4096;

    /// Chunks by name, counting each fetch.
    #[derive(Default)]
    struct Chunks {
        held: HashMap<String, Vec<u8>>,
        fetched: AtomicUsize,
        failing: bool,
    }

    impl ChunkSource for Chunks {
        fn fetch(&self, name: &str) -> io::Result<Vec<u8>> {
            self.fetched
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.failing {
                return Err(io::Error::other("unreachable"));
            }
            self.held
                .get(name)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, name.to_owned()))
        }
    }

    impl Chunks {
        fn fetched(&self) -> usize {
            self.fetched.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// A disk of four chunks, its third zeroes, made lazy at a new path; and the source.
    fn lazy_disk(name: &str) -> (PathBuf, Arc<Chunks>, Vec<u8>) {
        let dir = std::env::temp_dir().join(format!("virtx-lazy-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut whole = Vec::new();
        let mut chunks = Chunks::default();
        let mut base = Vec::new();
        for i in 0..4u8 {
            let data = if i == 2 {
                vec![0; CHUNK as usize]
            } else {
                vec![i + 1; CHUNK as usize]
            };
            whole.extend_from_slice(&data);
            if i == 2 {
                base.push(None);
            } else {
                let name = format!("chunk-{i}");
                chunks.held.insert(name.clone(), data);
                base.push(Some(name));
            }
        }
        let disk = dir.join("disk.ext4");
        make_lazy(
            &disk,
            4 * CHUNK,
            &Sidecar {
                chunk: CHUNK,
                base,
                boot: vec![],
                from: None,
            },
        )
        .unwrap();
        (disk, Arc::new(chunks), whole)
    }

    fn read(lazy: &LazyDisk, offset: u64, length: usize) -> Vec<u8> {
        let mut buf = vec![0xAA; length];
        unsafe {
            lazy.pure_readv(IoVectorMut::from(&mut buf[..]), offset)
                .unwrap()
        };
        buf
    }

    fn write(lazy: &LazyDisk, offset: u64, data: &[u8]) {
        unsafe { lazy.pure_writev(IoVector::from(data), offset).unwrap() };
    }

    #[test]
    fn a_read_fetches_the_chunks_it_touches_alone_once() {
        let (disk, chunks, whole) = lazy_disk("read");
        let lazy = LazyDisk::open(&disk, chunks.clone()).unwrap();
        // Across the first and second chunks.
        let at = CHUNK - 10;
        assert_eq!(read(&lazy, at, 20), whole[at as usize..at as usize + 20]);
        assert_eq!(chunks.fetched(), 2);
        // Read again: nothing fetched.
        assert_eq!(read(&lazy, 0, 100), whole[..100]);
        assert_eq!(chunks.fetched(), 2);
        // Zeroes: never fetched.
        assert_eq!(read(&lazy, 2 * CHUNK, 10), vec![0; 10]);
        assert_eq!(chunks.fetched(), 2);
        assert!(!lazy.whole());
    }

    #[test]
    fn a_whole_chunk_written_is_not_fetched_and_a_partial_write_keeps_the_rest() {
        let (disk, chunks, whole) = lazy_disk("write");
        let lazy = LazyDisk::open(&disk, chunks.clone()).unwrap();
        write(&lazy, 0, &vec![9; CHUNK as usize]);
        assert_eq!(chunks.fetched(), 0);
        assert_eq!(read(&lazy, 0, 4), vec![9; 4]);
        // Two bytes into the last chunk: the rest of it as it was.
        write(&lazy, 3 * CHUNK + 1, &[7, 7]);
        assert_eq!(chunks.fetched(), 1);
        let last = read(&lazy, 3 * CHUNK, CHUNK as usize);
        assert_eq!(
            &last[..4],
            &[
                whole[3 * CHUNK as usize],
                7,
                7,
                whole[3 * CHUNK as usize + 3]
            ]
        );
    }

    #[test]
    fn what_is_present_outlives_the_disk_opened_again_once_synced() {
        let (disk, chunks, _) = lazy_disk("sync");
        {
            let lazy = LazyDisk::open(&disk, chunks.clone()).unwrap();
            read(&lazy, 0, 1);
            write(&lazy, CHUNK, &vec![5; CHUNK as usize]);
            lazy.sync().unwrap();
        }
        assert_eq!(
            present(&disk).unwrap().unwrap(),
            vec![true, true, false, false]
        );
        let lazy = LazyDisk::open(&disk, chunks.clone()).unwrap();
        assert_eq!(read(&lazy, CHUNK, 3), vec![5; 3]);
        assert_eq!(chunks.fetched(), 1, "neither fetched again");
    }

    #[test]
    fn a_filled_disk_is_whole_and_plain() {
        let (disk, chunks, whole) = lazy_disk("fill");
        let lazy = LazyDisk::open(&disk, chunks.clone()).unwrap();
        write(&lazy, 0, &vec![8; CHUNK as usize]);
        lazy.fill(3).unwrap();
        assert!(lazy.whole());
        assert_eq!(chunks.fetched(), 2, "the second and the last");
        assert!(!is_lazy(&disk));
        let mut expected = whole;
        expected[..CHUNK as usize].fill(8);
        assert_eq!(fs::read(&disk).unwrap(), expected);
    }

    #[test]
    fn a_fill_stopped_keeps_what_it_fetched_and_the_disk_lazy() {
        let (disk, chunks, whole) = lazy_disk("stopped");
        let stop = AtomicBool::new(true);
        assert!(!materialize_until(&disk, chunks.clone(), &stop).unwrap());
        assert!(is_lazy(&disk), "stopped before a chunk");
        assert_eq!(chunks.fetched(), 0);
        // Read one, then stopped: that one present, the rest fetched by what opens it next.
        {
            let lazy = LazyDisk::open(&disk, chunks.clone()).unwrap();
            read(&lazy, 0, 1);
            assert!(!lazy.fill_until(2, &stop).unwrap());
        }
        assert_eq!(
            present(&disk).unwrap().unwrap(),
            vec![true, false, false, false]
        );
        materialize(&disk, chunks.clone()).unwrap();
        assert_eq!(fs::read(&disk).unwrap(), whole);
        assert_eq!(chunks.fetched(), 3, "none fetched twice");
    }

    #[test]
    fn a_disk_says_what_was_written_since_its_base_whole_or_not() {
        let (disk, chunks, _) = lazy_disk("written");
        {
            let lazy = LazyDisk::open(&disk, chunks.clone()).unwrap();
            read(&lazy, 0, 1);
            write(&lazy, 3 * CHUNK + 1, &[7]);
            lazy.fill(2).unwrap();
        }
        assert!(!is_lazy(&disk) && has_base(&disk), "whole, its base kept");
        assert_eq!(present(&disk).unwrap().unwrap(), vec![true; 4]);
        assert_eq!(
            written(&disk).unwrap().unwrap(),
            vec![false, false, false, true],
            "read and fetched chunks are as the base names them"
        );
        // Opened again, whole: written to, nothing fetched.
        let fetched = chunks.fetched();
        {
            let disk = LazyDisk::open(&disk, chunks.clone()).unwrap();
            write(&disk, CHUNK, &[1]);
            disk.sync().unwrap();
        }
        assert_eq!(chunks.fetched(), fetched);
        assert_eq!(
            written(&disk).unwrap().unwrap(),
            vec![false, true, false, true]
        );
        // Kept as it is: its base that, nothing written since.
        let kept = Sidecar {
            chunk: CHUNK,
            base: vec![Some("a".into()), Some("b".into()), None, Some("d".into())],
            boot: vec![],
            from: None,
        };
        set_base(&disk, &kept).unwrap();
        assert_eq!(sidecar(&disk).unwrap().unwrap(), kept);
        assert_eq!(written(&disk).unwrap().unwrap(), vec![false; 4]);
        assert!(!is_lazy(&disk));
        // Written on the host: no base, and read whole by its next save.
        forget(&disk).unwrap();
        assert!(!has_base(&disk) && written(&disk).unwrap().is_none());
    }

    #[test]
    fn a_disk_rebased_at_a_mark_says_written_only_what_was_written_since_it() {
        let (disk, chunks, _) = lazy_disk("rebase");
        let lazy = LazyDisk::open(&disk, chunks.clone()).unwrap();
        write(&lazy, 1, &[9]);
        lazy.mark(1).unwrap();
        assert_eq!(
            written(&disk).unwrap().unwrap(),
            vec![true, false, false, false],
            "what was written before the mark, on disk for the copy"
        );
        write(&lazy, 3 * CHUNK, &[8]);
        let kept = Sidecar {
            chunk: CHUNK,
            base: vec![
                Some("a".into()),
                Some("chunk-1".into()),
                None,
                Some("chunk-3".into()),
            ],
            boot: vec![],
            from: None,
        };
        // Another mark's: refused, nothing changed.
        stage_base(&disk, &kept).unwrap();
        assert!(!lazy.rebase(2).unwrap());
        assert!(!next_path(&disk).exists());
        assert_eq!(
            sidecar(&disk).unwrap().unwrap().base[0].as_deref(),
            Some("chunk-0")
        );
        // Its own: the copy's base its, and only what came after the mark written.
        lazy.mark(3).unwrap();
        write(&lazy, 3 * CHUNK + 5, &[7]);
        stage_base(&disk, &kept).unwrap();
        assert!(lazy.rebase(3).unwrap());
        assert_eq!(sidecar(&disk).unwrap().unwrap(), kept);
        assert_eq!(
            written(&disk).unwrap().unwrap(),
            vec![false, false, false, true]
        );
        // Written on after: said written as before.
        write(&lazy, CHUNK, &[6]);
        lazy.sync().unwrap();
        assert_eq!(
            written(&disk).unwrap().unwrap(),
            vec![false, true, false, true]
        );
        // Once only.
        stage_base(&disk, &kept).unwrap();
        assert!(!lazy.rebase(3).unwrap());
    }

    #[test]
    fn a_write_going_as_a_disk_is_marked_is_said_written_since() {
        let (disk, chunks, _) = lazy_disk("inflight");
        let lazy = LazyDisk::open(&disk, chunks.clone()).unwrap();
        read(&lazy, 2 * CHUNK, 1);
        // Said written before the mark, its data not in the file yet: it may land after the copy.
        let going = lazy.before_write(2 * CHUNK, 1).unwrap();
        lazy.mark(1).unwrap();
        drop(going);
        let kept = Sidecar {
            chunk: CHUNK,
            base: vec![Some("a".into()), Some("b".into()), None, Some("d".into())],
            boot: vec![],
            from: None,
        };
        stage_base(&disk, &kept).unwrap();
        assert!(lazy.rebase(1).unwrap());
        assert_eq!(
            written(&disk).unwrap().unwrap(),
            vec![false, false, true, false]
        );
        // One done before the mark: not.
        lazy.mark(2).unwrap();
        stage_base(&disk, &kept).unwrap();
        assert!(lazy.rebase(2).unwrap());
        assert_eq!(written(&disk).unwrap().unwrap(), vec![false; 4]);
    }

    #[test]
    fn a_new_disk_rebased_at_a_mark_has_a_base_from_then_on() {
        let dir =
            std::env::temp_dir().join(format!("virtx-lazy-rebase-new-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("disk.ext4");
        fs::File::create(&disk)
            .unwrap()
            .set_len(4 * UNBASED_CHUNK)
            .unwrap();
        let lazy = LazyDisk::open_unbased(&disk).unwrap();
        write(&lazy, 0, &[1]);
        lazy.mark(1).unwrap();
        write(&lazy, 2 * UNBASED_CHUNK, &[2]);
        lazy.sync().unwrap();
        assert!(!has_base(&disk), "no base until rebased");
        let kept = Sidecar {
            chunk: UNBASED_CHUNK,
            base: vec![Some("a".into()), None, None, None],
            boot: vec![],
            from: None,
        };
        stage_base(&disk, &kept).unwrap();
        assert!(lazy.rebase(1).unwrap());
        assert_eq!(sidecar(&disk).unwrap().unwrap(), kept);
        assert_eq!(
            written(&disk).unwrap().unwrap(),
            vec![false, false, true, false]
        );
        write(&lazy, 3 * UNBASED_CHUNK, &[3]);
        lazy.sync().unwrap();
        assert_eq!(
            written(&disk).unwrap().unwrap(),
            vec![false, false, true, true],
            "said written once it has a base"
        );
    }

    #[test]
    fn a_boot_is_recorded_and_fetched_first_by_the_next_lazy_boot() {
        let (disk, chunks, _) = lazy_disk("boot");
        {
            let lazy = LazyDisk::open(&disk, chunks.clone()).unwrap();
            read(&lazy, 3 * CHUNK, 1);
            write(&lazy, CHUNK + 5, &[1]);
            std::thread::sleep(BOOT_WINDOW);
            // Past its window: not of the boot set.
            read(&lazy, 0, 1);
            lazy.sync().unwrap();
        }
        assert_eq!(boot_set(&disk).unwrap(), vec![1, 3]);

        // Its machine booted: what is touched after is not its boot set. And stopped within its
        // window, what it touched is kept, as far as it got.
        {
            let short = LazyDisk::open(&disk, chunks.clone()).unwrap();
            read(&short, 2 * CHUNK, 1);
            short.booted();
            read(&short, 0, 1);
            short.sync().unwrap();
        }
        assert_eq!(boot_set(&disk).unwrap(), vec![2]);

        // Restored lazily with it: those fetched ahead, the rest not.
        let again = disk.with_extension("again");
        let mut sidecar = sidecar(&disk).unwrap().unwrap();
        sidecar.boot = vec![1, 3];
        make_lazy(&again, 4 * CHUNK, &sidecar).unwrap();
        assert_eq!(
            boot_set(&again).unwrap(),
            vec![1, 3],
            "kept until a boot writes another"
        );
        let fetched = chunks.fetched();
        let lazy = LazyDisk::open(&again, chunks.clone()).unwrap();
        lazy.fetch_boot_set(BOOT_AT_ONCE);
        assert_eq!(chunks.fetched() - fetched, 2);
        assert!(!lazy.whole());
        lazy.sync().unwrap();
        assert_eq!(
            present(&again).unwrap().unwrap(),
            vec![false, true, false, true]
        );
    }

    #[test]
    fn a_disk_with_no_base_notes_its_boot_set_alone() {
        let dir = std::env::temp_dir().join(format!("virtx-lazy-unbased-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let disk = dir.join("disk.ext4");
        fs::write(&disk, vec![3u8; 3 * UNBASED_CHUNK as usize]).unwrap();
        {
            let plain = LazyDisk::open_unbased(&disk).unwrap();
            assert_eq!(read(&plain, UNBASED_CHUNK, 2), vec![3, 3]);
            write(&plain, 2 * UNBASED_CHUNK, &[9]);
            plain.booted();
            read(&plain, 0, 1);
            plain.sync().unwrap();
        }
        assert_eq!(boot_set(&disk).unwrap(), vec![1, 2]);
        assert!(!has_base(&disk) && !is_lazy(&disk));
        assert!(
            !written_path(&disk).exists(),
            "no base, so nothing said written"
        );
        assert_eq!(fs::read(&disk).unwrap()[2 * UNBASED_CHUNK as usize], 9);
    }

    #[test]
    fn materialize_makes_a_lazy_disk_whole_and_leaves_a_plain_one_be() {
        let (disk, chunks, whole) = lazy_disk("materialize");
        materialize(&disk, chunks.clone()).unwrap();
        assert_eq!(fs::read(&disk).unwrap(), whole);
        assert!(!is_lazy(&disk));
        materialize(&disk, chunks.clone()).unwrap();
        assert_eq!(chunks.fetched(), 3);
    }

    #[test]
    fn a_socket_source_reads_answers_and_their_refusals() {
        let socket = chunks_socket(&format!("test-{}", std::process::id()));
        let _ = fs::remove_file(&socket);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        std::thread::spawn(move || {
            use std::io::BufRead as _;
            for stream in listener.incoming().take(2) {
                let mut stream = stream.unwrap();
                let mut name = String::new();
                io::BufReader::new(&stream).read_line(&mut name).unwrap();
                let answered = match name.trim_end() {
                    "here" => answer(Ok(b"bytes")),
                    other => answer(Err(&format!("no {other}"))),
                };
                stream.write_all(&answered).unwrap();
            }
        });
        let source = SocketSource::new(&socket);
        assert_eq!(source.fetch("here").unwrap(), b"bytes");
        let refused = source.fetch("gone").unwrap_err();
        assert!(refused.to_string().contains("no gone"), "{refused}");
        let _ = fs::remove_file(&socket);
    }

    #[test]
    fn a_chunk_that_cannot_be_fetched_fails_the_read_and_stays_missing() {
        let (disk, _, _) = lazy_disk("failing");
        let failing = Arc::new(Chunks {
            failing: true,
            ..Chunks::default()
        });
        let lazy = LazyDisk::open(&disk, failing).unwrap();
        let started = Instant::now();
        let mut buf = [0; 10];
        let read = unsafe { lazy.pure_readv(IoVectorMut::from(&mut buf[..]), 0) };
        assert!(read.is_err());
        assert!(started.elapsed() >= FETCH_DEADLINE - Duration::from_secs(1));
        assert!(!lazy.whole());
    }
}
