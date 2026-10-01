//! Delay-loads, on Windows MSVC, the DLLs a host may not have, so that this binary starts
//! without them and says what is missing rather than failing to load at all.
//!
//! * `vulkan-1.dll`, for a `gpu` build. virglrenderer links the Vulkan loader there -- the
//!   dynamic `vulkan-dload` path it has on Linux uses `dlfcn` -- so without this the boot
//!   needs `vulkan-1.dll` to *start*, and a host without a GPU driver could run no session
//!   at all rather than only no GPU one. Deferred, the DLL is loaded by the first call a GPU
//!   makes, which `vulkan::loader` in `main.rs` checks for first.
//! * `WinHvPlatform.dll` and `WinHvEmulation.dll`, libkrun's hypervisor, which exist only
//!   where the Windows Hypervisor Platform feature is turned on. Deferred, `--capabilities`
//!   still answers on a host without it, and the server's `hypervisor()` check is what
//!   tells the user to turn it on, before any boot calls into them.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    let mut dlls = Vec::new();
    if std::env::var_os("CARGO_FEATURE_VM").is_some() {
        dlls.extend(["WinHvPlatform.dll", "WinHvEmulation.dll"]);
    }
    if std::env::var_os("CARGO_FEATURE_GPU").is_some() {
        dlls.push("vulkan-1.dll");
    }
    for dll in &dlls {
        println!("cargo::rustc-link-arg-bins=/DELAYLOAD:{dll}");
    }
    if !dlls.is_empty() {
        println!("cargo::rustc-link-lib=delayimp");
    }
}
