# virtx-uvm

The server's machinery, on [libkrun](https://github.com/libkrun/libkrun) 2.0: images and the
disks made of them, machines booted from those disks, and the snapshots and volumes they
write to. [`virtx-server-stdio`](../virtx-server-stdio/) and
[`virtx-server-tcp`](../virtx-server-tcp/) serve it on `virtx-core`'s protocol, over stdio
and over HTTP; a client reaches a machine through one of them, never through this crate.

libkrun is linked as an rlib, so there is no `libkrun.so`/`libkrun.dylib` to ship and no
C ABI in the middle — `virtx-uvm` depends on the `libkrun` crate directly.

> libkrun 2.0 is still under active development and its API is not stable. The
> dependency is pinned to a specific commit for that reason; bump it
> deliberately, not automatically.

Every upstream project changed here is a fork under brekkylab, pinned to a commit on its
`virtx` branch: the upstream commit it tracks, then one commit per change of ours. `git log`
in a fork shows them on top of the upstream commit, and the sections below describe them one
by one, each headed by the name its commit's title starts with. libkrun and rutabaga_gfx are
Rust crates, which cargo fetches as git dependencies named in `Cargo.toml`; the rest are
submodules under `3rdparty/`, built by `xtask/scripts/build-deps.sh` and `.ps1`.

## Requirements

