//! What `browser` installs beside itself in `/abin`, all of it in `.browser/`, fetched into
//! `OUT_DIR`, checked against what is pinned here, and laid out in `OUT_DIR/install/.browser/`,
//! which xtask places at `/abin/.browser/`:
//!
//! - `agent-browser`, vercel-labs/agent-browser's static musl build, and `skill-data/`, the
//!   guides its `skills` command prints, from the same release's npm package.
//! - `chromium/`, Debian's Chromium, headless or in a window on an X display, glibc programs
//!   each with the interpreter its header names rewritten to `ld.so`: Chromium starts its own
//!   helpers again from `/proc/self/exe`, so it has to start with no loader named in front of it.
//! - `ld.so` and `lib/`, glibc and every library Chromium loads, out of Debian's packages, so
//!   that it runs on an image of any C library or of none.
//! - `fonts/`, Liberation (metric-compatible with Arial, Times and Courier), DejaVu, and Noto
//!   Sans CJK for Chinese, Japanese and Korean, with the `fonts.conf` beside this file.
//! - `licenses/`, the copyright of each package and of agent-browser.

use std::{
    collections::{HashMap, HashSet},
    env, fs, io,
    path::{Path, PathBuf},
    process::Command,
};

use sha2::{Digest as _, Sha256};

/// Where `OUT_DIR/install/.browser` is in the guest, as `/abin` is mounted: the one place it is
/// named, since the kernel reads an interpreter's path as it is written, from `/`.
const HOME: &str = "/abin/.browser";

/// agent-browser's release, and the sha256 of its static musl build for each architecture, as
/// the release's page gives them.
const AGENT_BROWSER: &str = "0.43.0";
const AGENT_BROWSER_SUMS: &[(&str, &str)] = &[
    (
        "aarch64",
        "f1d146ad95c1df627b3055475fcb971dfd02f757a1eef756a8f2bd9e19ffff4a",
    ),
    (
        "x86_64",
        "10b3b1d5fa8f53f7bc44273bcefaa64299ab77071bc83e75d96de5ed66025c89",
    ),
];
/// The sha256 of the same release's npm package, which `skill-data/` comes from.
const AGENT_BROWSER_NPM_SUM: &str =
    "e45d3b95c112da0858f8a7c836d55dbc0b7858fac47df1509d740adff65e0abb";

/// Debian's archives as they stood at one time, which snapshot.debian.org keeps for good: the
/// release and its security archive, where Debian updates Chromium; and for each architecture,
/// by Debian's name for it, the sha256 of each one's package index, which in turn gives each
/// package's. A package in both is the security archive's.
const RELEASE: &str = "trixie";
const ARCHIVES: &[(&str, &str)] = &[
    (
        "https://snapshot.debian.org/archive/debian/20261115T000000Z",
        "trixie",
    ),
    (
        "https://snapshot.debian.org/archive/debian-security/20261115T000000Z",
        "trixie-security",
    ),
];
const INDEX_SUMS: &[(&str, [&str; 2])] = &[
    (
        "arm64",
        [
            "be16b37b2f740f066ea93d35d56f04f94d63693e5a3eb421b052bf86fae75bcd",
            "222a0da0a840d7b443ace7c90867d1ce58aba04bc22692a1f09b39e5bb6104b3",
        ],
    ),
    (
        "amd64",
        [
            "7778d3e3f303b7ddb8ce0fe7c8d57473a076c6bf2e8f241f75421d2396352498",
            "950d7229d4e730fdcbbbc8e4503a8108cb447e7d0b77d6ae1cc06c2d420e062e",
        ],
    ),
];

/// The packages unpacked: Chromium's, and the libraries they depend on, followed through each
/// one's `Depends` to every `lib*` package it names; an alternative, such as GTK or a desktop
/// portal, is left out. A library Chromium loads that none of them has fails the build.
const CHROMIUM: &[&str] = &["chromium", "chromium-common"];

/// Where Chromium is in its package, all of it installed as `chromium/`.
const CHROMIUM_DIR: &str = "usr/lib/chromium";

/// Libraries NSS opens by name as it runs, which no header names: its software token, the
/// cryptography under it, and its built-in root certificates.
const OPENED: &[&str] = &["libsoftokn3.so", "libfreeblpriv3.so", "libnssckbi.so"];

/// The font packages, and the files of each installed: in a package's directory, those whose
/// names start so. Of Noto CJK only the sans, which serif and monospace fall back to as well.
const FONTS: &[(&str, &str)] = &[
    (
        "fonts-liberation",
        "usr/share/fonts/truetype/liberation/Liberation",
    ),
    (
        "fonts-dejavu-core",
        "usr/share/fonts/truetype/dejavu/DejaVu",
    ),
    (
        "fonts-noto-cjk",
        "usr/share/fonts/opentype/noto/NotoSansCJK-",
    ),
];

