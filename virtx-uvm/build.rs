//! Delay-loads, on Windows MSVC, the DLLs a host may not have, so that this binary starts
//! without them and says what is missing rather than failing to load at all.
//!
//! * `WinHvPlatform.dll` and `WinHvEmulation.dll`, libkrun's hypervisor, which exist only
//!   where the Windows Hypervisor Platform feature is turned on. Deferred, `image` and
//!   `session` run on a host without it, and `virtx_uvm::boot::hypervisor` is what tells
//!   the user to turn it on, before any boot calls into them.
//! * `vulkan-1.dll`, for a `gpu` build. virglrenderer links the Vulkan loader there, so
//!   without this the binary needs `vulkan-1.dll` to *start*. Deferred, the DLL is loaded by
//!   the first call a GPU makes, which `vulkan::loader` in `boot.rs` checks for first.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
        || std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc")
    {
        return;
    }
    let mut dlls = vec!["WinHvPlatform.dll", "WinHvEmulation.dll"];
    if std::env::var_os("CARGO_FEATURE_GPU").is_some() {
        dlls.push("vulkan-1.dll");
    }
    for dll in &dlls {
        println!("cargo::rustc-link-arg-bins=/DELAYLOAD:{dll}");
    }
    println!("cargo::rustc-link-lib=delayimp");
}
