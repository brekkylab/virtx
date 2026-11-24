//! ONNX Runtime, built from source for the target and linked into `docread` statically.
//!
//! `ort-sys` downloads a prebuilt ONNX Runtime, and there is none for musl, so its
//! `disable-linking` is on and this does the linking instead, everything it needs fetched into
//! `OUT_DIR`:
//!
//! 1. the release's source, checked against the sha512 pinned here;
//! 2. `protoc`, which the build runs on the host, checked against the sha1 the source's own
//!    `cmake/deps.txt` pins for this host;
//! 3. for a musl target, a sysroot of Alpine's packages — musl, the Linux headers, and GCC's C++
//!    headers and static runtime — for the host's clang to compile C++ against;
//! 4. ONNX Runtime built with CMake as static libraries, its own dependencies (abseil, protobuf,
//!    onnx, re2 and the rest) fetched by its CMake and checked against that same list;
//! 5. every archive the build made linked in, with the C++ runtime they need — statically for
//!    musl, so the program stays one static ELF.
//!
//! Once built, it is kept in `OUT_DIR` and not built again until this file changes. A build for
//! the host uses CMake's own compilers, so the tests run there too.
//!
//! And the models the program reads a PDF with, and the fonts a PDF's text without an embedded
//! font is drawn in, each checked against the sha256 pinned here, into `OUT_DIR/install/.models/`:
//! what is in `OUT_DIR/install` is what the program installs beside itself in `/abin`.

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use sha1::Sha1;
use sha2::{Digest as _, Sha256, Sha512};

/// The release built.
///
/// 1.30.0, from Alpine's copy of the release tarball: github.com/microsoft/onnxruntime has
/// answered 404 since November 2026, and the sha512 below is the one Alpine recorded when it
/// packaged 1.30.0 in September 2026, before that. `ort` asks for API 17 (1.17) at the least.
const VERSION: &str = "1.30.0";
const SOURCE: &str = "https://distfiles.alpinelinux.org/distfiles/edge/onnxruntime-1.30.0.tar.gz";
const SOURCE_SHA512: &str = "10045518738889ec63e3a490d248f8cfc342775ce54b134f2996c7e47f744d1bf7332a5d57b3b3072aed5ef4dfeffd0de2593954735c5ce898048f1380b2c91c";

/// The Alpine branch the sysroot's packages come from: the one the guest's rootfs is.
const ALPINE: &str = "v3.24";

/// The packages the sysroot is made of. A `+` is `%2B` in the index's links, and so here.
/// `libgcc` is the shared runtime, which the one shared library ONNX Runtime's build makes along
/// the way links against; `docread` links the static one.
const PACKAGES: &[&str] = &[
    "musl-dev",
    "linux-headers",
    "g%2B%2B",
    "libstdc%2B%2B-dev",
    "gcc",
    "libgcc",
    "libgcc-static",
];

/// The models a PDF is read with, from docling.rs's `models-v1` release, each by its sha256 as
/// the release's `SHA256SUMS` gives it, and where it goes under `.models/`: the layout model,
/// and TableFormer's encoder, decoder (the one with a key-value cache, which decodes a page's
/// tables together) and cell boxes. A `.data` file is the weights of the model beside it.
const MODELS: &str = "https://github.com/docling-project/docling.rs/releases/download/models-v1";
const MODEL_FILES: &[(&str, &str, &str)] = &[
    (
        "layout_heron_int8.onnx",
        "layout_heron_int8.onnx",
        "387be1f67bd58ec7fc575c53966492890d91af0810866bb383af45a18cc66161",
    ),
    (
        "encoder_fp16.onnx",
        "tableformer/encoder_fp16.onnx",
        "3fb23e039b635ad34ac8a8984d9521e6bb596bd8bac2be1ce16b02deb4fb52d8",
    ),
    (
        "decoder_kv.onnx",
        "tableformer/decoder_kv.onnx",
        "c87f0ff1c314179921c0332bbee6bb1207535643a132a4a9dc4905d3a3f573a5",
    ),
    (
        "decoder_kv.onnx.data",
        "tableformer/decoder_kv.onnx.data",
        "e71523a8b7603f983088b04c9d5cb4203be117e4a0e5950028ae46a9e98103eb",
    ),
    (
        "bbox.onnx",
        "tableformer/bbox.onnx",
        "a3cd88ae198bd8ae3338fbb89cf2f054474c964ae5001611727db7240eaf1125",
    ),
    (
        "bbox.onnx.data",
        "tableformer/bbox.onnx.data",
        "e936773730de30a2d10528f69e064fd9fd222259aa65078c421f3186b5b7e36a",
    ),
];