macOS on Apple Silicon. Releases are built for Linux on x86_64 and aarch64 too, and for
x86_64 Windows over the Windows Hypervisor Platform, which has its own setup — see
[Windows (WHP)](#windows-whp). The rest of this section is macOS.

```sh
brew install lld meson ninja krunvm
```

| Dependency | Why |
|---|---|
| `krunvm` | What `xtask/scripts/build-libkrunfw.sh` builds the guest kernel in — see [The guest kernel is built, not installed](#the-guest-kernel-is-built-not-installed). The library it produces is `dlopen`ed by leaf name at runtime and never linked, so it is not a build dependency of anything here. |
| `lld` | libkrun's init blob cross-compiles the guest init to `aarch64-unknown-linux-musl` with `clang -fuse-ld=lld`. Apple's toolchain has no lld. |
| `llvm` | Only for `virtx-abins/`, and only for `llvm-ar`: Apple's `ar` writes a Mach-O symbol table over ELF objects and `rust-lld` then reads an empty archive. The `lld` formula does not carry it; `llvm` does, and carries lld too. |
| `meson`, `ninja` | Only for `xtask/scripts/build-deps.sh`, run by `cargo xtask build server-stdio` and `server-tcp`. |

## Setup

```sh
cargo xtask init                        # checks out the submodules, adds the musl target
                                        #    (--force resets the submodules first)
cargo xtask build kernel                # the guest kernel -> build/krunfw/
cargo xtask build server-stdio          # -> target/release/virtx-stdio (builds MoltenVK and
                                        #    virglrenderer first; add server-tcp too)
cargo xtask install                     # -> $VIRTX_HOME/bin, or --prefix <dir>
```

## Running

A server is started by whoever wants a sandbox: a client spawns `virtx-stdio` and speaks to it
over its stdin and stdout, or reaches a `virtx-server-tcp` over HTTP. The repository's
[README](../README.md) has a client's side; the servers' own tests are the quickest way to boot
machines from here:

```sh
cargo xtask test -p virtx-server-stdio    # spawns the built binary, as a client does
cargo xtask test -p virtx-server-tcp
```

`KRUN_LOG=debug` (also `info`, `warn`, `trace`) turns on libkrun's own logging, which a
machine's process passes on to the server with the guest's console.

`cargo xtask run` and `cargo xtask test` are `cargo run` and `cargo test` with one thing added:
they build through cargo, then sign each binary before running it. On macOS that is
mandatory: an ad-hoc codesign with `com.apple.security.hypervisor`. No developer certificate
is needed, but Hypervisor.framework refuses to start without the entitlement. A test signs the
server binary it spawns, too, since that binary spawns itself to be each machine.

A plain `cargo run` or `cargo test` -- an IDE's Run button included -- does not sign, and a
binary that creates a VM fails without it. `cargo xtask build server-stdio` and
`server-tcp` sign what they build, since a server is started by whoever wants a sandbox --
a test, an agent runtime, a CLI -- and `cargo xtask run` is not on that path: the binary that
calls `hv_vm_create` has to come out of the build already entitled. A plain `cargo build`
leaves it unsigned.

Nothing has to be on a search path: the guest kernel is built here and installed beside the
binaries, and libkrun looks there first — see [The guest kernel is built, not
installed](#the-guest-kernel-is-built-not-installed). Worth not adding one: a search path wide
enough to find the kernel was also wide enough to let the virglrenderer that krunkit's tap
installs stand in for the one `build/` had built, silently, since the GPU still came up.

## Machines — libkrun commit `overlay-root`

[`fstree::erofs`](src/fstree/erofs/) makes an image's layers into a disk: one
read-only EROFS filesystem -- the layers merged into a metadata image that points into
them, rather than a copy of them -- shared by every machine ever booted on those layers.
A [snapshot](src/snapshot.rs) is what makes one of those machines writable: an ext4 of its own,
stacked over it by [`machine`](src/machine/):

```
/dev/vdb  ext4,  writable    the snapshot, the upper layer ─┐
                                                            ├─ overlay ──► /
/dev/vda  erofs, read-only   the image's disk              ─┘
```

A sandbox made without a snapshot gets one of its own, gone with it. A sandbox's machine is
stopped once nothing has used the sandbox for a while (`VIRTX_PARK_AFTER_MS`, five minutes
unless it says otherwise, `0` for never) and booted again over the same snapshot when something
does: see [`sandbox`](src/sandbox/sandbox.rs). The devices a machine has, and why each, are in
[`machine/boot.rs`](src/machine/boot.rs).

**The stacking happens in the guest**, because overlayfs is a kernel filesystem
and the host has no way to mount one — let alone an EROFS a macOS kernel cannot
read at all. libkrun's init already knows how to mount a block device as the
root and pivot onto it (`KRUN_BLOCK_ROOT_DEVICE`), but it takes exactly one
device. The patch teaches it the two-device form: `KRUN_BLOCK_LOWER_DEVICE` and
`KRUN_BLOCK_UPPER_DEVICE`, mounted, overlaid and pivoted onto through the same
code path, in `init/init-binary/src/fs.rs`. A boot that names neither variable behaves exactly
as before.

**Two block devices and not a virtio-fs share for the upper layer**, which would
need no patch at all and does not work. The guest kernel refuses a FUSE
filesystem as an upperdir outright — `filesystem on %s not supported as
upperdir` — and even past that, deleting a file the image shipped is recorded as
a character device numbered `0:0`, which `mknod` on a macOS host refuses to
anything but root. It is the same wall [`fstree`](src/fstree/) is
written around, and the reason layers become EROFS images rather than extracted
directories.

**The patch carries a second, smaller thing**, in `init/init-binary/src/exec.rs`: the guest's
exit status. libkrun's init reports it as an ioctl on the virtiofs mount, and any boot that
pivots onto a block root — the existing `KRUN_BLOCK_ROOT_DEVICE` one as much as this — has
left that mount under no path by the time the workload exits, so the status was
dropped and the host always saw 0. The init now holds a descriptor on the
virtiofs root across the pivot and sends the ioctl to that.

## Volumes — libkrun commit `session-mounts`

A sandbox can be given volumes, each one a virtio-fs share, and one more for each remote store
mounted in the volume (see [`volume`](src/volume/)).

**The host half needs no patch.** Sharing a directory under a tag is what
libkrun has always done — `krun_add_virtiofs` in the C API, `FsDevice::new` and
`FsDevice::new_read_only` in the Rust one — and read-only is a parameter of the
device rather than something bolted on afterwards.

**The guest half is what is missing, and deliberately so.** A virtio-fs share
has no notion of where it belongs: it exports a directory under a tag, and
something in the guest has to run `mount -t virtiofs <tag> <dir>`. libkrun's init
mounts the root and one tmpfs and stops there, which leaves automounting to
whoever builds the guest's boot — and for a machine that is this crate. The patch
reads the rest of the `mounts` list the init already parses out of
`/.krun_config.json`, and mounts each entry as the config spells it. It touches
`init/init-binary/src/config.rs` and `init/init-binary/src/main.rs` and adds
`init/init-binary/src/mounts.rs`; the overlay-root patch owns `fs.rs`, so the two
do not stack on one file.

**The mounts go on after the pivot**, once the root is the overlay, in the order given, so a
store's share lands over the directory the volume's own share holds for it. One made before
the pivot would sit on the boot root the pivot leaves behind, which afterwards is under no path
at all. The directory a share lands on is created if the image shipped none, which puts it on
the upper layer.

**Read-only is the host's, not the guest's.** A read-only volume is attached read-only at the
device, and the virtiofs server then refuses every write whatever the guest mounted it with. A
guest that writes anyway gets an error from the write rather than from the mount, and
`/proc/mounts` inside the guest still says `rw` — the flag a guest could remount away is not
where the guarantee lives.

Nothing here is a sandbox of the host's filesystem by itself. libkrun's own README is explicit
that a virtio-fs share offers no protection against a guest reaching other directories on the
same host filesystem, and read-only bounds what a guest may write, not what it may read.

## The network — libkrun commit `tsi`

A machine's network is libkrun's Transparent Socket Impersonation: the guest kernel hands every
socket its own stack cannot answer to a vsock device, and libkrun opens the host socket that
answers it. There is no interface, no address and no lease — nothing to configure in the guest
and nothing to forward on the host. [`machine/net`](src/machine/net/mod.rs) has the whole of
it: where a guest's connections may go (`none`, `host` or `public`), and how a published port
reaches a listener in the guest.

**What TSI lacks is a say in where a socket goes.** Upstream opens whatever the
guest asked for, so a guest's `listen` binds the host's own port on every
interface, and its own published port is reached at the host's port rather than
the guest's listener. The patch adds `TsiPolicy`, which the vsock muxer asks before a
`connect`, a datagram's destination or a `listen` reaches any proxy, and
`VsockDevice::set_policy` to hand one over. An answer is an address, not a yes; what each
request gets is the table in [`net/policy.rs`](src/machine/net/policy.rs).
In the muxer rather than in each proxy, because the muxer is the one place every
request passes through on every host — the unix and Windows proxies below it are
two implementations of the same thing.

**And a resolver, since there is no lease to name one.** The same patch has the
init write `nameserver $KRUN_NAMESERVER` into `/etc/resolv.conf` once the root is
in place, the same moment libkrun's DHCP client writes its own. A datagram's reply is
stamped by the guest kernel with the address it was sent to, so the guest never
sees that its resolver was answered from somewhere else.

## The `gpu` feature

Guest Vulkan goes through virtio-gpu with [venus](https://docs.mesa3d.org/drivers/venus.html):
the guest runs Mesa's venus driver, virglrenderer serves it on the host, and
MoltenVK turns that into Metal.

A sandbox asks for one with `gpu`, and `gpu_memory_mib` for how much the guest's GPU may hold
(libkrun's 8 GiB otherwise). The image carries the guest half
(`apk add mesa-vulkan-virtio vulkan-loader`); one without it boots and finds no device.

**The servers do not ignore it.** `cargo xtask build` always builds them with the feature, and
a server built without it refuses a sandbox that asks for a GPU rather than running it on the
CPU: a client on the other end of a pipe cannot `cfg` around anything, and a sandbox that
silently ran without one costs it the whole run to find out. A host with no Vulkan loader still
runs a `gpu` build -- the boot opens the loader at run time -- so one build serves hosts with a
GPU and without.

[`MachineSpec::gpu`](src/machine/machine.rs) itself is ignored in a build of this crate without
the feature, so a caller of the crate never has to `cfg` around it.

### Which Vulkan version the guest device reports

With the current build and an `alpine:edge` guest (Mesa 26.2.3) the device
reports **1.4.343**, with `VK_KHR_synchronization2` on. The same guest reported
1.3.0 against krunkit's 0.10.4e (see the note on that search path under [Running](#running)), and this section
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

### bfloat16 and cooperative matrices

`VK_KHR_shader_bfloat16` is an extension that depends only on Vulkan 1.1, so
the version cap above is irrelevant to it. Upstream **MoltenVK does not expose
it**, nor `VK_KHR_cooperative_matrix`; `VK_KHR_shader_float16_int8` (fp16) is
available. Metal itself has had bfloat since MSL 3.1. Upstream SPIRV-Cross now
translates bfloat16 and cooperative matrices, element-wise operations included,
but only as a single 8x8 `simdgroup_matrix` with literal dimensions. Both are
patched in here, by a commit `cooperative-matrix-bfloat16` on each fork, on Apple7 and
later: MoltenVK reports the extensions and 8x8 and 16x16 shapes, and SPIRV-Cross emits a
matrix as `spvCoopMatrix<T, R, C>`, a grid of 8x8 tiles, keeping upstream's element-wise
emission but looping it over every tile.

**The guest needs Mesa 26.0 or later.** Venus passes neither extension through
before that: Debian trixie's 25.0 shows the device without them, and
trixie-backports' 26.1 with both.

**A cooperative matrix's dimensions can be specialization constants**, and are
in ncnn, whose `gemm_cm` declares `coopmat<…, M, N, …>` with `M`, `N`, `K` set
per pipeline. A simdgroup matrix's shape is a template argument in MSL, so the
patch declares such a constant the way SPIRV-Cross already declares one used as
an array length — as a macro — and MoltenVK compiles the variant with the
pipeline's values defined, as it does for those. Two things follow. The
library is first compiled with the constants' defaults (`1` in ncnn), so
`spvCoopMatrix` has to compile for any shape and cannot `static_assert` a
multiple of 8. And a failed conversion does not fail `vkCreateComputePipelines`
loudly enough for ncnn: it loads the net and the guest aborts at dispatch, with
the reason only on the machine process's stderr, which is the server's.

Nothing upstream supports this combination yet, so three forks carry a commit for it.
Each is small and self-contained; the reasoning is worth keeping because none of it is
obvious from the symptoms.

### virglrenderer commit `macos`

The submodule is pinned to upstream main (`e0a1caf`), not to a release. 1.3.0 —
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

Venus is only initialised with `VIRGL_RENDERER_RENDER_SERVER` set, so a
machine asks for it wherever the render server is built -- macOS and Linux, not
Windows (see `virtx-uvm/src/machine/gpu.rs`). The patch carries what
upstream main still gets wrong there:

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
name, and Homebrew's `lib` is not on its default search path. Linux does the
opposite: `-Dvulkan-dload=true`, so a server starts on a host with no
Vulkan loader, and offers a GPU only where it finds one.

### virglrenderer commit `linux-host-memory`, for Linux

A guest maps host-visible memory through the fd its `VkDeviceMemory` exports:
the host `mmap`s it into the guest's window. A dma_buf or a shm fd can be
mapped; an opaque fd cannot. NVIDIA's driver exports host-visible memory only as
opaque fds, so on it every guest `vkMapMemory` failed with
`VK_ERROR_MEMORY_MAP_FAILED` (the host answers `RESOURCE_MAP_BLOB` with
`ERR_UNSPEC`), while llvmpipe, which exports dma_bufs, worked.

The patch does on such a device what macOS does with Metal buffers: memory is
allocated as shm, imported as a host pointer (`VK_EXT_external_memory_host`)
and exported as that shm. It is used only where the device exports no dma_buf,
takes host pointers at no more than page alignment, and the guest did not ask
for exportable memory; a memory type a host pointer cannot back falls through
to the path it had. Resizable-BAR types -- device-local *and* host-visible --
are reported to the guest as device-local only, since system memory cannot back
them and nothing else the host has can be mapped. The shm is freed in
`vkr_device_remove_object`, after the memory imported from it, on both the
`vkFreeMemory` and the device-teardown path.

`xtask/scripts/build-deps.sh` handles all of it. It builds the submodule into
`build/virglrenderer-build/` and installs
to `build/virglrenderer/`. venus-protocol is a meson subproject
that is not in that tree; meson clones it at configure time, so the first build
needs the network, and its codegen needs Mako alongside the PyYAML gallium
wants. It also synthesises a `vulkan.pc`
pointing at MoltenVK, which ships none, and installs it next to the built
library so consumers need a single `PKG_CONFIG_PATH` entry. That entry is set in
`.cargo/config.toml` without `force`, so an existing `PKG_CONFIG_PATH` in your
environment wins — if you set one, add `build/virglrenderer/lib/pkgconfig` to it.

### rutabaga_gfx commit `virgl-renderer`

Published rutabaga_gfx (0.1.85) routes macOS to a stub OS backend that panics as
soon as anything asks for a descriptor. A Darwin backend built on Mach ports
landed upstream on 2026-09-02 but has not been released, so `[patch.crates-io]`
points at our fork instead.

That backend widened `RawDescriptor` to `i64`, and `virgl_renderer.rs` was not
updated with it at first; upstream fixed it in `c9bae3a`, and macOS needs
nothing more: an exported blob is an shm fd, which the Darwin backend wraps as
`OwnedDescriptor::Fd`, and `map()` gets its pointer from
`virgl_renderer_resource_map`.

The patch is Windows-only. `virgl_renderer.rs` opens a DRM render node, folds
stderr into stdout with `dup2`, and turns exported `int` fds into descriptors —
none of which exist on Windows. On Windows `export_blob` has to report `Unsupported`, since a
`RawDescriptor` is a `HANDLE` that does not fit in the `int` virglrenderer
exports blobs through, and `map()` has to fall back to
`virgl_renderer_resource_get_map_ptr()`.

### libkrun commit `gpu`

One patch for both hosts, because they share the files. The macOS half:

- `resource_map_blob` on macOS called `export_blob` purely as a validity gate
  and required a dmabuf handle, which no macOS blob has — they are shm. The mapping
  only ever consumes `map_ptr`, so the gate is dropped. This is the same
  reasoning as [libkrun#740](https://github.com/libkrun/libkrun/pull/740),
  rewritten against the current rutabaga API — that PR predates the migration to
  the upstream crate and no longer applies.
- Hypervisor.framework maps in host pages, 16 KiB on Apple Silicon, while blob
  resources are sized in 4 KiB guest pages. A 132 KiB blob was rejected outright.
  Lengths are now rounded up, symmetrically in `add_mapping` and
  `remove_mapping`. Upstream offers `VIRTIO_GPU_F_BLOB_ALIGNMENT` (`51367cf`)
  instead, but a guest only honours it from Linux 7.2, and libkrunfw is 6.12.

The Windows half is mostly the same code, reached: upstream gates the whole
blob-mapping path on `target_os = "macos"` — `map_sender`, the `WorkerMessage`
variants and their arms in `vmm/worker.rs`, the `Vmm::add_mapping` wrappers, `resource_map_blob` and
`resource_unmap_blob`, and the `AttachContext` that carries the channel — and
every one of those is about handing the hypervisor a host pointer, not about
macOS. They now read `any(target_os = "macos", target_os = "windows")`. What
had to be written rather than widened:

- **`WhpVm::unmap_memory`**, wrapping `WHvUnmapGpaRange`. The `whp` crate had
  only `map_memory`, so there was no way to replace a blob mapping.
- **`Vm::add_mapping`/`remove_mapping` on WHP**, the counterparts of the macOS
  ones. No rounding: WHP asks for 4 KiB, which is what a blob is already sized
  in, so the host-page arithmetic Apple Silicon needs has no equivalent here.
  For the same reason the `VIRTIO_GPU_F_BLOB_ALIGNMENT` upstream offers
  (`51367cf`) is 4096 there: upstream reads it from `sysconf`, which Windows
  does not have.
- **`Vmm::vm` on Windows.** Upstream has no use for the field there and drops
  it (`5be8aa18`), but the `Vmm::add_mapping` wrappers reach the WHP partition
  through it.
- **`needs_worker`** on x86_64 followed `split_irqchip` alone, so the thread the
  GPU device sends its mappings to never started. Upstream fixed the macOS arm
  of the same expression (`221a028`); this is the x86_64 one.
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

## Windows (WHP)

An x86_64 Windows host boots a microVM over the Windows Hypervisor Platform:

```powershell
cargo xtask test -p virtx-server-stdio
```

The guest kernel there is `libkrunfw.dll`'s, which is 6.12.109 on x86_64 at the
tag `xtask/scripts/build-libkrunfw.sh` pins.

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
[`machine`](src/machine/) owns that call rather than leaving it to
a caller: the two spellings differ by little more than `AsFd` → `AsHandle`, and
holding the difference in one place is what lets everything above it be written
once.

### The agent's port is two pipes

The agent inside a machine talks to the machine's process over a named virtio-console port,
and on every host the process end of that port is **two pipes**, one each way, made in
[`machine/boot.rs`](src/machine/boot.rs) and passed on over `ctrlin` and `ctrlout` (see
[`machine::wire`](src/machine/wire.rs)).

Two, and not one socket, because of Windows: libkrun's Windows console reads the port's input
handle with a blocking `ReadFile` on one thread and writes its output handle with a blocking
`WriteFile` on another, neither of them overlapped. Both handles therefore have to
be synchronous, and two handles onto one synchronous file object serialize against
each other — so a read parked waiting for the guest would hold the write that was
meant to wake it, and the protocol would deadlock on its first call. Two pipes are
two file objects. An `AF_UNIX` socket is not a handle `ReadFile` takes, and Rust's standard
library has no Windows spelling for one.

The guest's own console gets no stdin either. libkrun's Windows console parks its input
thread in a blocking `ReadFile`, and the VMM joins that thread before it exits
— so a tty stdin means the process runs, prints, and then hangs
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

- `LIBCLANG_PATH` pointing at a directory holding
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

- No Vulkan SDK is needed. `build-deps.ps1` uses one when `VULKAN_SDK` names
  it, and otherwise the Vulkan headers virglrenderer bundles, with an import
  library for `vulkan-1.dll` made from them. The host delay-loads that DLL,
  which comes with any GPU driver, so a host without one still starts and says
  it cannot make a GPU.

One host setting is worth checking before anything else: **Smart App Control**
blocks unsigned executables, which is every build script cargo compiles and
every binary it produces. With it on, `cargo build` fails with `os error 4551`
before it gets anywhere near libkrun. Turning it off is a one-way door — it
cannot be re-enabled without reinstalling Windows.

### Setup

```powershell
cargo xtask init                        # checks out the submodules, adds the musl target
cargo xtask build kernel                # guest kernel -> build/krunfw/
cargo xtask build server-stdio          # builds libepoxy and virglrenderer first
cargo xtask test -p virtx-server-stdio
```

The kernel first, because `cargo xtask build server-*` and `cargo xtask test` stop
without one -- they copy it beside the server binaries, which is how they find it. A
plain `cargo build` leaves the binary unable to boot, as it leaves it unsigned.

`cargo xtask run` adds `build/epoxy/bin/` to `PATH`, when a `gpu` build made one.
That is the only thing left there: those libraries are linked rather than opened
by name, so they are the loader's to find at start-up and live where nothing else
would look.

### The guest kernel is built, not installed

[`xtask/scripts/build-libkrunfw.sh`](../xtask/scripts/build-libkrunfw.sh) builds libkrunfw
into `build/krunfw/` on every host, and the reason is not reproducibility.

**The config libkrunfw ships for x86_64 leaves the guest with no write-combining.**
`CONFIG_MTRR` is off there, and `CONFIG_X86_PAT` is `def_bool y` with `depends on MTRR`,
so PAT goes with it: the guest comes up with PAT slots `WB WT UC- UC` and no WC among
them, and every `ioremap_wc` a driver asks for is handed back UC-. Nothing reports an
error. It is only slower — and on the path where a CPU writes into a resizable BAR, it
is slower by a factor of 36. Measured here, loading one 900 MiB model onto the GPU:

| | throughput | load |
|---|---|---|
| shipped config | 5 MB/s | 178.0s |
| `CONFIG_MTRR=y` | 180 MB/s | 5.0s |

It went out in libkrunfw's [`d28d263`][d28d263], which turns off "processor quirks and
features we don't need in a virtual machine" and saves 75 KiB of kernel image. That is
true of MTRR itself, and it takes PAT along by accident: the commit mentions neither PAT
nor write-combining, and libkrunfw's own SEV and TDX configs still carry
`CONFIG_MTRR=y`. So it is a regression to carry rather than a trade to respect, and
carrying it means building the kernel rather than taking a release or a Homebrew bottle.

aarch64 has neither register set — memory types there come from the page tables' MAIR,
and write-combining needs none of this — so the patch is x86_64's alone and an arm64
build is only about controlling what is in the guest.

[d28d263]: https://github.com/containers/libkrunfw/commit/d28d263

**Where the kernel is built** depends on what the host can offer a Linux toolchain.
Linux and Windows both build in a throwaway `ubuntu:24.04` container — Windows because
it has no such toolchain at all, Linux so that it gets the same one rather than whatever
the distribution has. macOS cannot: the output is a `.dylib` that has to be linked on
macOS while the kernel inside it has to be built on Linux, which libkrunfw resolves with
`krunvm`, and which the script defers to. `brew install krunvm` if it is missing.

Windows has a second reason to build rather than install, which is that there is nothing
to install: libkrunfw gained a Windows target in
[libkrunfw#122](https://github.com/containers/libkrunfw/pull/122) but its releases ship
no DLL, and `make OS=Windows` cross-compiles from a *Linux* host with MinGW-w64,
configuring and building a Linux kernel and then wrapping `vmlinux` in a DLL. It uses
its own kernel config there — Hyper-V enlightenments on, and the kernel bundle aligned
to 4 KiB rather than 64 KiB to match WHP's page granularity.

Expect 10–20 minutes on the first run; it is a full kernel build. `LIBKRUNFW_REBUILD=1`
builds again over what is already there.

### libkrun commit `macos-krunfw-path`

One string, so that macOS obeys the same rule as the other two: **the guest kernel sits
beside the binary that opens it.**

`cargo xtask build server-*` and `cargo xtask test` put it there, and libkrun opens it
by name at run time rather than linking it. On Windows a bare name is enough, because the loader searches the
executable's own directory before anywhere else. On Linux it is enough too, because
`dlopen` consults the *calling object's* `DT_RUNPATH` and the link sets that to `$ORIGIN`
— see `.cargo/config.toml`.

dyld does neither. A `dlopen` argument with no slash in it is looked for in
`DYLD_LIBRARY_PATH` and then `DYLD_FALLBACK_LIBRARY_PATH`
(`$HOME/lib:/usr/local/lib:/usr/lib`), and nowhere near the caller. `@rpath` and
`@loader_path` do not help: they are prefixes dyld expands inside a *path*, so a name
that is not one is never resolved against them, whatever `LC_RPATH` the binary carries.
So `KRUNFW_NAME` is `@loader_path/libkrunfw.5.dylib` there — a path, which dyld then
resolves against the directory of whatever did the `dlopen`.

What this replaces is an environment variable that whoever runs the binary had to set,
which a wrapper can do for `cargo xtask run` and nothing can do for an installed binary
someone runs by hand.

### libkrun commit `windows-virtiofs`

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
Two more, found the first time a machine was given shares rather than a bare
guest. Both stop a machine before it has a guest to report from, and neither
error says what it is about.

- **`path_to_nt_wide` passed a Win32 path to the object manager verbatim.** The
  conversion prepends `\??\` and nothing else, so a path spelled with forward
  slashes — `Path::join("examples/sam3")` makes one, and every Win32 call takes
  it — arrives at `NtCreateFile` as a name with a `/` in it. The object manager
  is not Win32: there `/` is an ordinary character, so the path names a file
  that is not there and the answer is `ERROR_INVALID_NAME`. Separators are
  normalized on the way now, which loses nothing because a `/` cannot occur in
  a Windows filename.

- **The share root could not be a reparse point**, which is what every mount
  point on Windows is. `openat` opened the root with `FILE_OPEN_REPARSE_POINT`
  and then refused anything carrying `FILE_ATTRIBUTE_REPARSE_POINT` — so a tree
  attached to the host by a userspace filesystem and then handed to a guest,
  which is how `virtx` shares one it holds in memory, was refused. The root is
  followed rather than opened as a link now, and the refusal is gone rather
  than relaxed.

  It is gone because it did not buy what its wording suggested. What it covered
  was the *root*, which the host names when it boots a machine and a guest
  never picks. What a guest can reach it never covered: `lookup` composes a
  child path by joining strings and opens it absolutely,
  `FILE_OPEN_REPARSE_POINT` applies only to the last component, and the object
  manager follows every one before it — so a junction planted under a share is
  a way out of it today, and is reported to the guest as a plain directory
  besides, since `IO_REPARSE_TAG_MOUNT_POINT` is not treated as a symlink.
  **Containment belongs in `lookup` and is not there.** Refusing the root only
  made it look as though it were.

Worth knowing while reading either: **an error raised in this file arrives
under another name.** `linux_error` takes the errno off an `io::Error` and
falls back to `libc::EIO` when there is none, which is 5 on Windows; that maps
to `LINUX_EIO`, also 5, and `io::Error::from_raw_os_error(5)` formats as
Windows' error 5. So the reparse refusal above — an `io::Error::new(Other, …)`
with a message and no errno — reached the log as `ERROR_ACCESS_DENIED`, and the
message it was carrying was dropped on the way.


### libkrun commit `windows-vsock`

Three stalls in TSI, which is the whole of a machine's network — see
[The network](#the-network--libkrun-commit-tsi).
All of them are credit mistaken for something else, and all of them are made
permanent by the same property of this epoll: it is
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

### Guest Vulkan on Windows

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
cargo xtask build server-stdio
```

runs `xtask/scripts/build-deps.ps1`, which produces `build/virglrenderer/lib/libvirglrenderer.a`, linked against
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

**`meson` runs from a venv under `build/`.** It needs `pyyaml` for gallium's
codegen, which is this build's business and not the system Python's. Two traps
follow from that: meson resolves `python3` off `PATH` — a native-file
`[binaries] python3` entry does *not* steer the python module — and a Windows
venv never creates that name, so the script makes one.

### libkrun commit `x86_64-mptable`

The twelfth virtio-mmio device on an x86_64 guest got an interrupt line nothing
had told the kernel about, and the machine stopped there without saying so.

`MMIODeviceManager` hands out GSIs from `IRQ_BASE` to `IRQ_MAX`, which
`arch/x86_64/layout.rs` sets to 5 and 23 — its comment says as much: "The I/O
APIC has 24 pins (0-23) … leaving GSIs 5-23 for virtio-mmio devices." The MP
table describing those pins was still writing the 16 legacy ISA entries
Firecracker's original `for i in 0..16` produced. A GSI with no `MP_INTSRC`
entry is one the kernel never routes, so the device holding it waits for an
interrupt that cannot arrive — and waits silently, because nothing failed.

Which device that is depends on the machine's shape. A machine with a GPU, a
network and three shares lands on:

```
fs 5 · console 6 · rng 7 · balloon 8 · vsock 9 · gpu 10 · block 11 · block 12
fs 13 · fs 14 · fs 15 · fs 16   ← the line no MP_INTSRC covers
```

The guest's boot then stops where that fs device is mounted, which is before
the command is started at all — libkrun's init mounts the shares first. What a
server saw, back when a guest agent answered it, was that agent's handshake
never arriving: "the guest connected but did not say it was there within 60s",
naming nothing that would lead here. The
`ioapic: write: ioredtbl register N` line for 16 is simply absent from a debug
log that has 5, 13, 14 and 15.

The table now carries one entry per pin, counted from `IRQ_MAX` rather than
from a literal of its own, so the two cannot drift apart again. That is eleven
virtio-mmio devices before the fix and nineteen after — with the nine a machine
always has, two user mounts and ten.

**Not a Windows bug**, although this is where it was found: the MP table and
the IRQ range are both generic x86_64, so a Linux host on x86_64 has the same
ceiling. macOS is aarch64, where the interrupt controller is a GIC and
`IRQ_BASE`/`IRQ_MAX` are 32 and 159, and never reaches it.

### virglrenderer commit `windows`

Built on a Windows host at `8167744` with no further fixes, and carried to
`e0a1caf` unchanged: `xtask/scripts/build-deps.ps1` compiles and installs it, a `--features gpu` build
links against it, and the guest enumerates the device — `/dev/dri/card0`,
`renderD128`, `card0-Virtual-1`. What that does not say is that anything has
been *rendered* through it there; the image has to carry the guest half for
that (`apk add mesa-vulkan-virtio vulkan-loader`). Its non-Windows half still
builds on macOS in both `render-server-mode=none` and `thread`.

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

`/abin` is a directory of virtx's own executables for the guest: today `index` and `docread`, from [`virtx-abins/`](../virtx-abins), built for the machine that runs them.

Every machine gets the `abin/` directory beside the server's binary, shared read-only and mounted at `/abin`, with `/abin` put in front of the image's `PATH`.
A host with no `abin/` boots machines without one rather than refusing to.
On Windows each program is marked executable for the guest on every boot, since NTFS keeps no mode for libkrun's virtiofs to report.
`cargo xtask test` builds it beside the server binary it tests.

```sh
cargo xtask build abin                     # -> target/release/abin, beside the server;
                                           #    fetches the musl sysroot the first time
cargo xtask install --prefix <dir>         # -> <dir>/bin/abin, with the rest
```

What each program is travels with it: `virtx-abins/{program}/SKILLS.md` is installed at
`abin/skills/{program}/SKILLS.md`, so whatever gets the programs gets the reading of them too.
Under `skills/` rather than beside the executables, because the top of `/abin` is what would go
on `PATH` -- a file there that is not a program is something a shell would offer to run. A
program whose crate carries no `SKILLS.md` has none installed.

What is in one is the program's own `--help`, copied out: the front matter's `name` and
`description` are the usage line and the sentence clap opens with, and the body is that output,
subcommand by subcommand where there are subcommands. The content is settled on purpose -- a
program that answers `--help` has already written down what it is, and a second description
kept beside it is one that drifts from the parser it describes -- but **the copying is by hand
today**. Nothing checks that a `SKILLS.md` still says what its binary says, so a clap string
that moves is a file to regenerate.

## Where a host keeps things

Under one root: `$VIRTX_HOME`, or `virtx` under the user's cache directory (`~/Library/Caches`
on macOS, `%LOCALAPPDATA%` on Windows, `$XDG_CACHE_HOME` or `~/.cache` otherwise), as
[`cache_root`](src/lib.rs) answers it.

```text
<root>/
  bin/                  what `cargo xtask install` puts there: the servers, the guest kernel
                        beside them, libvirtx_core, abin/
  images/               the image store, shared by both servers
  snapshot/             the stdio server's snapshots, one ext4 each
  volume/               the stdio server's volumes
  project/<name>/       the HTTP server's, a snapshot/ and a volume/ per project; its config's
                        `home` moves the whole of it
```

## Layout

```
virtx-uvm/                  this crate: images, the disks made of them, machines and booting
                            them, snapshots, volumes and their remote stores, and the WebDAV
                            files endpoints
virtx-server-stdio/         the server over stdio, as the `virtx-stdio` binary -- and, as
                            `virtx-stdio vm`, spawned by itself, the process that becomes a VM;
                            signed on macOS
virtx-server-tcp/          the same server over HTTP, to the users and projects its config
                            names; also a VM's process, as `virtx-server-tcp vm`
virtx-uvm-guest/            the agent in every machine, which runs the commands it is sent; its
                            own workspace, built for musl by this crate's build script and
                            embedded in it
virtx-abins/                the programs for /abin -- index and docread -- and a SKILLS.md
                            each; its own workspace, built for musl
3rdparty/virglrenderer/     fork of virglrenderer, pinned to its virtx branch
3rdparty/MoltenVK/          fork of MoltenVK, with SPIRV-Cross; macOS builds only
3rdparty/libepoxy/          fork of libepoxy at 1.5.10, unchanged; Windows builds only
xtask/                      every build, check and release step as `cargo xtask`;
                            xtask/scripts/ has what it runs -- the guest kernel's and the GPU
                            libraries' builds, the musl sysroot fetch, and the musl cc/ar that
                            virtx-abins/ cross-compiles with
build/                      untracked: the kernel, the GPU libraries, the musl sysroot and the
                            meson venv, all reproducible from the scripts
```

`cargo xtask build` is what puts the server and the kernel in one directory: a server finds
the kernel by looking beside itself. A plain `cargo build` links a server, the agent embedded
in it, but not the kernel; `cargo xtask install` copies the whole set into `<prefix>/bin`. On
macOS the server needs the hypervisor entitlement, which `cargo xtask build` puts on after
cargo links it -- after rustc's own strip too, which would otherwise rewrite the signature.
`cargo xtask run` and `cargo xtask test` sign whatever they run, and put the kernel beside a
server they run.
