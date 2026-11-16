//! Xvfb, the X server `display` runs, for `xvfb.rs` to build in: unpins/xvfb's static musl
//! build for the target's architecture, with its keymaps and fonts inside it. Fetched once into
//! `OUT_DIR`, checked against the sum pinned here, and unpacked there as `Xvfb`.

use std::{env, fs, io::Read as _, path::PathBuf, process::Command};

use sha2::{Digest as _, Sha256};

/// The release built in, and the sha256 of its zstd-compressed binary for each architecture,
/// as the release's own `.sha256` files give them.
const VERSION: &str = "21.1.24-1";
const SUMS: &[(&str, &str)] = &[
    (
        "aarch64",
        "08d6493d0c550bae4c32562a194e4baf0e2a2eea1ef0f92c891e2d824dbe4eea",
    ),
    (
        "x86_64",
        "3d37b304e32231532702f8173dd44287b31e5b03c474c9387820cf9e1e2a7003",
    ),
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("cargo names the target's architecture");
    let Some((_, sum)) = SUMS.iter().find(|(a, _)| *a == arch) else {
        panic!("no Xvfb for {arch}");
    };
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo names OUT_DIR"));
    let name = format!("xvfb-{VERSION}-{arch}-linux.zst");
    let packed = out.join(&name);
    let digest = |data: &[u8]| format!("{:x}", Sha256::digest(data));

    if !fs::read(&packed).is_ok_and(|data| digest(&data) == *sum) {
        let url = format!("https://github.com/unpins/xvfb/releases/download/v{VERSION}/{name}");
        let status = Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&packed)
            .arg(&url)
            .status()
            .expect("running curl");
        assert!(status.success(), "fetching {url} failed");
        let data = fs::read(&packed).expect("reading what curl fetched");
        let got = digest(&data);
        assert!(got == *sum, "{url} is {got}, not the {sum} pinned here");
    }

    let file = fs::File::open(&packed).expect("opening the fetched Xvfb");
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(file).expect("a zstd stream");
    let mut xvfb = Vec::new();
    decoder.read_to_end(&mut xvfb).expect("unpacking Xvfb");
    fs::write(out.join("Xvfb"), xvfb).expect("writing Xvfb");
}