/// The fonts a page is drawn in where a PDF names a font it does not embed, which is what the
/// layout model sees: Liberation, metric-compatible with Arial, Times New Roman and Courier New,
/// and DejaVu Sans, docling's last resort for symbols. Each goes under `.models/fonts/` in a
/// directory of its own, with its licence: the archive by its sha256, and the members taken from
/// it, their first component dropped.
const FONTS: &[(&str, &str, &str, &[&str])] = &[
    (
        "liberation",
        "https://github.com/liberationfonts/liberation-fonts/files/7261482/liberation-fonts-ttf-2.1.5.tar.gz",
        "7191c669bf38899f73a2094ed00f7b800553364f90e2637010a69c0e268f25d0",
        &["liberation-fonts-ttf-2.1.5"],
    ),
    (
        "dejavu",
        "https://github.com/dejavu-fonts/dejavu-fonts/releases/download/version_2_37/dejavu-fonts-ttf-2.37.tar.bz2",
        "fa9ca4d13871dd122f61258a80d01751d603b4d3ee14095d65453b4e846e17d7",
        &[
            "dejavu-fonts-ttf-2.37/ttf/DejaVuSans.ttf",
            "dejavu-fonts-ttf-2.37/LICENSE",
        ],
    ),
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let target = env::var("TARGET").expect("cargo names the target");
    let host = env::var("HOST").expect("cargo names the host");
    let os = env::var("CARGO_CFG_TARGET_OS").expect("cargo names the target's OS");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("cargo names the target's arch");
    let musl = env::var("CARGO_CFG_TARGET_ENV").is_ok_and(|e| e == "musl");

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo names OUT_DIR"));
    let source = out.join(format!("onnxruntime-{VERSION}"));
    let sysroot = out.join("sysroot");
    let build = out.join("build");
    let install = out.join("install");

    // `OUT_DIR` outlives a change to this file, so what was built is marked with the file it was
    // built by, and anything else is built again from the start.
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("cargo names it"));
    let script = fs::read(manifest.join("build.rs")).expect("reading build.rs");
    let stamp = format!("{target}\n{:x}\n", Sha256::digest(script));
    let built = out.join(".built");
    let fresh = fs::read_to_string(&built).is_ok_and(|was| was == stamp);
    if !fresh {
        for dir in [&build, &sysroot, &install] {
            if dir.exists() {
                fs::remove_dir_all(dir).expect("removing what an older build.rs made");
            }
        }
    }

    // What `docread` installs beside itself: the models, where docling looks for them, in a
    // `.models/` beside the program.
    let models = install.join(".models");
    for (name, at, sha256) in MODEL_FILES {
        let fetched = fetch(&out, &format!("{MODELS}/{name}"), |data| {
            format!("{:x}", Sha256::digest(data)) == *sha256
        });
        let at = models.join(at);
        fs::create_dir_all(at.parent().expect("a model in a directory")).expect("making .models");
        if fs::hard_link(&fetched, &at).is_err() && !at.exists() {
            fs::copy(&fetched, &at).expect("placing a model");
        }
    }
    for (dir, url, sha256, members) in FONTS {
        let at = models.join("fonts").join(dir);
        if at.exists() {
            continue;
        }
        let fetched = fetch(&out, url, |data| {
            format!("{:x}", Sha256::digest(data)) == *sha256
        });
        // Unpacked under another name and renamed whole, as `fetch` does.
        let part = at.with_extension("part");
        if part.exists() {
            fs::remove_dir_all(&part).expect("removing an unpacking cut off");
        }
        fs::create_dir_all(&part).expect("making .models/fonts");
        run(Command::new("tar")
            .arg("-xf")
            .arg(&fetched)
            .arg("-C")
            .arg(&part)
            .arg("--strip-components=1")
            .args(*members));
        fs::rename(&part, &at).expect("placing the fonts");
    }

    if !fresh {
        if !source.join("cmake/deps.txt").exists() {
            let tarball = fetch(&out, SOURCE, |data| {
                format!("{:x}", Sha512::digest(data)) == SOURCE_SHA512
            });
            fs::create_dir_all(&source).expect("making the source directory");
            run(Command::new("tar")
                .arg("-xzf")
                .arg(&tarball)
                .arg("-C")
                .arg(&source)
                .arg("--strip-components=1"));
        }
        let protoc = protoc(&out, &source, &host);

        let mut configure = cmake();
        configure
            .arg("-S")
            .arg(source.join("cmake"))
            .arg("-B")
            .arg(&build)
            .args(["-G", "Ninja", "-DCMAKE_BUILD_TYPE=MinSizeRel"])
            // A static library each, and nothing this program does not link: no tests, no
            // Python bindings, no benchmarks.
            .args([
                "-Donnxruntime_BUILD_SHARED_LIB=OFF",
                "-Donnxruntime_BUILD_UNIT_TESTS=OFF",
                "-Donnxruntime_ENABLE_PYTHON=OFF",
                "-Donnxruntime_BUILD_BENCHMARKS=OFF",
                "-DFETCHCONTENT_QUIET=ON",
                // Its dependencies are the ones `deps.txt` pins, never the host's: a Homebrew
                // protobuf found by `find_package` generates headers the fetched runtime does
                // not match.
                "-DFETCHCONTENT_TRY_FIND_PACKAGE_MODE=NEVER",
            ])
            .arg(format!(
                "-DONNX_CUSTOM_PROTOC_EXECUTABLE={}",
                protoc.display()
            ));
        if musl {
            alpine_sysroot(&sysroot, &arch);
            // The system's clang, which cross-compiles already, given Alpine's triple: the C++
            // headers are the GCC installation's under `usr/lib/gcc/<arch>-alpine-linux-musl/`,
            // and clang finds their target-specific half only under the triple they were
            // installed as. The system's, as `musl-cc.sh` takes, not the first on `PATH`: a
            // Homebrew LLVM's own libc++ headers would win over the sysroot's libstdc++.
            let triple = format!("{arch}-alpine-linux-musl");
            configure
                .args([
                    "-DCMAKE_SYSTEM_NAME=Linux".to_owned(),
                    format!("-DCMAKE_SYSTEM_PROCESSOR={arch}"),
                    "-DCMAKE_C_COMPILER=/usr/bin/clang".to_owned(),
                    "-DCMAKE_CXX_COMPILER=/usr/bin/clang++".to_owned(),
                    "-DCMAKE_ASM_COMPILER=/usr/bin/clang".to_owned(),
                    format!("-DCMAKE_C_COMPILER_TARGET={triple}"),
                    format!("-DCMAKE_CXX_COMPILER_TARGET={triple}"),
                    // MLAS's kernels are assembly, and without the triple they are Mach-O.
                    format!("-DCMAKE_ASM_COMPILER_TARGET={triple}"),
                    // CMake's checks compile and do not link, and what the build links along the
                    // way (flatbuffers' `flatc`, a shared provider library) links with lld: the
                    // host's default linker makes Mach-O. Programs statically, as the sysroot
                    // has the static runtime alone.
                    "-DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY".to_owned(),
                    "-DCMAKE_EXE_LINKER_FLAGS=-fuse-ld=lld -static".to_owned(),
                    "-DCMAKE_SHARED_LINKER_FLAGS=-fuse-ld=lld".to_owned(),
                    "-DCMAKE_MODULE_LINKER_FLAGS=-fuse-ld=lld".to_owned(),
                ])
                .arg(format!("-DCMAKE_SYSROOT={}", sysroot.display()))
                .arg(format!("-DCMAKE_AR={}", llvm("llvm-ar").display()))
                .arg(format!("-DCMAKE_RANLIB={}", llvm("llvm-ranlib").display()));
        } else {
            assert_eq!(
                target, host,
                "ONNX Runtime is built here for musl or for the host"
            );
        }
        run(&mut configure);

        // `all` leaves out the session library and re2, which only the shared library and the
        // tests depend on, and this links in place of both.
        let jobs = env::var("NUM_JOBS").unwrap_or_else(|_| "1".into());
        run(cmake().arg("--build").arg(&build).args([
            "--parallel",
            &jobs,
            "--target",
            "all",
            "onnxruntime_session",
            "re2",
        ]));
        fs::write(&built, stamp).expect("marking the build done");
    }

    // Every archive the build made. Which libraries ONNX Runtime is made of is its own to say,
    // and the linker takes from an archive only what something refers to.
    for archive in archives(&build) {
        link_static(&archive);
    }

    if musl {
        // GCC's directory, whose name is its version.
        let gccs = sysroot.join(format!("usr/lib/gcc/{arch}-alpine-linux-musl"));
        let gcc = fs::read_dir(&gccs)
            .expect("reading the GCC installation")
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.is_dir())
            .unwrap_or_else(|| panic!("no GCC installation in {}", gccs.display()));
        link_static(&sysroot.join("usr/lib/libstdc++.a"));
        link_static(&gcc.join("libgcc.a"));
        link_static(&gcc.join("libgcc_eh.a"));
    } else if os == "macos" {
        println!("cargo:rustc-link-lib=c++");
        println!("cargo:rustc-link-lib=framework=Foundation");
    } else {
        println!("cargo:rustc-link-lib=stdc++");
    }
}

