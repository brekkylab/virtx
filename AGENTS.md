# AGENTS.md

virtx is a microVM sandbox for AI agents: it runs tasks in disposable Linux microVMs from your own code. This repository is both ends of it: the client, one library with packages for Python, Node and Rust over it, and the server that runs the VMs. Read this before changing anything.

## One behaviour, two ends

- The client's behaviour is `virtx-core`, built as `libvirtx_core` by `virtx-cabi`. `virtx` (the Rust crate in `virtx-rs/`), `virtx-py` and `virtx-node` load that library at run time through its C ABI and carry only what one language needs of its own. A feature goes into `virtx-core` first and the packages connect to it.
- A change to what the client does moves the packages in the same pull request, and, when a public API they show changes, the Quickstart blocks in `README.md`, which every package ships as its own, and its translations (`README.*.md`), and the examples.
- That is the standard for a change you make: the library first, every package connected, before the work is done. It is not the gate for a person's pull request: `CONTRIBUTING.md` accepts a change that reaches one package only, and a review holds a PR to that, with the library-first shape as a suggestion or a follow-up.
- Names follow each language's convention and nothing else: `create_sandbox` in Rust and Python, `createSandbox` in Node. The thing they name is the same.
- The wire between client and server is the protocol, `docs/content/reference/protocol.mdx` (what code comments mean by "the protocol"), and both ends of it are here: the types and both transports in `virtx-core`, the servers that answer them beside it. A change to the protocol moves both ends in the same pull request.

## Where a change goes

- `virtx-core/` — the methods' args and outputs (`image/`, `sandbox/`, `volume/`), the client and server ends of the transport (`transport/`, plain HTTP over TCP or over a child's stdio), the C ABI (`ffi/`, declared by `include/virtx.h`, which the build writes).
- `virtx-cabi/` — `libvirtx_core`, the C ABI exported as a cdylib: a crate of its own, so that no other crate's build makes one.
- `virtx-rs/`, `virtx-py/`, `virtx-node/` — the packages: Rust over `libloading`, Python over `ctypes`, Node over `koffi`. The Python package builds `virtx-cabi` with setuptools-rust (`virtx-py/pyproject.toml`), the Node package with napi-rs (`virtx-node/package.json`).
- `virtx-uvm/` — the server's machinery: images and their store (`image/`), the filesystems a disk is made of (`fstree/`), machines over libkrun (`machine/`), the sandboxes the servers make of them and the jobs run in each (`sandbox/`), volumes and their remote stores (`volume/`), and the WebDAV files endpoints (`files/`).
- `virtx-server-stdio/` and `virtx-server-tcp/` — the endpoints over `virtx-uvm`, served over stdio (the `virtx-stdio` binary a client spawns) and over HTTP. Each binary is also the process a VM runs in. `virtx-server-tcp` is a library too, which its binary and a server with an authenticator and routes of its own are built from.
- `virtx-uvm-guest/` — the agent inside every VM, cross-compiled to musl and embedded by `virtx-uvm`'s build script. `virtx-abins/` — the programs a sandbox gets at `/abin`, those its `abins` name; each one's `SKILL.md` is what an agent reads to use it: its `--help` copied by hand, so a clap string that changes is a `SKILL.md` to regenerate, or, for `browser`, which runs another's program, a guide to that program written for it. What every VM gets at `/alib` is built by `xtask/scripts/build-alib.sh`.
- `3rdparty/` — libkrun, rutabaga_gfx, smoltcp, virglrenderer, MoltenVK and SPIRV-Cross as submodules, virglrenderer and MoltenVK each built by its own script there, `3rdparty/_scripts/build-virglrenderer.*` and `build-moltenvk.sh`, which `xtask/scripts/build-deps.*` runs in order for the server's `gpu` feature. The guest kernel, libkrunfw, is built there too, by `3rdparty/_scripts/build-libkrunfw.sh` from upstream's tag, with no submodule and no patch. None is a fork: each is upstream's own repository pinned to a commit of its `main`, and what virtx changes in it is its patches in `3rdparty/_patches/`; each patch's message says why virtx needs it and when it can go. libkrun's, rutabaga_gfx's and smoltcp's are applied in each submodule's own tree by `cargo xtask init`, since cargo takes their crates from there by path, and the others' to a copy under `build/` by their build. `3rdparty/README.md` says how a patch is written and when one is taken out, and a project virtx comes to change is taken the same way.
- `xtask/` — every build, check and release step, as `cargo xtask`.
- `scripts/packages/` — what the workflows run to smoke-test the client's packages. `.github/actions/pack` is the one place they are built, each platform's with the packages' own tools, and `.github/workflows/packages.yml` tries them on a runner that did not build them.
- `.github/workflows/` — `pipeline` lints and tests every platform, and in full also builds the servers (`virtx-stdio` and `virtx-server-tcp`, each archived with the guest kernel, abin and alib) and the client's packages for every platform and smoke-tests them (`packages`); `ci` runs it on a pull request (lint and tests) and on `main` (in full), and `release` runs it in full on a version tag (`vX.Y.Z`, or a pre-release `vX.Y.Z-alpha.N`, `-beta.N` or `-rc.N`, which `pipeline` refuses any other spelling of), runs `publish`'s checks over what was built, and attaches it to the tag's draft GitHub release. Publishing the draft runs `publish`, which checks every package of the release and, all of them right, puts its npm packages and wheels on npm and PyPI and the crates on crates.io. `docs` builds `docs/` and checks its links on a pull request that touches it.

