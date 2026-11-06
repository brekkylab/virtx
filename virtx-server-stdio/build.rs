//! Delay-loads, on Windows MSVC, the DLLs a host may not have, so that a server binary starts
//! without them and only a call that needs one fails. `virtx-server-http` builds with this
//! script too.
//!
//! * `WinHvPlatform.dll` and `WinHvEmulation.dll`, libkrun's hypervisor, which exist only
//!   where the Windows Hypervisor Platform feature is turned on. The server answers
//!   everything but a boot without them, and a boot runs in a `<exe> vm` process of its own.
//! * `vulkan-1.dll`, for a `gpu` build. virglrenderer links the Vulkan loader, which comes
//!   with a GPU driver; deferred, it is loaded by the first call a GPU makes.
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
