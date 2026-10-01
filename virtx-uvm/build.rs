//! Delay-loads `vulkan-1.dll` on Windows MSVC, as `virtx-uvm-host/build.rs` does for the
//! boot, and for the same reason: a host without a GPU driver has no such DLL, and the
//! server has to start there all the same.
//!
//! The server makes no GPU, but links virglrenderer anyway whenever it is built together with
//! a `gpu` host -- cargo unifies `virtx-uvm-lib/gpu` across the two -- and that imports the
//! Vulkan loader. Asked for whatever the build, then, since this package's own features do
//! not say whether the library it links has the GPU in it; a link that imports nothing from
//! the DLL only warns (LNK4199).
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo::rustc-link-arg-bins=/DELAYLOAD:vulkan-1.dll");
        println!("cargo::rustc-link-lib=delayimp");
    }
}