const PT_INTERP: u64 = 3;
const SHT_DYNAMIC: u64 = 6;
const DT_NULL: u64 = 0;
const DT_NEEDED: u64 = 1;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=fonts.conf");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("cargo names the target's architecture");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo names OUT_DIR"));
    let sha256 = |data: &[u8]| format!("{:x}", Sha256::digest(data));

    // Replaced whole, so that nothing a build before this one placed is left in it.
    let install = out.join("install");
    if install.exists() {
        fs::remove_dir_all(&install).expect("removing the last install");
    }
    let home = install.join(".browser");
    for dir in ["chromium", "lib", "fonts", "licenses"] {
        fs::create_dir_all(home.join(dir)).expect("making the install's directories");
    }

    // Debian's package indexes, each by its sum: each package's file, its sum and what it
    // depends on.
    let debian_arch = match arch.as_str() {
        "aarch64" => "arm64",
        "x86_64" => "amd64",
        _ => panic!("no Debian packages for {arch}"),
    };
    let dir = out.join(format!("debian/{debian_arch}"));
    let mut index: HashMap<String, (String, String, String)> = HashMap::new();
    let (_, sums) = INDEX_SUMS
        .iter()
        .find(|(a, _)| *a == debian_arch)
        .unwrap_or_else(|| panic!("no Debian index pinned for {debian_arch}"));
    for ((archive, dist), sum) in ARCHIVES.iter().zip(sums) {
        let at = dir.join(dist);
        fs::create_dir_all(&at).expect("making the packages' directory");
        let file = fetch(
            &at,
            &format!("{archive}/dists/{dist}/main/binary-{debian_arch}/Packages.xz"),
            |data| sha256(data) == *sum,
        );
        let mut text = String::new();
        io::Read::read_to_string(
            &mut lzma_rust2::XzReader::new(fs::File::open(&file).expect("opening an index"), true),
            &mut text,
        )
        .expect("unpacking an index");
        for stanza in text.split("\n\n") {
            let field = |name: &str| {
                stanza
                    .lines()
                    .find_map(|line| line.strip_prefix(name)?.strip_prefix(": "))
            };
            if let (Some(package), Some(path), Some(sum)) =
                (field("Package"), field("Filename"), field("SHA256"))
            {
                index.insert(
                    package.to_owned(),
                    (
                        format!("{archive}/{path}"),
                        sum.to_owned(),
                        field("Depends").unwrap_or_default().to_owned(),
                    ),
                );
            }
        }
    }

    // Chromium's packages and the libraries they depend on, and the fonts.
    let mut packages: Vec<String> = Vec::new();
    let mut queue: Vec<String> = CHROMIUM.iter().map(|name| name.to_string()).collect();
    while let Some(name) = queue.pop() {
        if packages.contains(&name) {
            continue;
        }
        let (_, _, depends) = index
            .get(&name)
            .unwrap_or_else(|| panic!("no {name} in Debian's {RELEASE} for {debian_arch}"));
        for group in depends.split(',').filter(|group| !group.contains('|')) {
            let depend = group
                .trim()
                .split([' ', '(', ':'])
                .next()
                .unwrap_or_default();
            if (depend.starts_with("lib") || depend == "zlib1g") && index.contains_key(depend) {
                queue.push(depend.to_owned());
            }
        }
        packages.push(name);
    }
    packages.extend(FONTS.iter().map(|(name, _)| name.to_string()));

    // Each unpacked together, checked by its sum.
    let root = dir.join("root");
    if root.exists() {
        fs::remove_dir_all(&root).expect("removing the packages unpacked last");
    }
    for name in &packages {
        let (url, sum, _) = index
            .get(name)
            .unwrap_or_else(|| panic!("no {name} in Debian's {RELEASE} for {debian_arch}"));
        let deb = fetch(&dir, url, |data| sha256(data) == *sum);
        // An ar archive: each member a 60-byte header, its name first and its size at 48, and
        // then its bytes, padded to an even length. The files are the member `data.tar.xz`.
        let data = fs::read(&deb).expect("reading a package");
        assert!(
            data.starts_with(b"!<arch>\n"),
            "{} is no ar archive",
            deb.display()
        );
        let mut at = 8;
        loop {
            assert!(at + 60 <= data.len(), "no data.tar.xz in {}", deb.display());
            let field = |from: usize, to: usize| {
                std::str::from_utf8(&data[at + from..at + to])
                    .expect("an ar header")
                    .trim()
            };
            let size: usize = field(48, 58).parse().expect("an ar member's size");
            if field(0, 16).trim_end_matches('/') == "data.tar.xz" {
                let body = &data[at + 60..at + 60 + size];
                tar::Archive::new(lzma_rust2::XzReader::new(body, true))
                    .unpack(&root)
                    .unwrap_or_else(|e| panic!("unpacking {}: {e}", deb.display()));
                break;
            }
            at += 60 + size + size % 2;
        }
        let copyright = root.join(format!("usr/share/doc/{name}/copyright"));
        if copyright.is_file() {
            fs::copy(&copyright, home.join("licenses").join(name)).expect("placing a copyright");
        }
    }

    // Chromium, its directory whole, with its modes.
    let chromium = home.join("chromium");
    let mut beside = HashSet::new();
    let mut dirs = vec![PathBuf::new()];
    while let Some(sub) = dirs.pop() {
        let from = root.join(CHROMIUM_DIR).join(&sub);
        for entry in fs::read_dir(&from).unwrap_or_else(|_| panic!("no {}", from.display())) {
            let entry = entry.expect("an entry");
            let path = sub.join(entry.file_name());
            let kind = entry.file_type().expect("an entry's type");
            if kind.is_dir() {
                fs::create_dir_all(chromium.join(&path)).expect("making a dir");
                dirs.push(path);
            } else if kind.is_file() {
                fs::copy(entry.path(), chromium.join(&path)).expect("placing Chromium");
                beside.insert(path.to_string_lossy().into_owned());
            }
        }
    }

    // Each of its ELF files, which the libraries are looked for from, and of its programs, with
    // `ld.so` in place of the interpreter its program header names: written over the old one,
    // which has the room, so that nothing else in the file moves.
    let interpreter = format!("{HOME}/ld.so");
    let mut queue = Vec::new();
    for name in &beside {
        let path = chromium.join(name);
        let mut elf = fs::read(&path).expect("reading Chromium");
        if !elf.starts_with(b"\x7fELF\x02\x01") {
            continue;
        }
        let (table, size) = (le(&elf, 0x20, 8), le(&elf, 0x36, 2));
        if let Some(header) = (0..le(&elf, 0x38, 2))
            .map(|i| table + i * size)
            .find(|&header| le(&elf, header, 4) == PT_INTERP)
        {
            let (at, room) = (le(&elf, header + 8, 8), le(&elf, header + 0x20, 8));
            assert!(
                (interpreter.len() as u64) < room,
                "no room for {interpreter} in {name}"
            );
            let old = &mut elf[at as usize..(at + room) as usize];
            old.fill(0);
            old[..interpreter.len()].copy_from_slice(interpreter.as_bytes());
            fs::write(&path, &elf).expect("writing Chromium");
            executable(&path);
        }
        queue.push(path);
    }
    // Chromium loads its own libraries, such as `libEGL.so`, by the names they have beside it.
    let beside: HashSet<String> = beside
        .iter()
        .filter_map(|path| Path::new(path).file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .collect();

    // Each library in the packages, by its name, wherever under the libraries' directory it is:
    // PulseAudio's are in a directory of their own, which its RUNPATH names.
    let mut libraries = HashMap::new();
    let mut dirs = vec![root.join(format!("usr/lib/{arch}-linux-gnu"))];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).expect("reading the libraries") {
            let entry = entry.expect("an entry");
            if entry.file_type().expect("an entry's type").is_dir() {
                dirs.push(entry.path());
            } else {
                libraries
                    .entry(entry.file_name().to_string_lossy().into_owned())
                    .or_insert_with(|| entry.path());
            }
        }
    }

    // Each library Chromium loads, by the name it is loaded by, and those each of them does in
    // turn, as each one's dynamic section names them: the loader too, which they all name, as
    // `ld.so`.
    let mut placed = HashSet::new();
    for name in OPENED {
        let at = home.join("lib").join(name);
        fs::copy(&libraries[*name], &at).expect("placing a library NSS opens");
        placed.insert(name.to_string());
        queue.push(at);
    }
    while let Some(file) = queue.pop() {
        let elf = fs::read(&file).unwrap_or_else(|e| panic!("reading {}: {e}", file.display()));
        assert!(
            elf.starts_with(b"\x7fELF\x02\x01"),
            "a 64-bit little-endian ELF"
        );
        let (table, size) = (le(&elf, 0x28, 8), le(&elf, 0x3a, 2));
        let section = |i: u64| table + i * size;
        let Some(dynamic) = (0..le(&elf, 0x3c, 2))
            .map(section)
            .find(|&s| le(&elf, s + 4, 4) == SHT_DYNAMIC)
        else {
            continue;
        };
        let strings = le(&elf, section(le(&elf, dynamic + 0x28, 4)) + 0x18, 8);
        let (at, length) = (le(&elf, dynamic + 0x18, 8), le(&elf, dynamic + 0x20, 8));
        let names: Vec<String> = (at..at + length)
            .step_by(16)
            .take_while(|&entry| le(&elf, entry, 8) != DT_NULL)
            .filter(|&entry| le(&elf, entry, 8) == DT_NEEDED)
            .map(|entry| {
                let name = &elf[(strings + le(&elf, entry + 8, 8)) as usize..];
                let end = name
                    .iter()
                    .position(|&b| b == 0)
                    .expect("a terminated name");
                String::from_utf8_lossy(&name[..end]).into_owned()
            })
            .collect();
        for name in names {
            if beside.contains(&name) || !placed.insert(name.clone()) {
                continue;
            }
            let from = libraries
                .get(&name)
                .and_then(|path| fs::canonicalize(path).ok())
                .unwrap_or_else(|| {
                    panic!(
                        "{} needs {name}, which none of the packages has",
                        file.display()
                    )
                });
            if name.starts_with("ld-linux") {
                let at = home.join("ld.so");
                fs::copy(&from, &at).expect("placing the loader");
                executable(&at);
            } else {
                let at = home.join("lib").join(&name);
                fs::copy(&from, &at).expect("placing a library");
                queue.push(at);
            }
        }
    }
    assert!(home.join("ld.so").is_file(), "nothing names glibc's loader");

    for (name, prefix) in FONTS {
        let prefix = root.join(prefix);
        let start = prefix.file_name().expect("a font's name").to_string_lossy();
        let dir = prefix.parent().expect("a font in a directory");
        let mut found = false;
        for entry in fs::read_dir(dir).unwrap_or_else(|_| panic!("no fonts in {name}")) {
            let entry = entry.expect("an entry");
            if entry.file_name().to_string_lossy().starts_with(&*start) {
                fs::copy(entry.path(), home.join("fonts").join(entry.file_name()))
                    .expect("placing a font");
                found = true;
            }
        }
        assert!(found, "no {start}* in {name}");
    }
    fs::copy("fonts.conf", home.join("fonts/fonts.conf")).expect("placing fonts.conf");

    // agent-browser, and its guides.
    let (_, sum) = AGENT_BROWSER_SUMS
        .iter()
        .find(|(a, _)| *a == arch)
        .unwrap_or_else(|| panic!("no agent-browser for {arch}"));
    let platform = if arch == "aarch64" { "arm64" } else { "x64" };
    let program = fetch(
        &out,
        &format!(
            "https://github.com/vercel-labs/agent-browser/releases/download/v{AGENT_BROWSER}/agent-browser-linux-musl-{platform}"
        ),
        |data| sha256(data) == *sum,
    );
    let at = home.join("agent-browser");
    fs::copy(&program, &at).expect("placing agent-browser");
    executable(&at);
    let package = fetch(
        &out,
        &format!("https://registry.npmjs.org/agent-browser/-/agent-browser-{AGENT_BROWSER}.tgz"),
        |data| sha256(data) == AGENT_BROWSER_NPM_SUM,
    );
    let file = fs::File::open(&package).expect("opening agent-browser's npm package");
    let mut tarball = tar::Archive::new(flate2::read::GzDecoder::new(file));
    for entry in tarball
        .entries()
        .expect("reading agent-browser's npm package")
    {
        let mut entry = entry.expect("reading agent-browser's npm package");
        let path = entry.path().expect("a path in the package").into_owned();
        let at = if path == Path::new("package/LICENSE") {
            home.join("licenses/agent-browser")
        } else if let Ok(rest) = path.strip_prefix("package/skill-data") {
            home.join("skill-data").join(rest)
        } else {
            continue;
        };
        fs::create_dir_all(at.parent().expect("a file in a directory")).expect("making a dir");
        entry
            .unpack(&at)
            .expect("unpacking agent-browser's npm package");
    }
}

/// `url`, fetched into `dir` unless it is there already, and the path it is at; `good` says
/// whether the bytes are the ones pinned.
fn fetch(dir: &Path, url: &str, good: impl Fn(&[u8]) -> bool) -> PathBuf {
    let path = dir.join(url.rsplit('/').next().expect("a URL with a file name"));
    if !fs::read(&path).is_ok_and(|data| good(&data)) {
        let status = Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&path)
            .arg(url)
            .status()
            .expect("running curl");
        assert!(status.success(), "fetching {url} failed");
        let data = fs::read(&path).expect("reading what curl fetched");
        assert!(good(&data), "{url} is not what is pinned for it");
    }
    path
}

/// The `width` bytes of the ELF `elf` at `at`, read as a little-endian number.
fn le(elf: &[u8], at: u64, width: usize) -> u64 {
    let at = at as usize;
    elf[at..at + width]
        .iter()
        .rev()
        .fold(0, |n, &b| n << 8 | u64::from(b))
}

/// Make `path` executable by everyone, as a program and the loader have to be.
fn executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("making {} executable: {e}", path.display()));
    }
    #[cfg(not(unix))]
    let _ = path;
}
