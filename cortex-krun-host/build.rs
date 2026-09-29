//! Delay-loads `vulkan-1.dll` for a `gpu` build on Windows MSVC.
//!
//! virglrenderer links the Vulkan loader there -- the dynamic `vulkan-dload` path it has on
//! Linux uses `dlfcn` -- so without this the boot needs `vulkan-1.dll` to *start*, and a host
//! without a GPU driver could run no session at all rather than only no GPU one. Deferred,
//! the DLL is loaded by the first call a GPU makes, which `vulkan::loader` in `main.rs` checks
//! for first.
fn main() {
    if std::env::var_os("CARGO_FEATURE_GPU").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows")
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo::rustc-link-arg-bins=/DELAYLOAD:vulkan-1.dll");
        println!("cargo::rustc-link-lib=delayimp");
    }
}
