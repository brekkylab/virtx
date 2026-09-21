# cortex-krun

Rust host-side bindings and runtime built on [libkrun](https://github.com/libkrun/libkrun) 2.0.

libkrun is vendored as a submodule and linked as an rlib, so there is no
`libkrun.so`/`libkrun.dylib` to ship and no C ABI in the middle — `cortex-krun`
depends on the `libkrun` crate directly.

> libkrun 2.0 is still under active development and its API is not stable. The
> `krun` submodule is pinned to a specific commit for that reason; bump it
> deliberately, not automatically.

## Requirements

macOS on Apple Silicon (Linux hosts are untested here).

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
./scripts/apply-patches.sh              # patches the krun and rutabaga submodules
./scripts/fetch-alpine-rootfs.sh        # guest rootfs -> data/rootfs/alpine
./scripts/build-virglrenderer.sh        # only for the `gpu` feature
```

## Running

```sh
cargo run --example hello               # boots a microVM, runs a command in it
cargo run --example macos_gpu -- 'uname -a'          # adds TSI networking
cargo run --features gpu --example macos_gpu         # vulkaninfo over venus
./scripts/run-ncnn-squeezenet.sh                     # a CNN on the guest GPU
```

`KRUN_LOG=debug` (also `info`, `warn`, `trace`) turns on libkrun's own logging.

`cargo run` goes through `scripts/macos-run.sh`, registered as the cargo runner
for `aarch64-apple-darwin`. It does two things that are mandatory on macOS:

1. Ad-hoc codesigns the binary with `com.apple.security.hypervisor`. No
   developer certificate is needed, but Hypervisor.framework refuses to start
   without the entitlement.
2. Puts Homebrew's `lib` on `DYLD_LIBRARY_PATH`, because libkrun `dlopen`s
   `libkrunfw.5.dylib` by leaf name and `/opt/homebrew/lib` is not on dyld's
   default search path.

Running the built binary directly, without the runner, will fail on both counts.

## The `gpu` feature

Guest Vulkan goes through virtio-gpu with [venus](https://docs.mesa3d.org/drivers/venus.html):
the guest runs Mesa's venus driver, virglrenderer serves it on the host, and
MoltenVK turns that into Metal. Working end to end:

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

### rutabaga_gfx — `patches/rutabaga-8cb133e-macos-virgl-renderer.patch`

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

### libkrun — `patches/libkrun-e6cdb55-macos-gpu.patch`

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

## Layout

```
guest/             sources compiled and run inside the VM
krun/            submodule: libkrun, pinned
rutabaga/        submodule: rutabaga_gfx, pinned (via [patch.crates-io])
virglrenderer/   submodule: upstream virglrenderer, pinned to 1.3.0
patches/         see "The gpu feature"
scripts/         build and provisioning helpers, plus the cargo runner
examples/
data/            untracked: rootfs, models, build outputs
```

`Cargo.lock` is not tracked. `data/` holds everything reproducible from the
scripts.
