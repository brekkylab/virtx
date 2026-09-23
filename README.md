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
| `meson`, `ninja` | Only for `scripts/build-virglrenderer.sh` (the `gpu` feature). |

## Setup

```sh
git submodule update --init
./scripts/apply-patches.sh              # patches the libkrun and rutabaga_gfx submodules
./scripts/fetch-alpine-rootfs.sh        # guest rootfs -> data/rootfs/alpine
./scripts/build-virglrenderer.sh        # only for the `gpu` feature
```

## Running

```sh
cargo run --example macos_hello                      # boots a microVM, runs a command in it
cargo run --example run -- alpine:3.21 'id'          # pulls an OCI image and runs in it
cargo run --example macos_gpu -- 'uname -a'          # adds TSI networking
cargo run --features gpu --example macos_gpu         # vulkaninfo over venus
./scripts/run-ncnn-squeezenet.sh                     # a CNN on the guest GPU
```

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

`run` is the one example not gated by host — `session` attaches the guest's
console itself, which is the only call that differs between them. The `hello`
and `gpu` examples still come in `macos_` and `win_` pairs, and the Windows
setup is different throughout; see [Windows (WHP)](#windows-whp).

`cargo run` goes through `scripts/macos-run.sh`, registered as the cargo runner
for `aarch64-apple-darwin`. It does two things that are mandatory on macOS:

1. Ad-hoc codesigns the binary with `com.apple.security.hypervisor`. No
   developer certificate is needed, but Hypervisor.framework refuses to start
   without the entitlement.
2. Puts Homebrew's `lib` on `DYLD_LIBRARY_PATH`, because libkrun `dlopen`s
   `libkrunfw.5.dylib` by leaf name and `/opt/homebrew/lib` is not on dyld's
   default search path.

Running the built binary directly, without the runner, will fail on both counts.

## Sessions — `patches/libkrun-85bed71-overlay-root.patch`

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

## Mounts — `patches/libkrun-85bed71-session-mounts.patch`

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

The examples below do the same thing by hand, over a plain rootfs rather than an
image. Working end to end:

```
$ cargo run --features gpu --example macos_gpu
GPU0:
        deviceName = Virtio-GPU Venus (Apple M3 Pro)
        driverID   = DRIVER_ID_MESA_VENUS
        driverInfo = Mesa 26.1.6
```

Install the guest side once, which needs networking and so the TSI vsock that
`macos_gpu` already sets up:

```sh
cargo run --example macos_gpu -- 'apk add --no-cache mesa-vulkan-virtio vulkan-loader vulkan-tools'
```

`vulkaninfo` only enumerates. `scripts/run-vk-smoke.sh` compiles
`guest/vk_smoke.c` inside the VM and runs a real round-trip -- create a device,
allocate host-visible memory, submit `vkCmdFillBuffer`, wait on a fence, read
the result back:

```
instance      : 1.4.347
device        : Virtio-GPU Venus (Apple M3 Pro)
device api    : 1.2.0
queue family  : 0
memory type   : 1 (16384 bytes)
fill 4096 words : OK
```

### Why the guest device reports Vulkan 1.2

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
the 1.2 cap above is irrelevant to it. The guest is ready — Mesa venus passes
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

### virglrenderer — `patches/virglrenderer-1.3.0-macos-venus.patch`

- **rutabaga_gfx hardcodes a virglrenderer >= 1.3.0 check**, so the 0.10.4-based
  fork that krunkit uses is not an option for libkrun 2.0.
- **Upstream 1.3.0 forces the out-of-process render server on whenever venus is
  enabled** (`with_render_server = with_venus`, no override), and that server is
  built against epoll and seccomp. The patch ports Sergio Lopez's in-process
  venus context onto 1.3.0 and restores the `render-server` option. It also
  brings `VIRGL_RENDERER_BLOB_FD_TYPE_APPLE` and
  `virgl_renderer_resource_get_map_ptr()`: MoltenVK cannot export
  `VkDeviceMemory` as a descriptor, so a blob is handed over as a host pointer.
- **`vulkan-dload` defaults to true**, which resolves Vulkan with `dlopen`
  instead of linking it. The in-process venus context calls `vkMapMemory`
  directly, so the build needs `-Dvulkan-dload=false` and a linked MoltenVK.

`scripts/build-virglrenderer.sh` handles all of it. It exports the pinned
submodule tree with `git archive` and patches the copy under `data/`, so the
`virglrenderer` submodule stays pristine. It also synthesises a `vulkan.pc`
pointing at MoltenVK, which ships none, and installs it next to the built
library so consumers need a single `PKG_CONFIG_PATH` entry. That entry is set in
`.cargo/config.toml` without `force`, so an existing `PKG_CONFIG_PATH` in your
environment wins — if you set one, add `data/virglrenderer/lib/pkgconfig` to it.

### rutabaga_gfx — `patches/rutabaga_gfx-8cb133e-virgl-renderer.patch`

Published rutabaga_gfx (0.1.85) routes macOS to a stub OS backend that panics as
soon as anything asks for a descriptor. A Darwin backend built on Mach ports
landed upstream on 2026-09-02 but has not been released, so `[patch.crates-io]`
points at the submodule instead.

That backend widened `RawDescriptor` to `i64`, and `virgl_renderer.rs` was not
updated with it — it is behind an optional feature that the new macOS CI does
not build. The patch fixes those three type errors, stops `export_blob` from
wrapping the `-1` the Apple path returns, and makes `map()` fall back to
`virgl_renderer_resource_get_map_ptr()`, since the in-process venus context has
no `resource_map` hook and the transient-dmabuf fallback is Linux-only.

The same file needs the same kind of work for Windows, and it is in this patch
rather than a second one: two patches on one file break `apply-patches.sh`'s
"already patched" check, because the first one's context stops matching once
the second is in. `virgl_renderer.rs` opens a DRM render node, folds stderr
into stdout with `dup2`, and turns exported `int` fds into descriptors — none
of which exist on Windows, and all of which are now `cfg`-gated. `export_blob`
reports `Unsupported` there for the same reason it does on macOS: a
`RawDescriptor` is a `HANDLE`, which does not fit in the `int` virglrenderer
exports blobs through, so the mapping has to come from `map_ptr`.

### libkrun — `patches/libkrun-85bed71-gpu.patch`

One patch for both hosts, because they share the files. The macOS half:

- `resource_map_blob` on macOS called `export_blob` purely as a validity gate
  and required a dmabuf handle, which no macOS blob can ever have. The mapping
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

## Inference on the guest GPU

`scripts/run-ncnn-squeezenet.sh` runs SqueezeNet v1.1 on the guest GPU with
[ncnn](https://github.com/Tencent/ncnn)'s Vulkan backend, and checks the result
against ncnn's CPU backend in the same process:

```
$ ./scripts/run-ncnn-squeezenet.sh
[0 Virtio-GPU Venus (Apple M3 Pro)]  fp16-p/s/u/a=1/1/1/1  int8-p/s/u/a=1/1/1/1  bf16-p/s=1/0
[0 Virtio-GPU Venus (Apple M3 Pro)]  fp16-cm=0  int8-cm=0  bf16-cm=0  fp8-cm=0
device        : Virtio-GPU Venus (Apple M3 Pro)
device api    : 1.2.0
image         : /mnt/inference/assets/dog.jpg (1546x1213)
model         : squeezenet_v1.1 (1000 classes)
cpu           : 1.6 ms/iter
vulkan        : 3.7 ms/iter

top-5 on vulkan:
   82.57%  n02111889 Samoyed, Samoyede
    5.28%  n02112018 Pomeranian
    ...

cpu/vulkan top-1 : match
max score diff   : 7.32e-03
```

ncnn's own probe lines restate what the sections above worked out: fp16 all the
way through, no cooperative matrix, bf16 storage but no bf16 arithmetic.

### Why ncnn

ncnn's floor is Vulkan 1.0 plus `VK_KHR_get_physical_device_properties2`;
everything past that — fp16, int8, cooperative matrix — is probed at runtime
and has a fallback path. That is the only requirement the 1.2 ceiling above
leaves comfortable room under. The alternatives do not fit as well:

- Alpine's `onnxruntime` is built with no GPU execution provider, and upstream
  ONNX Runtime has no Vulkan one at all.
- `llama.cpp-vulkan` is packaged for aarch64 and would run here (it needs
  instance 1.2 and treats maintenance4, coopmat and bf16 as optional, and never
  touches `KHR_synchronization2`), but it only runs GGUF language models.
- wonnx and Burn/wgpu are the Rust-native options; wonnx has been dormant since
  2023 with thin operator coverage, and wgpu brings its own feature demands.

ONNX models go in through ncnn's `onnx2ncnn`/`pnnx` converters on the host.

### The build

ncnn is compiled inside the VM, so `macos_inference` differs from `macos_gpu`
in two ways: up to 8 vcpus and 6 GiB of RAM (`KRUN_VCPUS`, `KRUN_RAM_MIB`), and a
second virtiofs share, `data/inference/` mounted at `/mnt/inference`, holding
the checkout, the build trees and the model. The first run takes around 13
minutes; after that the script only recompiles `guest/ncnn_squeezenet.cpp`.

The build uses Alpine's glslang rather than ncnn's submodule, which is what
keeps the clone shallow. That needs `spirv-tools-dev` as well as `glslang-dev`:
`glslang-config.cmake` does a `find_dependency(SPIRV-Tools-opt)`, and without
it `find_package(glslang)` fails quietly, `NCNN_SYSTEM_GLSLANG` switches itself
off, and the build dies asking for the submodule. `cmake` also needs
`-DCMAKE_POLICY_VERSION_MINIMUM=3.5`, since ncnn still declares a 2.8.12 floor
that cmake 4 rejects.

### On the timings

The GPU is slower than the CPU here, and that is the expected result.
SqueezeNet v1.1 is 75 layers over a 227x227 input — very little arithmetic per
dispatch, and every dispatch crosses the guest/host boundary into virglrenderer
and then MoltenVK. At this size the per-dispatch overhead is the measurement.
This checks that the stack computes the right answer, not that it is fast.

The right answer is the part worth trusting: ncnn falls back to its CPU
implementation per layer, silently, for any layer whose Vulkan path is missing,
so a passing run needs more than a plausible number. The reported max score
difference is 7.32e-03 and not zero, which is fp16 on the GPU against fp32 on
the CPU — a silent fallback would agree exactly.

## Windows (WHP)

An x86_64 Windows host boots a microVM over the Windows Hypervisor Platform:

```
> cargo run --example win_hello
hello from Linux 6.12.109 x86_64
```

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
`BorrowedHandle` where it takes `BorrowedFd` elsewhere, so
[`cortex-krun-lib/examples/win_hello.rs`](cortex-krun-lib/examples/win_hello.rs) differs from
[`cortex-krun-lib/examples/macos_hello.rs`](cortex-krun-lib/examples/macos_hello.rs) by little more than `AsFd` →
`AsHandle`.

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
- **Docker**, in Linux-container mode, to build `libkrunfw.dll`.
- **WSL**, to unpack the rootfs. Both reasons are below.

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
.\scripts\fetch-alpine-rootfs.ps1       # guest rootfs -> data/rootfs/alpine
.\scripts\build-virglrenderer-windows.ps1   # only for the `gpu` feature
cargo run --example win_hello
cargo run --features gpu --example win_gpu   # vulkaninfo over venus
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

### The rootfs needs NTFS to carry things NTFS has no idea about

[`scripts/fetch-alpine-rootfs.ps1`](scripts/fetch-alpine-rootfs.ps1) replaces the
POSIX script on Windows. Unpacking the tarball is the easy part; two properties
of a Linux rootfs have no native NTFS equivalent, and libkrun's Windows
passthrough reads each from a specific place.

**Symlinks** come from `IO_REPARSE_TAG_LX_SYMLINK` reparse points — the WSL
representation, which stores the target verbatim as a Linux path. A native
Windows symlink cannot hold `/bin/busybox`: Windows resolves it against the
current drive, and creating one needs administrator rights or Developer Mode
besides. So the tarball is unpacked from inside WSL, whose drvfs creates LX
symlinks, and no elevation is needed.

**Mode bits** have no NTFS equivalent at all. `file_info_to_stat64` reports
0o644 for every regular file and 0o755 for every directory, and only an
alternate data stream named `user.containers.override_stat` — holding
`uid:gid:0mode` — overrides that. Without it `/bin/busybox` is not executable
and init dies with `EACCES`. The modes are read back out of the tarball, not off
disk, because drvfs synthesises a mode for every file it shows; only entries
whose mode differs from what the passthrough assumes get a stream, which is 27
of them for Alpine 3.24.2.

One more thing the passthrough cannot absorb: the rootfs path it is handed goes
straight to `NtCreateFile`, which — unlike Win32 — has no working directory and
no notion of a forward slash. A relative root aborts the VM and
`data/rootfs/alpine` is `ERROR_INVALID_NAME`, so `win_hello` puts the path
through `std::path::absolute` first. Not `canonicalize`: that returns a `\\?\`
path, which `NtCreateFile` rejects in turn.

### libkrun — `patches/libkrun-85bed71-windows-virtiofs.patch`

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
  representation `read_lx_symlink_by_handle` already read and
  `fetch-alpine-rootfs.ps1` already writes. `write_override_stat` grew a
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

### libkrun — `patches/libkrun-85bed71-windows-vsock.patch`

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

### Guest Vulkan

```
> cargo run --features gpu --example win_gpu
GPU0:
        apiVersion   = 1.2.0
        deviceName   = Virtio-GPU Venus (NVIDIA GeForce RTX 5090)
        driverID     = DRIVER_ID_MESA_VENUS
        driverInfo   = Mesa 26.1.6
```

The guest reaches the host GPU the same way it does on macOS — Mesa's venus
driver over virtio-gpu, served by virglrenderer — except that virglrenderer
talks to the native Vulkan driver here rather than to MoltenVK. The 1.2 cap has
the same cause as on macOS, described above: no sync-fd import, so no
`KHR_synchronization2`.

Setup is [virglrenderer on Windows](#virglrenderer-on-windows), then the guest
side once:

```powershell
cargo run --example win_gpu -- 'apk add --no-cache mesa-vulkan-virtio vulkan-loader vulkan-tools'
```

`vulkaninfo` only enumerates. `scripts/run-vk-smoke.ps1` compiles
`guest/vk_smoke.c` inside the VM and runs the same round-trip the POSIX script
does — create a device, allocate host-visible memory, submit `vkCmdFillBuffer`,
wait on a fence, read the result back — which is what puts the blob mapping
above under load:

```
instance      : 1.4.347
device        : Virtio-GPU Venus (NVIDIA GeForce RTX 5090)
device api    : 1.2.0
queue family  : 0
memory type   : 2 (16384 bytes)
fill 4096 words : OK
```

`scripts/run-ncnn-squeezenet.ps1` is the counterpart of the POSIX script
described under [Inference on the guest GPU](#inference-on-the-guest-gpu), over
[`cortex-krun-lib/examples/win_inference.rs`](cortex-krun-lib/examples/win_inference.rs):

```
[0 Virtio-GPU Venus (NVIDIA GeForce RTX 5090)]  fp16-p/s/u/a=1/1/1/1  int8-p/s/u/a=1/1/1/1  bf16-p/s=1/1
[0 Virtio-GPU Venus (NVIDIA GeForce RTX 5090)]  fp16-cm=16x16x16/16x8x16/16x8x8  int8-cm=16x16x32/16x8x32  bf16-cm=16x16x16  fp8-cm=16x16x32
device        : Virtio-GPU Venus (NVIDIA GeForce RTX 5090)
device api    : 1.2.0
cpu           : 3.2 ms/iter
vulkan        : 2.2 ms/iter

top-5 on vulkan:
   82.47%  n02111889 Samoyed, Samoyede
    5.36%  n02112018 Pomeranian
    ...

cpu/vulkan top-1 : match
max score diff   : 2.80e-03
```

### The same stack, and the answers the macOS sections could not give

ncnn's probe lines are the two hosts' capabilities side by side, from the same
Mesa and the same venus:

| | macOS / MoltenVK | Windows / NVIDIA |
|---|---|---|
| bf16 storage / arithmetic | `1/0` | `1/1` |
| cooperative matrix | `fp16-cm=0 int8-cm=0 bf16-cm=0 fp8-cm=0` | `16x16x16` / `16x16x32` / `16x16x16` / `16x16x32` |
| SqueezeNet, cpu vs vulkan | 1.6 vs 3.7 ms | 3.2 vs 2.2 ms |

So [bfloat16 is not available, and the version is not why](#bfloat16-is-not-available-and-the-version-is-not-why)
was right about the cause and can now be said plainly: the missing piece was
MoltenVK, not venus and not the 1.2 cap, which is still 1.2 here for the
unrelated sync-fd reason. Cooperative matrix arrives with it.

The GPU also stops being the slower of the two. [On the
timings](#on-the-timings) puts that down to per-dispatch overhead dominating a
75-layer, 227x227 model rather than to the GPU — same model, same code, and
here it comes out ahead.

What has not changed is what makes the run worth trusting: ncnn falls back to
its CPU implementation per layer, silently, for any layer whose Vulkan path is
missing, so the score difference being 2.80e-03 rather than zero is the
evidence that the work really happened on the GPU, in fp16.

### A guest command is an argument, and arguments do not survive the trip

`cargo run --example win_gpu -- '<script>'` reaches the binary through
`powershell -File scripts/windows-run.ps1`, and PowerShell's `-File` argument
parsing does not carry a multi-line shell script intact. Indented lines vanish
outright — an `if` body or a function body comes out empty, leaving syntax that
still parses. A double-quoted run of words loses everything after its first
space. A backslash line continuation loses every line but the first. `!`
arrives as `'\!'`. None of it is reported.

Short single-line commands are fine, which is what every example in this README
passes. Anything longer belongs in a file: `run-ncnn-squeezenet.ps1` writes its
script into the share the guest already mounts and passes
`sh /mnt/inference/run.sh`. Write it with LF line endings — `Set-Content` gives
CRLF, and `sh` takes the carriage return as part of each command.

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

[`cortex-krun-lib/examples/win_gpu.rs`](cortex-krun-lib/examples/win_gpu.rs) is `win_hello` plus a TSI vsock,
and — under `--features gpu` — the virtio-gpu device, the same division of
labour `macos_gpu` has.

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

**libepoxy is not optional, so it is vendored.** `src/meson.build` does
`virgl_sources += vrend_sources` with no guard and lists `epoxy_dep` in that
target's dependencies, so even a venus-only build compiles the GL renderer and
links epoxy. Upstream has no switch for it. `3rdparty/libepoxy` is pinned to
1.5.10 and built first; Windows gets the WGL half only, which is why no GL
winsys backend ends up in virglrenderer.

**Both patches, in order.** `virglrenderer-1.3.0-macos-venus.patch` carries the
in-process venus context, which any host without the out-of-process render
server needs — that server is `fork`, `epoll` and seccomp — so Windows needs it
too, name notwithstanding. `virglrenderer-1.3.0-windows.patch` is cut against a
tree that already has it.

**`meson` runs from a venv under `data/`.** It needs `pyyaml` for gallium's
codegen, which is this build's business and not the system Python's. Two traps
follow from that: meson resolves `python3` off `PATH` — a native-file
`[binaries] python3` entry does *not* steer the python module — and a Windows
venv never creates that name, so the script makes one.

### virglrenderer — `patches/virglrenderer-1.3.0-windows.patch`

27 files. Most of it is the POSIX surface venus and vrend assume and Windows
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
- **The `__APPLE__` guards the macOS patch introduced are about blobs, not
  about Apple.** Twelve of the thirteen mark the path where a host-visible blob
  is handed over as a mapped host pointer instead of an exportable descriptor,
  which is exactly what the probe above says Windows should do, so they now
  read `defined(__APPLE__) || defined(_WIN32)`. The thirteenth is `nanosleep`
  versus `clock_nanosleep` and stays as it was.
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

## Layout

```
cortex-krun-lib/            the machinery: images, disks, sessions (lib `cortex_krun`)
cortex-krun/                the console server a user runs; spawns the VM, is not one
cortex-krun-host/           the process that becomes a VM, and -- as its library half --
                            the command line the server starts one with; signed on macOS
cortex-krun-guest/          the agent inside the VM; its own workspace, built for musl
guest/                      C sources compiled and run inside the VM by the examples
3rdparty/libkrun/           submodule: libkrun, pinned
3rdparty/rutabaga_gfx/      submodule: rutabaga_gfx, pinned (via [patch.crates-io])
3rdparty/virglrenderer/     submodule: upstream virglrenderer, pinned to 1.3.0
3rdparty/libepoxy/          submodule: pinned to 1.5.10; Windows builds only
patches/                    see "Sessions", "Mounts", "The gpu feature" and "Windows (WHP)"
scripts/                    build and provisioning helpers, plus the cargo runner
data/                       untracked: rootfs, sessions, models, build outputs
```

`Cargo.lock` is not tracked. `data/` holds everything reproducible from the
scripts.

`cargo build --workspace` is what puts all three binaries in one directory:
`cortex-krun` finds the other two by looking beside itself, and a `-p cortex-krun`
builds only the guest -- which is its build script's -- and not the boot. On macOS
the boot needs the hypervisor entitlement, which `scripts/macos-run.sh` applies to
it as well as to whatever `cargo run` was asked for.