/// Alpine's packages unpacked into `sysroot` for `arch`, unless they are there already.
///
/// The file names are read from the branch's index rather than pinned: the CDN keeps one build
/// of each package per branch and deletes the last when a rebuild lands, and within a branch
/// they are ABI-compatible by Alpine's own rule.
fn alpine_sysroot(sysroot: &Path, arch: &str) {
    if sysroot.join("usr/lib/libstdc++.a").exists()
        && sysroot.join("usr/lib/libgcc_s.so.1").exists()
    {
        return;
    }
    fs::create_dir_all(sysroot).expect("making the sysroot");
    let repo = format!("https://dl-cdn.alpinelinux.org/alpine/{ALPINE}/main/{arch}");
    let index = Command::new("curl")
        .args(["-fsSL", "--retry", "3", &format!("{repo}/")])
        .output()
        .expect("running curl");
    assert!(index.status.success(), "fetching {repo}/ failed");
    let index = String::from_utf8_lossy(&index.stdout);
    for package in PACKAGES {
        let prefix = format!("{package}-");
        let file = index
            .split('"')
            .filter(|name| {
                name.strip_prefix(&prefix)
                    .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
                    && name.ends_with(".apk")
            })
            // The newest, by the numbers in its version and release: `-r10` after `-r9`.
            .max_by_key(|name| {
                name[prefix.len()..]
                    .split(|c: char| !c.is_ascii_digit())
                    .filter_map(|n| n.parse::<u64>().ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| panic!("no {package} in {repo}"));
        let apk = fetch(
            sysroot.parent().expect("OUT_DIR"),
            &format!("{repo}/{file}"),
            |_| true,
        );
        // An apk is a gzipped tar with a signature segment in front, which tar reads past with a
        // warning and a failure status: what it unpacked is checked below instead.
        Command::new("tar")
            .arg("-xzf")
            .arg(&apk)
            .arg("-C")
            .arg(sysroot)
            .stderr(std::process::Stdio::null())
            .status()
            .expect("running tar");
    }
    assert!(
        sysroot.join("usr/include/stdio.h").exists()
            && sysroot.join("usr/lib/libstdc++.a").exists(),
        "unpacked Alpine's packages, and no libc or libstdc++ in {}",
        sysroot.display()
    );
}

/// The LLVM tool `name`: on `PATH`, or in Homebrew's keg-only LLVM. Apple's `ar` writes a symbol
/// table `rust-lld` reads as empty, so an ELF archive is LLVM's to write.
fn llvm(name: &str) -> PathBuf {
    [
        PathBuf::from(name),
        Path::new("/opt/homebrew/opt/llvm/bin").join(name),
        Path::new("/usr/local/opt/llvm/bin").join(name),
    ]
    .into_iter()
    .find(|tool| Command::new(tool).arg("--version").output().is_ok())
    .unwrap_or_else(|| panic!("no {name}: brew install llvm, or put one on PATH"))
}

/// `url`, fetched into `dir` unless it is there already, and the path it is at; `good` says
/// whether the bytes are the ones pinned. Fetched under another name and renamed whole, so a
/// fetch cut off is never taken for the file.
fn fetch(dir: &Path, url: &str, good: impl Fn(&[u8]) -> bool) -> PathBuf {
    let name = url.rsplit('/').next().expect("a URL with a file name");
    let path = dir.join(name.replace("%2B", "+"));
    if !fs::read(&path).is_ok_and(|data| good(&data)) {
        let part = path.with_extension("part");
        run(Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&part)
            .arg(url));
        fs::rename(&part, &path).expect("placing what curl fetched");
        let data = fs::read(&path).expect("reading what curl fetched");
        assert!(good(&data), "{url} is not what is pinned for it");
    }
    path
}

/// The `protoc` the source's `cmake/deps.txt` pins for `host`, fetched and unpacked into `dir`.
fn protoc(dir: &Path, source: &Path, host: &str) -> PathBuf {
    let entry = if host.contains("apple-darwin") {
        "protoc_mac_universal"
    } else if host.starts_with("x86_64") && host.contains("linux") {
        "protoc_linux_x64"
    } else if host.starts_with("aarch64") && host.contains("linux") {
        "protoc_linux_aarch64"
    } else {
        panic!("no protoc in ONNX Runtime's deps.txt runs on {host}")
    };
    let deps = fs::read_to_string(source.join("cmake/deps.txt")).expect("reading deps.txt");
    let (url, sha1) = deps
        .lines()
        .find_map(|line| {
            let mut fields = line.split(';');
            (fields.next() == Some(entry)).then(|| fields.next().zip(fields.next()))?
        })
        .unwrap_or_else(|| panic!("no {entry} in deps.txt"));

    let zip = fetch(dir, url, |data| format!("{:x}", Sha1::digest(data)) == sha1);
    let mut archive = zip::ZipArchive::new(fs::File::open(&zip).expect("opening protoc's zip"))
        .expect("protoc's zip");
    let mut binary = archive
        .by_name("bin/protoc")
        .expect("bin/protoc in its zip");
    let protoc = dir.join("protoc");
    let mut file = fs::File::create(&protoc).expect("writing protoc");
    std::io::copy(&mut binary, &mut file).expect("unpacking protoc");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&protoc, fs::Permissions::from_mode(0o755)).expect("making protoc run");
    }
    protoc
}

/// Each `lib*.a` under `dir`.
fn archives(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).expect("reading the build").flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(archives(&path));
        } else if path.extension().is_some_and(|ext| ext == "a") {
            found.push(path);
        }
    }
    found
}

/// The archive at `path`, linked statically.
fn link_static(path: &Path) {
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.strip_prefix("lib"))
        .unwrap_or_else(|| panic!("{} is not named lib<name>.a", path.display()));
    let dir = path.parent().expect("an archive in a directory");
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static={name}");
}

/// CMake, without the shell's compiler flags: a `CXXFLAGS=-stdlib=libc++` meant for the host
/// would reach every compile, the musl target's included.
fn cmake() -> Command {
    let mut cmake = Command::new("cmake");
    for flags in ["CFLAGS", "CXXFLAGS", "CPPFLAGS", "LDFLAGS", "ASMFLAGS"] {
        cmake.env_remove(flags);
    }
    cmake
}

/// `command`, run to success.
fn run(command: &mut Command) {
    let status = command
        .status()
        .unwrap_or_else(|e| panic!("running {command:?}: {e}"));
    assert!(status.success(), "{command:?} failed");
}
