# cortex-krun

Rust host-side bindings and runtime built on [libkrun](https://github.com/libkrun/libkrun) 2.0.

libkrun is vendored as a submodule and linked as an rlib, so there is no
`libkrun.so`/`libkrun.dylib` to ship and no C ABI in the middle — `cortex-krun`
depends on the `libkrun` crate directly.

> libkrun 2.0 is still under active development and its API is not stable. The
> `libkrun` submodule is pinned to a specific commit for that reason; bump it
> deliberately, not automatically.

## Requirements

macOS on Apple Silicon (Linux hosts are untested here). x86_64 Windows boots
too, over the Windows Hypervisor Platform, with its own setup — see
[Windows (WHP)](#windows-whp). The rest of this section is macOS.

```sh
brew install lld meson ninja
brew tap libkrun/krun
brew install libkrunfw
```

| Dependency | Why |
|---|---|
| `libkrunfw` | The guest kernel, packaged as a shared library. libkrun `dlopen`s `libkrunfw.5.dylib` at runtime; it is never linked. |
| `lld` | libkrun's init blob cross-compiles the guest init to `aarch64-unknown-linux-musl` with `clang -fuse-ld=lld`. Apple's toolchain has no lld. |
| `llvm` | Only for `cortex-abins/`, and only for `llvm-ar`: Apple's `ar` writes a Mach-O symbol table over ELF objects and `rust-lld` then reads an empty archive. The `lld` formula does not carry it; `llvm` does, and carries lld too. |
| `meson`, `ninja` | Only for `scripts/build-virglrenderer.sh` (the `gpu` feature). |

## Setup

```sh
git submodule update --init
./scripts/apply-patches.sh              # patches the libkrun and rutabaga_gfx submodules
./scripts/build-virglrenderer.sh        # only for the `gpu` feature
./scripts/fetch-musl-sysroot.sh         # only to build cortex-abins/
./scripts/build-abin.sh                 # cortex's own executables -> {home}/abin
```

## Running

```sh
cargo run --example run -- alpine:3.21 'id'          # pulls an OCI image and runs in it
CORTEX_NET=public cargo run --example run -- alpine:3.21 'apk add curl'
CORTEX_GPU=1 cargo run --features gpu --example run -- alpine:3.21 'vulkaninfo --summary'
```

One example, because there is one thing to show: a machine made out of the parts
this repository has. What a *session* is — the agent, the console protocol, `/abin`
— is the server's, and a client reaches that by spawning `cortex-krun` rather than
by running an example.

`run` is the [`session`](cortex-krun-lib/src/session/) module end to end: it pulls the
image, stitches its layers into one disk, formats a writable ext4 beside it and
boots the guest on an overlay of the two. The session is the ext4 file, so the
second run of the same image sees what the first one did:

```sh
cargo run --example run -- alpine:3.21 'echo hi > /root/note'
cargo run --example run -- alpine:3.21 'cat /root/note'     # hi
```

Deleting `data/sessions/<image>.ext4` is how one starts over. The file is
sparse — a 2 GiB ceiling costs a couple of megabytes until it is used.

`KRUN_LOG=debug` (also `info`, `warn`, `trace`) turns on libkrun's own logging.

Nothing in it is gated by host: `session` attaches the guest's console itself,
which is the one call that would otherwise differ. The cargo runner is
host-specific all the same, and the Windows setup is different throughout; see
[Windows (WHP)](#windows-whp).

`cargo run` goes through `scripts/macos-run.sh`, registered as the cargo runner
for `aarch64-apple-darwin`. It does two things that are mandatory on macOS:

1. Ad-hoc codesigns the binary with `com.apple.security.hypervisor`. No
   developer certificate is needed, but Hypervisor.framework refuses to start
   without the entitlement.
2. Puts Homebrew's `lib` on `DYLD_FALLBACK_LIBRARY_PATH`, because libkrun
   `dlopen`s `libkrunfw.5.dylib` by leaf name and `/opt/homebrew/lib` is not on
   dyld's default search path. The *fallback* path, not `DYLD_LIBRARY_PATH`:
   dyld consults the latter first for every library, absolute install names
   included, so it let the virglrenderer 0.10.4e that krunkit's Homebrew tap
   installs stand in for the one `data/` built — silently, since the GPU still
   came up.

Running the built binary directly, without the runner, will fail on both counts
— with one exception, and it is the one that matters for a console server:
`cortex-krun-host` is signed as it is *linked*, by `scripts/macos-link.sh`,
registered as the cargo linker for the same target. A console server is started
by whoever wants a session — a test, an agent runtime, a CLI — and `cargo run`
is not on that path, so the binary that calls `hv_vm_create` has to come out of
a plain `cargo build` already entitled. Nothing else linked for this target is
touched.

## Sessions — `3rdparty/_patches/libkrun-85bed71-overlay-root.patch`

[`image`](cortex-krun-lib/src/image/) answers with a disk: the layers stitched into one
read-only EROFS filesystem, shared by every machine ever booted from that image.
[`session`](cortex-krun-lib/src/session/) is what makes one of those machines writable, by
stacking an ext4 of its own over it:

```
/dev/vdb  ext4,  writable    this session's upper layer ─┐
                                                         ├─ overlay ──► /
/dev/vda  erofs, read-only   the image's disk           ─┘
```

**The stacking happens in the guest**, because overlayfs is a kernel filesystem
and the host has no way to mount one — let alone an EROFS a macOS kernel cannot
read at all. libkrun's init already knows how to mount a block device as the
root and pivot onto it (`KRUN_BLOCK_ROOT_DEVICE`), but it takes exactly one
device. The patch teaches it the two-device form: `KRUN_BLOCK_LOWER_DEVICE` and
`KRUN_BLOCK_UPPER_DEVICE`, mounted, overlaid and pivoted onto through the same
code path. It touches one file, `init/init-binary/src/fs.rs`, and a boot that
names neither variable behaves exactly as before.

**Two block devices and not a virtio-fs share for the upper layer**, which would
need no patch at all and does not work. The guest kernel refuses a FUSE
filesystem as an upperdir outright — `filesystem on %s not supported as
upperdir` — and even past that, deleting a file the image shipped is recorded as
a character device numbered `0:0`, which `mknod` on a macOS host refuses to
anything but root. It is the same wall [`image::ingest`](cortex-krun-lib/src/image/blob.rs) is
written around, and the reason layers become EROFS images rather than extracted
directories.

**The patch carries a second, smaller thing**: the guest's exit status. libkrun's
init reports it as an ioctl on the virtiofs mount, and any boot that pivots onto
a block root — the existing `KRUN_BLOCK_ROOT_DEVICE` one as much as this — has
left that mount under no path by the time the workload exits, so the status was
dropped and the host always saw 0. The init now holds a descriptor on the
virtiofs root across the pivot and sends the ioctl to that, which is what makes
`cargo run --example run -- alpine:3.21 'exit 42'` exit 42.

The ext4 is formatted on the host, by [`arcbox-ext4`][arcbox]: macOS has no
`mke2fs`, nor does Windows, so a formatter that is a library rather than a
program is the only form that works here. Pure Rust, so no C toolchain comes
with it. The guest makes the overlay's `upperdir` and `workdir` on it, which is
also what lets it pick up a disk an earlier session left.

[arcbox]: https://docs.rs/arcbox-ext4

## Mounts — `3rdparty/_patches/libkrun-85bed71-session-mounts.patch`

A session can see host directories, each one a virtio-fs share:

```rust
Session::new()
    .mount(Mount::new("./data", "/data").read_only())
    .command(["ls", "/data"])
    .enter("alpine:3.21", None)?;
```

**The host half needs no patch.** Sharing a directory under a tag is what
libkrun has always done — `krun_add_virtiofs` in the C API, `FsDevice::new` and
`FsDevice::new_read_only` in the Rust one — and read-only is a parameter of the
device rather than something bolted on afterwards.

**The guest half is what is missing, and deliberately so.** A virtio-fs share
has no notion of where it belongs: it exports a directory under a tag, and
something in the guest has to run `mount -t virtiofs <tag> <dir>`. libkrun's init
mounts the root and one tmpfs and stops there, which leaves automounting to
whoever builds the guest's boot — and for a session that is this repo. The patch
reads the rest of the `mounts` list the init already parses out of
`/.krun_config.json`, and mounts each entry as the config spells it. It touches
`init/init-binary/src/config.rs` and `init/init-binary/src/main.rs` and adds
`init/init-binary/src/mounts.rs`; the overlay-root patch owns `fs.rs`, so the two
do not stack on one file.

**The mounts go on after the pivot**, once the root is the overlay. One made
before it would sit on the boot root the pivot leaves behind, which afterwards is
under no path at all. The directory a share lands on is created if the image
shipped none, which puts it on the upper layer — so it is made once and found
again by the next session over the same one.

**Read-only is the host's, not the guest's.** `Mount::read_only` attaches the
device read-only, and the virtiofs server then refuses every write whatever the
guest mounted it with. A guest that writes anyway gets an error from the write
rather than from the mount, and `/proc/mounts` inside the guest still says `rw` —
the flag a guest could remount away is not where the guarantee lives.

Nothing here is a sandbox. libkrun's own README is explicit that a virtio-fs
share offers no protection against a guest reaching other directories on the same
host filesystem, and `read_only` bounds what a guest may write, not what it may
read.

## The `gpu` feature

Guest Vulkan goes through virtio-gpu with [venus](https://docs.mesa3d.org/drivers/venus.html):
the guest runs Mesa's venus driver, virglrenderer serves it on the host, and
MoltenVK turns that into Metal.

A session asks for one with `.gpu(true)`:

```rust
Session::new()
    .image("alpine:3.21")
    .gpu(true)
    .command(["vulkaninfo", "--summary"])
    .enter()?;
```

**Without the feature the flag is ignored**, and deliberately: a build that has no
virglrenderer to serve Vulkan runs the same session without a GPU rather than
refusing to start, so a caller never has to `cfg` around the call. The image
carries the guest half either way (`apk add mesa-vulkan-virtio vulkan-loader`);
one without it boots and finds no device.

**The console server does not ignore it.** `cortex-krun` answers a `gpu` member on
`init`, and there what is asked for is what is given: it asks the boot beside it
(`cortex-krun-host --capabilities`) whether a device can be attached and refuses
the session with `UNSUPPORTED_MACHINE` when it cannot. A client on the other end
of a pipe cannot `cfg` around anything, and a session that silently ran on the CPU
costs it the whole run to find out. `vcpus` and `memory_mib` come the same way,
with `CORTEX_KRUN_GPU`, `CORTEX_KRUN_VCPUS` and `CORTEX_KRUN_MEMORY_MIB` as this
server's own defaults for a client that says nothing.

### Which Vulkan version the guest device reports

With the current build and an `alpine:edge` guest (Mesa 26.2.3) the device
reports **1.4.343**, with `VK_KHR_synchronization2` on. The same guest reported
1.3.0 against krunkit's 0.10.4e (see the runner note above), and this section
was first written about a 1.2 cap, for the reason below. Which host change
lifted it has not been pinned down.

MoltenVK reports 1.4.357 for the same GPU on the host, so the ceiling is venus.
`vn_physical_device_init_properties()` starts from `MIN3(host device version,
VN_MAX_API_VERSION, capset vk_xml_version)`, which is 1.4 here, and then:

```c
if (!supported_extensions.KHR_synchronization2)
    ver = MIN2(VK_API_VERSION_1_2, ver);
```

`KHR_synchronization2` is off because venus ties it to sync-fd semaphore import
when built with a WSI platform:

```c
#if defined(VK_USE_PLATFORM_ANDROID_KHR) || defined(VN_USE_WSI_PLATFORM)
   const bool can_sync2 = physical_dev->renderer_sync_fd.semaphore_importable;
#else
   static const bool can_sync2 = true;
#endif
```

A sync fd is a Linux sync file, which MoltenVK cannot provide, and Alpine's
`mesa-vulkan-virtio` is built with WSI (it pulls in libx11 and wayland). Mesa
notes the coupling is for "code simplicity", so a headless Mesa build
(`-Dplatforms= -Dvulkan-drivers=virtio`) would take the compile-time `true`
branch and lift the cap to 1.3. Nothing here needs it so far.

### bfloat16 is not available, and the version is not why

`VK_KHR_shader_bfloat16` is an extension that depends only on Vulkan 1.1, so
the version cap above is irrelevant to it. The guest is ready — Mesa venus passes
it through — but **MoltenVK does not implement it**, and neither does
SPIRV-Cross, which would have to translate the SPIR-V types to MSL first.
Metal itself has had bfloat since MSL 3.1. Both upstream issues
([MoltenVK#2507](https://github.com/KhronosGroup/MoltenVK/issues/2507),
[SPIRV-Cross#2478](https://github.com/KhronosGroup/SPIRV-Cross/issues/2478))
have been idle since May 2025. `VK_KHR_cooperative_matrix` is missing for the
same reason; `VK_KHR_shader_float16_int8` (fp16) is available.

Nothing upstream supports this combination yet, so three dependencies are
patched. Each patch is small and self-contained; the reasoning is worth keeping
because none of it is obvious from the symptoms.

### virglrenderer — `3rdparty/_patches/virglrenderer-8167744-macos.patch`

The submodule is pinned to upstream main (`8167744`), not to a release. 1.3.0 —
the minimum rutabaga_gfx accepts — forced the render server on with venus and
that server was Linux-only, so macOS needed an in-process venus context and a
host-pointer blob API carried out of tree. Since then upstream grew both halves
of a real macOS path:

- **the render server as a thread** (`render-server-mode=thread`,
  `render-server-worker=thread`), so nothing is forked or spawned, and the
  server builds on macOS (kqueue for signalfd, `SOCK_STREAM` framing for
  `SOCK_SEQPACKET`);
- **memory as shm**: host-visible `VkDeviceMemory` is shm wrapped in an
  `MTLBuffer` and imported through `VK_EXT_external_memory_metal`, and the blob
  is exported as an ordinary `FD_SHM` descriptor, which
  `virgl_renderer_resource_map` simply `mmap`s.

Venus is only initialised with `VIRGL_RENDERER_RENDER_SERVER` set, so the
session asks for it on macOS (see `cortex-krun-lib/src/session/gpu.rs`). The
patch carries what upstream main still gets wrong there:

- **`vkr_metal_helpers.m` includes `venus-protocol/vulkan_metal.h`**, a path
  that went away when venus-protocol moved out to a subproject. Upstream CI
  builds no macOS, so main does not compile on it. It is `vulkan/vulkan_metal.h`
  now, as `vkr_common.h` already has it.
- **`VIRGL_RENDERER_THREAD_SYNC` is dropped on any host without eventfd**, and
  with it the proxy context's sync thread — so `ASYNC_FENCE_CB`, which rutabaga
  always sets, delivered no fence until someone called `virgl_renderer_poll`,
  and the guest found no device. The proxy context now stands a socket pair in
  for the eventfd: the render server writes one end, the sync thread polls and
  drains the other, `SO_NOSIGPIPE` on the writing end turns a late write into
  `EPIPE`. `virgl_renderer_init` keeps the flag without eventfd when only the
  render server path is asked for, since vrend and the drm renderers still
  need a real eventfd.

`-Dvulkan-dload=false` still links MoltenVK rather than `dlopen`ing it: the
dload path asks dyld for `libvulkan.1.dylib` or `libMoltenVK.dylib` by leaf
name, and Homebrew's `lib` is not on its default search path.

`scripts/build-virglrenderer.sh` handles all of it. It exports the pinned
submodule tree with `git archive` and patches the copy under `data/`, so the
`virglrenderer` submodule stays pristine. venus-protocol is a meson subproject
that is not in that tree; meson clones it at configure time, so the first build
needs the network, and its codegen needs Mako alongside the PyYAML gallium
wants. It also synthesises a `vulkan.pc`
pointing at MoltenVK, which ships none, and installs it next to the built
library so consumers need a single `PKG_CONFIG_PATH` entry. That entry is set in
`.cargo/config.toml` without `force`, so an existing `PKG_CONFIG_PATH` in your
environment wins — if you set one, add `data/virglrenderer/lib/pkgconfig` to it.

### rutabaga_gfx — `3rdparty/_patches/rutabaga_gfx-8cb133e-virgl-renderer.patch`

Published rutabaga_gfx (0.1.85) routes macOS to a stub OS backend that panics as
soon as anything asks for a descriptor. A Darwin backend built on Mach ports
landed upstream on 2026-09-02 but has not been released, so `[patch.crates-io]`
points at the submodule instead.

That backend widened `RawDescriptor` to `i64`, and `virgl_renderer.rs` was not
updated with it — it is behind an optional feature that the new macOS CI does
not build. The patch fixes those three type errors, and that is all macOS needs:
an exported blob is an shm fd, which the Darwin backend wraps as
`OwnedDescriptor::Fd`, and `map()` gets its pointer from
`virgl_renderer_resource_map`.

The same file needs work for Windows too, and it belongs in this patch rather
than a second one: two patches on one file break `apply-patches.sh`'s "already
patched" check, because the first one's context stops matching once the second
is in. `virgl_renderer.rs` opens a DRM render node, folds stderr into stdout
with `dup2`, and turns exported `int` fds into descriptors — none of which exist
on Windows. On Windows `export_blob` has to report `Unsupported`, since a
`RawDescriptor` is a `HANDLE` that does not fit in the `int` virglrenderer
exports blobs through, and `map()` has to fall back to
`virgl_renderer_resource_get_map_ptr()`. **That half is not in the patch at the
moment**: it was dropped when the patches moved under `3rdparty/` (`938bb44`),
and has to come back before a Windows `gpu` build links.

### libkrun — `3rdparty/_patches/libkrun-85bed71-gpu.patch`

One patch for both hosts, because they share the files. The macOS half:

- `resource_map_blob` on macOS called `export_blob` purely as a validity gate
  and required a dmabuf handle, which no macOS blob has — they are shm. The mapping
  only ever consumes `map_ptr`, so the gate is dropped. This is the same
  reasoning as [libkrun#740](https://github.com/libkrun/libkrun/pull/740),
  rewritten against the current rutabaga API — that PR predates the migration to
  the upstream crate and no longer applies.
- `needs_worker` was unconditionally false on aarch64, so the VMM worker thread
  never started and its receiver was dropped. The GPU device sends
  `WorkerMessage::GpuAddMapping` to that thread because only it can call into
  the hypervisor, so the send panicked the gpu worker.
- Hypervisor.framework maps in host pages, 16 KiB on Apple Silicon, while blob
  resources are sized in 4 KiB guest pages. A 132 KiB blob was rejected outright.
  Lengths are now rounded up, symmetrically in `add_mapping` and
  `remove_mapping`.

The Windows half is mostly the same code, reached: upstream gates the whole
blob-mapping path on `target_os = "macos"` — `map_sender`, the `WorkerMessage`
variants, the `Vmm::add_mapping` wrappers, `resource_map_blob` and
`resource_unmap_blob`, and the `AttachContext` that carries the channel — and
every one of those is about handing the hypervisor a host pointer, not about
macOS. They now read `any(target_os = "macos", target_os = "windows")`. What
had to be written rather than widened:

- **`WhpVm::unmap_memory`**, wrapping `WHvUnmapGpaRange`. The `whp` crate had
  only `map_memory`, so there was no way to replace a blob mapping.
- **`Vm::add_mapping`/`remove_mapping` on WHP**, the counterparts of the macOS
  ones. No rounding: WHP asks for 4 KiB, which is what a blob is already sized
  in, so the host-page arithmetic Apple Silicon needs has no equivalent here.
- **`needs_worker`** on x86_64 followed `split_irqchip` alone, so the thread the
  GPU device sends its mappings to never started. This is the same bug the
  macOS half fixed on aarch64, in the other arm of the same expression.
- `gpu/worker.rs` cleared `O_NONBLOCK` off the control eventfd with `nix`,
  which is not a dependency on Windows. It still has to be cleared, though:
  that `read()` is what parks the worker between kicks, and the Windows
  `EventFd` keeps its own non-blocking flag, so the loop spun at full tilt
  logging `WouldBlock`. The flag moved into an `AtomicBool` behind a
  `set_nonblocking`, which reaches every clone — exactly as `O_NONBLOCK` does,
  living on the open file description that `dup` shares.
- `crossbeam-channel` was a `cfg(unix)` dependency of `krun-utils`, which
  `WorkerMessage` now needs everywhere.
- The one pre-existing `#[cfg(windows)]` block in the GPU device, in
  `resource_flush`, matched `RutabagaError::Unsupported` — a variant that does
  not exist and never did. Unsupported arrives wrapped, as
  `RutabagaError::MagmaGpuError(RutabagaUnsupported)`.

These two are Rust crates that cargo resolves by path, so their patches cannot
live in an exported copy the way virglrenderer's does — `scripts/apply-patches.sh`
applies them to the submodule working trees, and both submodules are marked
`ignore = dirty` in `.gitmodules`. Running the script twice is a no-op.

## Windows (WHP)

An x86_64 Windows host boots a microVM over the Windows Hypervisor Platform:

```powershell
cargo run --example run -- alpine:3.21 'uname -srm'
```

The guest kernel there is `libkrunfw.dll`'s, which is 6.12.109 on x86_64 at the
tag `scripts/build-libkrunfw-windows.sh` pins.

Everything libkrun needs is upstream — the Windows port merged as
[libkrun#858](https://github.com/libkrun/libkrun/pull/858) on 2026-09-21 and the
submodule is pinned past it — but upstream has no Windows CI job, and the pieces
that turn a Linux rootfs into something NTFS can serve are not upstream's
problem at all. Both gaps are carried here.

libkrun's Windows port covers the `whp` crate, the x86_64 Windows arch backend,
the WHP IOAPIC and MMIO device managers, Windows paths through virtio
console/fs/blk/balloon/vsock/net, an IOCP-backed epoll and an Event-backed
eventfd in `utils`, the `Vm`/`Vcpu` pair on WHP in
`src/libkrun/src/vmm/windows/vstate.rs`, `KRUNFW_NAME = "libkrunfw.dll"`, and a
clang cross-linker for the guest init.

The 2.0 public Rust API was `std::os::fd` only. On Windows `ConsoleBuilder` takes
`BorrowedHandle` where it takes `BorrowedFd` elsewhere, which is why
[`session`](cortex-krun-lib/src/session/) owns that call rather than leaving it to
a caller: the two spellings differ by little more than `AsFd` → `AsHandle`, and
holding the difference in one place is what lets everything above it be written
once.

It also hands the console no stdin. libkrun's Windows console parks its input
thread in a blocking `ReadFile`, and the VMM joins that thread before it exits
— so a tty stdin means the process runs the command, prints, and then hangs
forever. Ctrl-C does not help: raw mode clears `ENABLE_PROCESSED_INPUT`, so it
arrives as a byte rather than a signal. Nothing here needs guest stdin, and
leaving it out avoids the code path entirely; an interactive console on Windows
would need the bug fixed upstream.

### Requirements

- **x86_64.** The WHP backend is written against `WHvX64Register*` and reads
  `CPUID`/`RDTSC` directly, so Windows on ARM is not a target.
- The *Windows Hypervisor Platform* optional feature, and firmware
  virtualization enabled.
- **Rust 1.94 or newer**, above this package's declared `rust-version`. `whp`'s
  `detect_tsc_frequency` calls `core::arch::x86_64::__cpuid` from safe code, and
  those intrinsics only became safe in
  [1.94](https://github.com/rust-lang/stdarch/pull/1935). On 1.93 and earlier
  the `krun-whp` crate does not compile.
- MSVC build tools — `bzip2-sys` and `zstd-sys` compile C on x86_64 — plus
  clang and lld, which the guest init is linked with, and
  `rustup target add x86_64-unknown-linux-musl`.
- **Docker**, in Linux-container mode, to build `libkrunfw.dll`. The reason is
  below.

For the `gpu` feature, additionally:

- The **Vulkan SDK**, and `LIBCLANG_PATH` pointing at a directory holding
  `libclang.dll`. `bindgen` builds `krun_display` and needs that DLL; having
  `clang.exe` on `PATH` is not enough, even though an LLVM install keeps both
  in the same directory — `clang-sys` does not look there. Without it the
  build stops at `krun-display` with `Unable to find libclang`.

  ```powershell
  $env:LIBCLANG_PATH = 'C:\path\to\llvm\bin'
  ```

  Set it once per machine in `%USERPROFILE%\.cargo\config.toml` to be done with
  it, since the path is machine-specific and this repo's `.cargo/config.toml`
  is tracked:

  ```toml
  [env]
  LIBCLANG_PATH = 'C:\path\to\llvm\bin'
  ```

One host setting is worth checking before anything else: **Smart App Control**
blocks unsigned executables, which is every build script cargo compiles and
every binary it produces. With it on, `cargo build` fails with `os error 4551`
before it gets anywhere near libkrun. Turning it off is a one-way door — it
cannot be re-enabled without reinstalling Windows.

### Setup

```powershell
git submodule update --init
sh scripts/apply-patches.sh             # Git Bash; patches the submodules
sh scripts/build-libkrunfw-windows.sh   # guest kernel -> data/krunfw/
.\scripts\build-virglrenderer-windows.ps1   # only for the `gpu` feature
cargo run --example run -- alpine:3.21 'uname -srm'
```

`cargo run` goes through [`scripts/windows-run.ps1`](scripts/windows-run.ps1),
registered as the cargo runner for `x86_64-pc-windows-msvc`. It puts
`data/krunfw/` on `PATH`, because libkrun resolves the guest kernel with
`LoadLibrary("libkrunfw.dll")` by leaf name. Running the built binary directly
fails with `Error: FileNotFound`.

### `libkrunfw.dll` has to be cross-compiled from Linux

libkrunfw gained a Windows target in
[libkrunfw#122](https://github.com/containers/libkrunfw/pull/122), but its
releases ship no DLL and `make OS=Windows` cross-compiles from a *Linux* host
with MinGW-w64: it configures and builds a Linux kernel, then wraps `vmlinux`
in a DLL. It uses its own kernel config — Hyper-V enlightenments on, and the
kernel bundle aligned to 4 KiB rather than 64 KiB to match WHP's page
granularity.

None of that runs on Windows, so
[`scripts/build-libkrunfw-windows.sh`](scripts/build-libkrunfw-windows.sh) does
it in a throwaway `ubuntu:24.04` container and copies the DLL back into
`data/krunfw/`. It pins libkrunfw to a tag for the same reason the submodules
are pinned. Expect 10–20 minutes on the first run; it is a full kernel build.

### libkrun — `3rdparty/_patches/libkrun-85bed71-windows-virtiofs.patch`

Four bugs in the Windows virtio-fs passthrough, all invisible to a CI job that
never boots a guest. The first two are on the path every `execve` takes.

- **`read_lx_symlink_by_handle` returned the reparse data from offset 8**, but
  an LX symlink's data opens with a 4-byte version field and the target follows
  it. `readlink("/bin/sh")` answered `\x02\x00\x00\x00/bin/busybox`, the guest
  kernel looked that up, and the host rejected the name — surfacing in the guest
  as the unmapped Windows error 123 wearing the Linux errno of the same number,
  `ENOMEDIUM`.
- **`getxattr` answered `EACCES` for `security.capability`**, to stop the guest
  reading the attributes that back the emulation. But the guest kernel reads
  that attribute itself on every `execve`, and anything other than `ENODATA`
  there fails the exec. The macOS backend hides only its own mode-encoding
  xattr, which is what this now does too.
- **`symlink()` created a native NT symlink**, through
  `std::os::windows::fs::symlink_file`. That needs
  `SeCreateSymbolicLinkPrivilege`, so every guest `symlink(2)` failed with
  `ERROR_PRIVILEGE_NOT_HELD` unless the VMM ran elevated — `apk add mesa` lost
  21 files that way, every one of them an `.so.N` — and even with the privilege
  a native symlink cannot hold `/bin/busybox`, since Windows resolves it
  against the current drive. It now writes an `IO_REPARSE_TAG_LX_SYMLINK`
  reparse point: no privilege, target stored verbatim, and the same
  representation `read_lx_symlink_by_handle` already read. `write_override_stat` grew a
  no-follow path with it, because the mode bits of a link belong on the link
  and nothing on the host can resolve an LX reparse tag — a plain `fs::write`
  to the stream fails with `ERROR_CANT_ACCESS_FILE`.

- **`rename` moved the file and left every cached inode pointing at the old
  path.** An inode's stored path is how `getattr`, `open` and `setattr` reach
  the file, so an inode the guest already held went on naming something that
  was no longer there: `mv a b` followed by `stat b` answered ENOENT, while
  `cat b` — which looks the name up afresh — read it fine. `do_lookup` already
  repairs a path when it sees one, and `InodeData::update_path_if_changed`
  exists for exactly this, with no caller that makes it happen on a rename.
  Now the rename updates the moved inode and, for a directory, its whole
  subtree. `apk` renames every file it unpacks into place, so this is on the
  path of every package install; `MultikeyBTreeMap` gained a `values()` to
  walk the subtree with.

`setattr` still follows a symlink when it touches the emulation's streams, so
`lchown` on one fails and `apk` reports `failed to preserve <file>: owner`.
Ownership is uniformly root here and the install completes, so it is a wart
rather than a blocker.

### libkrun — `3rdparty/_patches/libkrun-85bed71-windows-vsock.patch`

Three stalls in TSI, all of them credit mistaken for something else, and all of
them made permanent by the same property of this epoll: it is
`WSAEventSelect`-backed, so re-arming interest does not re-announce bytes that
landed while the interest was off.

- **UDP carried nothing, so nothing resolved.** `recv_to_pkt` for datagrams
  honoured the peer's credit, found none, and answered `WaitForCredit`; the
  muxer then parked the socket on an empty polling interest and asked the guest
  for credit that never came, so every DNS reply was dropped rather than merely
  delayed. The unix side disables UDP credit accounting outright — "until is
  fixed in the kernel" — and this now does the same. `recv()` on a bound,
  unconnected UDP socket turned out to be fine on Windows; that was the other
  suspect.
- **UDP then failed at random, roughly one lookup in several.** With the credit
  test gone, the datagram read loop ended the only way it can — `WSAEWOULDBLOCK`
  from a drained socket — and that was still being reported as `WaitForCredit`.
  So every *successful* read disarmed the socket and asked for credit, and the
  next reply arrived to an interest that was off. The unix side answers `Error`
  for `EWOULDBLOCK`, which stops the loop and leaves the socket armed; this now
  does too.
- **TCP stopped for good whenever the guest fell behind.** A stream proxy that
  runs out of guest credit parks its polling interest and asks for more; when
  the credit update comes back, `update_peer_credit` re-arms interest — and on
  Windows that is not enough. This epoll is IOCP-backed and reports only what
  arrives *after* a registration, so the bytes that stranded the proxy, which
  arrived while its interest was empty, are never announced, and a sender with
  nothing left to send never produces another edge. The code knows this: it
  pulls data directly to "bypass the missed Windows edge-trigger" — but only
  behind `status == WaitingCreditUpdate && peer_avail_credit() > 0`. A credit
  update that arrives before the guest has actually freed space fails that
  test, takes the quiet path, and waits forever. The drain is now
  unconditional. Symptom: `wget -O - … | tar -xz` onto virtiofs stops about a
  third of the way through a large file, with the guest still running, every
  device idle, and nothing in the log but the guest kernel asking for entropy.

There is no external-kernel payload on Windows: `linux-loader` is under
`cfg(unix)` upstream, because no `linux-loader` release builds for Windows. The
guest kernel comes from `libkrunfw.dll` and nothing here needs the other path.

### A guest command is an argument, and arguments do not survive the trip

`cargo run --example run -- alpine:3.21 '<script>'` reaches the binary through
`powershell -File scripts/windows-run.ps1`, and PowerShell's `-File` argument
parsing does not carry a multi-line shell script intact. Indented lines vanish
outright — an `if` body or a function body comes out empty, leaving syntax that
still parses. A double-quoted run of words loses everything after its first
space. A backslash line continuation loses every line but the first. `!`
arrives as `'\!'`. None of it is reported.

Short single-line commands are fine, which is what every example in this README
passes. Anything longer belongs in a file: write the script into a directory the
guest already mounts and pass `sh /mnt/run.sh`. Write it with LF line endings —
`Set-Content` gives CRLF, and `sh` takes the carriage return as part of each
command.

**The blob mapping is what the design rests on.** Guest Vulkan puts host memory
in the guest's shm window: venus
allocates and maps a `VkDeviceMemory` on the host, and the hypervisor aliases
that pointer into guest physical address space. On macOS that is
`hv_vm_map`; here it is `WHvMapGpaRange`, which Microsoft documents as taking
memory in the *calling process's* address space — memory a GPU driver mapped
need not qualify, and host-visible VRAM behind the PCI BAR least of all.

A standalone probe settled it before any of this was built: a WHP partition, a
Vulkan allocation, `WHvMapGpaRange` over the pointer, and a real mode vCPU that
reads two marker bytes back out over an I/O port and writes one of its own —
because `S_OK` from the mapping call says nothing about what the guest sees at
that GPA. On an RTX 5090 every case passed:

| | |
|---|---|
| `VirtualAlloc`, as a control | pass |
| `vkMapMemory`, host-visible system RAM | pass |
| `vkMapMemory`, `DEVICE_LOCAL` — ReBAR | pass |
| `VK_EXT_external_memory_host`, imported host pointer | pass |
| `VK_KHR_external_memory_win32`, exported section | pass |

Two of those are worth keeping in mind. ReBAR memory — 32 GiB of host-visible
`DEVICE_LOCAL` on this card — is coherent through EPT, so blob resources need
not be confined to system RAM. And an `OPAQUE_WIN32` export is a real section
object that `MapViewOfFile` accepts, which macOS could not offer: the
descriptor route stays open here, and `export_blob` need not stay
`Unsupported` forever.

Getting the guest packages in took three of the patches below. Every `.so.N`
symlink failed to extract, `stat` on a freshly renamed file answered ENOENT,
and a download into a slow consumer — `apk` unpacking a 190 MiB library while
still fetching it — stopped dead about a third of the way in.

The guest kernel needs nothing: `config-libkrunfw-windows_x86_64` already sets
`CONFIG_DRM_VIRTIO_GPU=y` and `CONFIG_DRM_VIRTIO_GPU_KMS=y`.

### virglrenderer on Windows

```powershell
.\scripts\build-virglrenderer-windows.ps1
```

produces `data/virglrenderer/bin/virglrenderer-1.dll`, linked against
`vulkan-1.dll` and `epoxy-0.dll`, exporting the 48 `virgl_renderer_*` entry
points — `virgl_renderer_resource_get_map_ptr` among them, which is the one the
blob mapping above is built on.

Four things decided the shape of that script, and none of them are obvious from
the symptoms.

**clang-cl, not cl.** This is mesa-derived C, written against GCC builtins,
function attributes and VLAs. Configure under `cl` answers NO to every
`cc.has_function` and every `cc.has_function_attribute` in the list, and sets
`__STDC_NO_VLA__`. clang-cl keeps the MSVC ABI, so the DLL still links with the
msvc Rust toolchain, and the porting work drops to what is genuinely missing
rather than what GCC merely spells differently.

**libepoxy is vendored, because vrend is still built.** At 1.3.0 vrend had no
guard and even a venus-only build compiled the GL renderer and linked epoxy.
Upstream has since grown `-Dvrend=false`, but the patch was ported with vrend in
and has not been tried without it. `3rdparty/libepoxy` is pinned to 1.5.10 and
built first; Windows gets the WGL half only, which is why no GL winsys backend
ends up in virglrenderer.

**One patch, with the in-process venus context in it.** Both of upstream's
render server forms — a process, or a thread of this one — pass descriptors
over a unix socket, so neither is an option here. The patch adds a `none`
choice to `render-server-mode` that builds `src/venus_context.c` instead:
Sergio Lopez's in-process venus context, the same work krunkit's fork carries.
macOS used to take it too and no longer does. It is not Windows-specific: a
`render-server-mode=none` build on macOS compiles, and serves a context, a blob
and a fence to a C harness. A VM on it panics in rutabaga's Darwin backend on a
`-1` descriptor, which was not chased, since nothing ships that combination. The
blob-as-host-pointer path the context pairs with on Windows is `_WIN32`-only and
has not been compiled on the new base.

**`meson` runs from a venv under `data/`.** It needs `pyyaml` for gallium's
codegen, which is this build's business and not the system Python's. Two traps
follow from that: meson resolves `python3` off `PATH` — a native-file
`[binaries] python3` entry does *not* steer the python module — and a Windows
venv never creates that name, so the script makes one.

### virglrenderer — `3rdparty/_patches/virglrenderer-8167744-windows.patch`

**Rebased onto `8167744` without a Windows host to build it on**: it applies,
and its non-Windows half builds on macOS in both `render-server-mode=none` and
`thread`, but the `_WIN32` code has not been compiled against the new base.
Expect the first Windows build to need fixes, most likely in files upstream
added since 1.3.0 (`virgl_iov.c`, `vkr_host_copy.c`, `vkr_descriptor_heap.c`).

32 files. Most of it is the POSIX surface venus and vrend assume and Windows
does not have — `unistd.h`, `sys/mman.h`, `dlfcn.h`, `sys/resource.h`,
`poll.h`, `close`, `lseek`, `getpagesize`, `strtok_r`, `clock_gettime`,
`nanosleep` — gathered behind `src/win32_compat.h` and `.c` rather than
scattered. `src/mman_win32.c` was already in the tree and already added to the
Windows build; it just had no `getpagesize` and nothing included its header.

Five are worth calling out, because they are wrong answers rather than missing
ones:

- **`-imacros config.h` is not a flag an MSVC-style driver has**, and neither
  `cl` nor `clang-cl` errors on it — it is warned about, ignored, and then
  `config.h` is taken for a source file. The build dies much later, in
  `u_endian.h`, with both `UTIL_ARCH_*_ENDIAN` unset. `/FI` is the equivalent.
- **`VIRGL_EXPORT` was `__attribute__((visibility("default")))`**, which means
  nothing to the MSVC ABI. The library built, linked and installed while
  exporting not one symbol, and the DLL depended on neither Vulkan nor epoxy,
  because with nothing exported there was nothing to keep. It is now
  `__declspec(dllexport)` behind a `VIRGL_RENDERER_BUILD` the build defines.
- **A host-visible blob is handed over as a mapped host pointer**, not an
  exportable descriptor, which is what the probe above says Windows should do.
  That path — `vkMapMemory` in `export_blob`, a `map_ptr` in the blob and the
  resource, `virgl_renderer_resource_get_map_ptr()` and
  `VIRGL_RENDERER_BLOB_FD_TYPE_APPLE` (the name is the in-process context's
  history; the value is what rutabaga matches) — is guarded by `_WIN32`. It used
  to be shared with macOS, which now exports shm instead. `nanosleep` in place
  of `clock_nanosleep` is the one guard still shared: `defined(__APPLE__) ||
  defined(_WIN32)`.
- **`thrd_current()` is disabled in mesa's C11 threads shim for Windows**, with
  a comment saying why: `GetCurrentThread()` returns a pseudo-handle that
  cannot be compared with the handle `thrd_create()` gave out. `vkr_ring_stop`
  asks "am I the ring thread?" that way, so on Windows it compares thread ids
  instead.
- **`struct blit_prog_key` stopped fitting in the `uint64_t` it is hashed as.**
  The MSVC ABI starts a new allocation unit whenever a bitfield's declared type
  differs from its predecessor's, and that struct alternates `bool`, `enum` and
  `uint8_t`: five bytes under the Itanium ABI, fifteen here. The fields keep
  their order and widths and are now all spelled `uint32_t`.

And one that is a plain bug rather than a portability gap, which only a host
without dmabufs reaches. `virgl_renderer_resource_map` checks whether the
context has a `resource_map` hook and, finding none, tries to export a
transient dmabuf instead — then falls into a `switch` arm that calls the hook
it just found missing:

```c
if (!ctx->resource_map) {
   /* Create a transient dmabuf. */
   export_fd_type = virgl_resource_export_fd(res, &fd);
}
...
case VIRGL_RESOURCE_OPAQUE_HANDLE:
   map = ctx->resource_map(ctx, res, NULL, PROT_WRITE | PROT_READ, MAP_SHARED);
```

When the export cannot be made either — always, here — `export_fd_type` stays
`OPAQUE_HANDLE` and the null call takes the process with it. macOS never sees
this because it can still hand back an shm descriptor. Leaving `map` NULL
instead turns it into the `-EINVAL` the function already returns below, and
rutabaga falls back to `virgl_renderer_resource_get_map_ptr()`, which is where
the mapping was always going to come from.

`src/drm/drm-uapi/drm.h` gained a `_WIN32` branch for the same reason its
`#else` says "one of the BSDs": it reaches for `sys/ioccom.h` to spell ioctl
numbers. Nothing here issues a DRM ioctl — the header is pulled in for the
capset enums in `virtgpu_drm.h` — so the branch supplies the typedefs and lets
the `DRM_IOCTL_*` definitions evaluate to zero.

## `/abin`

Every guest gets one directory of cortex's own executables at `/abin`, **first on a
command's `PATH`**. Today that is `index` and `mem` -- the programs in
[`cortex-abins/`](cortex-abins), which are written in the sibling `cortex` checkout and built
here for the machine that runs them.

`docread` is in that workspace and not in `/abin`. It links pdfium into every build, pdfium is
C++, and the musl sysroot this repository fetches is libc alone, so there is no toolchain here
that can cross-compile it for the guest. [`scripts/build-abin.sh`](scripts/build-abin.sh) says
what would lift that.

```sh
./scripts/fetch-musl-sysroot.sh   # once
./scripts/build-abin.sh           # -> $CORTEX_KRUN_HOME/abin, or ~/.cache/cortex-krun/abin
```

Everything this host keeps is under that one root -- `$CORTEX_KRUN_HOME`, or `cortex-krun`
under the user's cache directory (`XDG_CACHE_HOME`, `%LOCALAPPDATA%` on Windows, `~/.cache`
otherwise): `abin/` here, `images/` that every tool in this repository pulls into, and
`session/{pid}/` while a server is up. One rule, in `cortex_krun::home`, because a host has
one answer to where its things go. `CORTEX_IMAGE_CACHE_ROOT` still moves the images out from
under it on their own, being the narrower of the two.

What each program is travels with it: `cortex-abins/{program}/SKILLS.md` is installed at
`/abin/skills/{program}/SKILLS.md`, so a session gets the programs and the reading of them from
the one mount. Under `skills/` rather than beside the executables, because the top of `/abin`
is what goes on `PATH` -- a file there that is not a program is something a shell would offer
to run. A program whose crate carries no `SKILLS.md` has none installed.

What is in one is the program's own `--help`, copied out: the front matter's `name` and
`description` are the usage line and the sentence clap opens with, and the body is that output,
subcommand by subcommand where there are subcommands. The content is settled on purpose -- a
program that answers `--help` has already written down what it is, and a second description
kept beside it is one that drifts from the parser it describes -- but **the copying is by hand
today**. Nothing checks that a `SKILLS.md` still says what its binary says, so a clap string
that moves is a file to regenerate. Making that mechanical is work not done.

The server looks for that one directory and nothing names another: where it keeps things is
already what `home()` answers, so a second way to point somewhere else would be two answers to
one question. **A host with no such directory is a session with no `/abin`**, not a session
that fails -- these are executables a session is better off with and can run without, and what
its absence costs is a `PATH` that is only the image's.

It is shared read-only, which here means read-only *at the device*: the host's virtiofs server
refuses the write whatever the guest mounted it with, so a guest that is root inside itself
cannot remount it away. Prepended to `PATH` rather than appended, because `/abin` is what this
backend undertakes to provide -- an image that happens to carry a program of the same name
would make that undertaking untrue, and nothing would say which one a session got. The path is
reserved for the same reason `/.cortex` is: a client that asks for a tree there is refused.

## Layout

```
cortex-krun-lib/            the machinery: images, disks, sessions (lib `cortex_krun`)
cortex-krun/                the console server a user runs; spawns the VM, is not one
cortex-krun-host/           the process that becomes a VM, and -- as its library half --
                            the command line the server starts one with; signed on macOS
cortex-krun-guest/          the half inside the VM; its own workspace, built for musl
cortex-abins/               the programs in the guest's /abin -- index and mem, with docread
                            beside them and not on it -- and a SKILLS.md each, saying what one
                            is; copied from cortex/cortex-execs/; its own workspace, built for
                            musl
3rdparty/libkrun/           submodule: libkrun, pinned
3rdparty/rutabaga_gfx/      submodule: rutabaga_gfx, pinned (via [patch.crates-io])
3rdparty/virglrenderer/     submodule: upstream virglrenderer, pinned to main at 8167744
3rdparty/libepoxy/          submodule: pinned to 1.5.10; Windows builds only
3rdparty/_patches/           applied to the two submodules above by scripts/apply-patches.sh;
                            see "Sessions", "Mounts", "The gpu feature" and "Windows (WHP)"
scripts/                    build and provisioning helpers, plus the cargo runner and linker,
                            the musl cc/ar that cortex-abins/ cross-compiles with, and
                            build-abin.sh, which installs the result -- programs and skills
                            both -- where a session finds it
data/                       untracked: musl sysroot, sessions, build outputs
```

`Cargo.lock` is not tracked. `data/` holds everything reproducible from the
scripts.

`cargo build --workspace` is what puts all three binaries in one directory:
`cortex-krun` finds the other two by looking beside itself, and a `-p cortex-krun`
builds only the guest -- which is its build script's -- and not the boot. On macOS
the boot needs the hypervisor entitlement, and `scripts/macos-link.sh` -- the cargo
linker for `aarch64-apple-darwin` -- signs it as it is linked, so a plain build is
enough. `scripts/macos-run.sh` signs whatever `cargo run` was asked for, which is
what the examples that create a VM themselves need.
