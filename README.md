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
./scripts/fetch-alpine-rootfs.sh        # guest rootfs -> data/rootfs/alpine
./scripts/build-virglrenderer.sh        # only for the `gpu` feature
```

## Running

```sh
cargo run --example hello               # boots a microVM, runs a command in it
cargo run --features gpu --example hello
```

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

Guest Vulkan goes through virtio-gpu with [venus](https://docs.mesa3d.org/drivers/venus.html),
which the host serves with virglrenderer on top of MoltenVK. Getting that to
build on macOS needs a patched virglrenderer, for three reasons that are easy
to forget:

- **rutabaga_gfx requires virglrenderer ≥ 1.3.0.** The version check in its
  `build.rs` is hardcoded, so the 0.10.4-based fork that krunkit uses is not an
  option for libkrun 2.0.
- **Upstream 1.3.0 forces the render server on whenever venus is enabled**
  (`with_render_server = with_venus`, with no option to override). That server
  is a separate executable built against epoll and seccomp, so it cannot build
  on macOS. `patches/virglrenderer-1.3.0-macos-venus.patch` ports Sergio
  Lopez's in-process venus context — the same work krunkit's fork carries —
  onto upstream 1.3.0, and adds back the `render-server` meson option.
- **`vulkan-dload` defaults to true**, which makes virglrenderer resolve Vulkan
  with `dlopen` instead of linking it. The in-process venus context calls
  `vkMapMemory`/`vkUnmapMemory` directly, so the build must pass
  `-Dvulkan-dload=false` and link MoltenVK.

`scripts/build-virglrenderer.sh` handles all of it. It exports the pinned
submodule tree with `git archive` and patches the copy under `data/`, so the
`virglrenderer` submodule itself stays pristine and `git status` stays clean.
It also synthesises a `vulkan.pc` pointing at MoltenVK, since MoltenVK ships
no pkg-config file, and installs it next to the built library so consumers need
a single `PKG_CONFIG_PATH` entry. That entry is set in `.cargo/config.toml`,
without `force`, so an existing `PKG_CONFIG_PATH` in your environment wins — if
you set one, add `data/virglrenderer/lib/pkgconfig` to it yourself.

## Layout

```
krun/            submodule: libkrun, pinned
virglrenderer/   submodule: upstream virglrenderer, pinned to 1.3.0
patches/         applied to the exported submodule tree at build time
scripts/         build and provisioning helpers, plus the cargo runner
examples/
data/            untracked: rootfs, build outputs
```

`Cargo.lock` is not tracked. `data/` holds everything reproducible from the
scripts.