## Building from source

Every step is `cargo xtask`, run from a clone of this repository; `cargo xtask --help` lists them.

```bash
git clone https://github.com/brekkylab/virtx && cd virtx
git submodule update --init 3rdparty/libkrun 3rdparty/rutabaga_gfx 3rdparty/smoltcp   # cargo reads their crates' manifests, whatever it builds
```

### Server

The servers need more of the host than Rust, and Docker on every host, which builds the guest kernel:

- **macOS**, Apple silicon: `brew install cmake lld llvm meson ninja`, and Docker running Linux containers (Docker Desktop or colima). lld links the guest's init, llvm's `llvm-ar` archives `virtx-abins/` (Apple's `ar` writes a symbol table `rust-lld` reads as empty), meson and ninja build the GPU libraries, and cmake and ninja the ONNX Runtime `docread` links. Without Docker, `LIBKRUNFW_KERNEL_C=<path>` names a `kernel.c` built elsewhere.
- **Windows**, x86_64: the Windows Hypervisor Platform feature, MSVC build tools, clang and lld, and Docker in Linux-container mode. The `gpu` feature also needs `LIBCLANG_PATH` naming the directory `libclang.dll` is in, best set in `%USERPROFILE%\.cargo\config.toml` since the path is the machine's; no Vulkan SDK is needed. Smart App Control blocks every unsigned build script, failing with `os error 4551`, so it has to be off.

```bash
cargo xtask init                                  # checks out the submodules, applies libkrun's, rutabaga_gfx's and smoltcp's patches and adds the guest's musl target; the servers need it
cargo xtask build kernel server-stdio server-tcp  # the guest kernel and both servers, signed on macOS
cargo xtask install --prefix ~/.local             # what was built, into ~/.local/bin
export VIRTX_STDIO=~/.local/bin/virtx-stdio       # what Client.spawn() starts, in place of a downloaded one
```

- The first `build kernel` is a full kernel build, 10 to 20 minutes; `LIBKRUNFW_REBUILD=1` builds it again. `build abin` and `build alib` (which needs Docker) make what sandboxes get at `/abin` and `/alib`, and `install` copies them with the rest. The first `build abin` fetches about 560 MB, ONNX Runtime's source and dependencies and `docread`'s models, and builds ONNX Runtime, several minutes more; on Linux it needs the `cmake` and `ninja-build` packages.
- `cargo xtask build` and `test` put the guest kernel beside a server, where it looks for it, and on macOS sign the server with `com.apple.security.hypervisor`, which Hypervisor.framework requires; `cargo xtask run` only signs. A plain `cargo build`, `run` or `test`, an IDE's Run button included, does neither, and its server cannot boot a machine.
- Set no library search path for the servers: one wide enough to find the kernel lets another virglrenderer, such as krunkit's, stand in for the one in `build/`, silently.
- `KRUN_LOG=debug` (or `info`, `warn`, `trace`) turns on libkrun's log, which reaches the server with the guest's console.
- `build/` is untracked and reproducible from `xtask/scripts/` and `3rdparty/_scripts/`: the kernel, the GPU libraries, alib's builds, the musl sysroot and the meson venv.

### Clients

The Python and Node packages, and the Rust crate without its `embedded` feature, open one library, `libvirtx_core`, which `virtx-cabi` builds. Installed from the clone, each package builds it with its language's own tool and keeps it beside its files:

- **Python:** `pip install path/to/virtx/virtx-py`, with setuptools-rust, in `release`. `pip install -e` (and `uv sync` in `virtx-py/`) builds it in `dev`, and `uv` builds it again when the crates change.
- **Node:** `npm install` in `virtx-node/`, whose `prepare` runs `napi build`, in `dev`, as `pretest` does before `npm test`; then `npm install path/to/virtx/virtx-node` in your own project. npm installs no `devDependencies` of a directory another project installs, so napi is there only once `virtx-node/` has had its own `npm install`.
- **Rust:** `cargo add virtx --path path/to/virtx/virtx-rs`, which links the library in; with `--no-default-features` it opens one at run time instead, from `VIRTX_CORE_LIB`.

- `cargo build -p virtx-cabi` builds the library alone; `cargo xtask build client` builds it and `libvirtx_core.a` in `release`, and `cargo xtask install` copies the library out.
- `VIRTX_CORE_LIB` names a library to open in place of the one a package carries; `load(path)` opens one from the program itself; a process opens one.
- A module that takes a client from another opens the file `library_path()` answers, and takes the client from the pointer `client.clone().into_raw()` answers, with `Client.from_raw`. In Node, `libraryPath()`, `intoRaw()` and `fromRaw`.
- A change to the client needs nothing of `init`, only those three submodules checked out, as every cargo command does. A change to the servers needs `init` and the guest kernel, and `init` again after a pull that changed their patches in `3rdparty/_patches/` or their pins: a server is not built from a tree `init` has not brought to them. It is `init` that moves those three to a new pin, not `git submodule update` or `git pull --recurse-submodules`, which git refuses over a tree with the patches applied.

## Tests

### Principles

- A test pins what is required, not what the code happens to do: each call a caller has is tested for what it promises, even where two share one implementation now.
- No mock or in-memory server stands in for a real one. What needs a server runs against a server's own `app`, or against the fixture.
- A test's steps are written in it. A helper is for setup several tests share, not for a line or a step one test takes.
- A bug fix brings a test that fails without it. A behaviour change brings the test that pins the new behaviour, in the layer *Where a test goes* puts it.

### Where a test goes

Each test goes in the lowest layer that can show what it tests, fastest first:

- **`virtx-core`: unit tests.** What it does by itself, in a `#[cfg(test)] mod tests {}` inside the module it tests. Its transport is tested from one end to the other there, since both ends are in the crate.
- **`virtx-uvm`: unit tests, and the tests that boot machines.** Its unit tests sit in its modules as `virtx-core`'s do. A machine is a process the binary it is in spawns of itself (see `machine/`), which a module's test cannot be, so the tests that boot one are `virtx-uvm/tests/machine.rs`, a test binary with a `main` that becomes the machine when spawned so. They make sandboxes of `virtx-uvm` itself, with no server in front: a job's output, end and timeout, a job cancelled, a machine parked and booted again, what a closed sandbox leaves, sandboxes made, closed and mounting one volume at once, and what the files endpoints answer a write to a sandbox's file.
- **`virtx-uvm-guest`: unit tests, on Linux alone.** The agent's own, in `virtx-uvm-guest/src/main.rs`, over directories standing in for the guest's `/dev` and sysfs. It is a workspace of its own and builds for Linux only, so `cargo xtask check` and `cargo xtask test` run them on Linux and skip them elsewhere.
- **`virtx-server-*`: unit tests, and the app served in the test's process.** Each server's `app` -- what its binary serves -- is served in the test's own process, over a pipe as stdio serves it or on the loopback over HTTP, and called as a client calls it: the endpoints, what they refuse, who reaches which project, and what each keeps on disk. No machine is booted here: what a server does with one, such as a build's steps, is tested end to end.
- **`virtx-rs`, `virtx-py`, `virtx-node`: end to end, against the servers.** Each package through its own runner, against the real servers served as the fixture: a client's flow through the package, the library, a transport and a server to a machine, such as a sandbox's files as its processes see them, its ports, and a build. There is no other suite of them. The flows, and what a server does in them, are `virtx-rs`'s, since every package reaches the server through the one library. `virtx-py` and `virtx-node` test their own layer: each call goes through, its arguments as the language takes them, its answer as the language's types, its log as the language iterates one, and its refusal as the language raises one -- not a server's behaviour again.

- A refusal, a bad argument or any case a server answers without a machine is tested in the server, where it is fast and exact; the end-to-end tests are for the flows a client goes through.

### Running them

```bash
cargo xtask test                                  # every suite, each summed up at the end; those that need the servers are skipped until they can be built
cargo xtask test --no-run                         # compiles and installs everything `test` would and runs none; needs no guest kernel
cargo xtask test -p virtx-core                    # one crate's tests
cargo xtask test -p virtx-uvm                     # its unit tests, and the tests that boot machines
(cd virtx-uvm-guest && cargo test)                # the agent's unit tests, on Linux
cargo xtask test -p virtx-server-stdio            # a server's own, its app served in the test's process
cargo xtask test -p virtx                         # Rust, against the fixture
cargo xtask test -p virtx-cli                     # the command's signing in, against the fixture
cargo xtask test abin                             # virtx-abins/'s, for the guest's target: run on Linux, compiled alone elsewhere
(cd virtx-py && uv run pytest)                    # Python; its conftest serves the fixture
(cd virtx-node && npm install && npm test)        # Node; `npm test` serves the fixture
cargo xtask fixture -- <command>                  # any command, against the fixture
```

- The fixture is `virtx-stdio` and `virtx-server-tcp`, built and signed, over a home of the run's own, removed once it ends, and an OpenID Connect provider of the run's own, which the server takes the tokens of and `virtx login` signs in to. `cargo xtask fixture` serves it and names it to the tests in `VIRTX_TEST_STDIO`, `VIRTX_TEST_URL`, `VIRTX_TEST_TOKEN` and `VIRTX_TEST_PROJECT`, and the tokens of two more projects' members in `VIRTX_TEST_USERS`; a test reads those, and starts no server of its own.
- A run already under the fixture, as `cargo xtask test` runs `npm test` and `uv run pytest`, serves no other.

## Lint and the gate

```bash
cargo xtask check                                 # format, clippy and the crates' unit tests: of the client always, of the servers once `init` has run; and the bindings' lint (ruff, biome, tsc) where uv and npm are on PATH
cargo xtask check --client                        # the client's alone, without `init`
cargo xtask clippy --gpu                          # clippy alone, the servers with `gpu`: this host's own `cfg`s, as a release builds them
```

- Run `cargo xtask check` before pushing. CI (`ci`) runs it on every pull request too, lints each platform's own code with `cargo xtask clippy --gpu` and runs the machine-booting tests, but a push that fails here only finds out later there. Building, packing and trying each platform's release runs on `main` after a merge, and when `ci` is run by hand on a branch (`gh workflow run ci --ref <branch>`). `git config core.hooksPath .githooks` installs the hooks in `.githooks/`: the format check on every commit, and `cargo xtask check` on every push. `SKIP_HOOKS=1 git push` skips the hook for that one command.
- `cargo xtask check` runs no test that boots a VM. A change that reaches one runs `cargo xtask test`, or the suites it touches.

## Comments and docs

- A comment states what the code beside it does now and why, in as few words as that takes. Not what it replaced, not the issue it fixed, not the conversation that produced it.
- A comment longer than the code it explains is a comment to cut.
- Every relative link in every markdown file resolves on disk.
- A module with submodules is a directory whose `mod.rs` holds its documentation and re-exports; the code is in its siblings. `src/lib.rs` says so, and new modules follow it.

## Commits and PRs

- Commit titles are `type(scope): summary`, as in `fix(fs): a mount leaves the process's SIGCHLD handling as it found it` or `docs: add CONTRIBUTING.md`: a lower-case imperative summary of the new state, with no issue number (that goes in the PR).
- A PR's title and description follow Pull requests in `CONTRIBUTING.md`: one sentence saying what virtx does once it is merged, and a description started from `.github/pull_request_template.md`, which `gh pr create --body` bypasses.
- PR and issue bodies are written one paragraph per line, never wrapped at a column: GitHub renders every newline as a line break, so a wrapped paragraph shows as ragged lines.
