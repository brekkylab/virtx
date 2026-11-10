# AGENTS.md

virtx runs tasks in disposable Linux micro-VMs from your own code. This repository is both ends of it: the client, one library with packages for Python, Node and Rust over it, and the server that runs the VMs. Read this before changing anything.

## One behaviour, two ends

- The client's behaviour is `virtx-core`, built as `libvirtx_core`. `virtx` (the Rust crate in `virtx-rs/`), `virtx-py` and `virtx-node` load that library at run time through its C ABI and carry only what one language needs of its own. A feature goes into `virtx-core` first and the packages connect to it.
- A change to what the client does moves the packages in the same pull request, and, when a public API they show changes, the Quickstart blocks in `README.md` and its translations (`README.*.md`), each package's own `README.md`, and the examples.
- That is the standard for a change you make: the library first, every package connected, before the work is done. It is not the gate for a person's pull request: `CONTRIBUTING.md` accepts a change that reaches one package only, and a review holds a PR to that, with the library-first shape as a suggestion or a follow-up.
- Names follow each language's convention and nothing else: `create_sandbox` in Rust and Python, `createSandbox` in Node. The thing they name is the same.
- The wire between client and server is `virtx-core/PROTOCOL.md`, and both ends of it are here: the types and both transports in `virtx-core`, the servers that answer them beside it. A change to the protocol moves both ends in the same pull request.

## Where a change goes

- `virtx-core/` — the methods' args and outputs (`image/`, `sandbox/`, `snapshot/`, `volume/`), the client and server ends of the transport (`transport/`, plain HTTP over TCP or over a child's stdio), the C ABI (`ffi/`, declared by `include/virtx.h`, which the build writes).
- `virtx-rs/`, `virtx-py/`, `virtx-node/` — the packages: Rust over `libloading`, Python over `ctypes`, Node over `koffi`.
- `virtx-uvm/` — the server's machinery: images and their store (`image/`), the filesystems a disk is made of (`fstree/`), machines over libkrun (`machine/`), the sandboxes the servers make of them and the jobs run in each (`sandbox/`), volumes and their remote stores (`volume/`), and the WebDAV files endpoints (`files/`).
- `virtx-server-stdio/` and `virtx-server-tcp/` — the endpoints over `virtx-uvm`, served over stdio (the `virtx-stdio` binary a client spawns) and over HTTP. Each binary is also the process a VM runs in.
- `virtx-uvm-guest/` — the agent inside every VM, cross-compiled to musl and embedded by `virtx-uvm`'s build script. `virtx-abins/` — the programs every VM gets at `/abin`.
- `3rdparty/` — virglrenderer, MoltenVK, SPIRV-Cross and libepoxy as submodules, built by `xtask/scripts/build-deps.*` for the server's `gpu` feature. Each is a brekkylab fork pinned to its `virtx` branch: the upstream commit, then one commit per change of ours; libkrun and rutabaga_gfx are forks of the same shape, which cargo fetches as git dependencies.
- `xtask/` — every build, check and release step, as `cargo xtask`.
- `scripts/packages/` — what the workflows run to pack and smoke-test the client's packages.
- `.github/workflows/` — `build` builds the server and the client's packages for every platform and `test` tests them, `cargo xtask test` among it, on Linux over KVM; `ci` runs both on a pull request, and `release` runs both on a version tag and attaches what was built to the tag's draft GitHub release. Publishing the draft runs `publish`, which puts its npm packages and wheels on npm and PyPI. `docs` builds `docs/` and checks its links on a pull request that touches it.

## Building from source

Every step is `cargo xtask`, run from a clone of this repository; `cargo xtask --help` lists them.

```bash
git clone https://github.com/brekkylab/virtx && cd virtx
```

### Server

The servers need more of the host than Rust; [virtx-uvm/README.md](virtx-uvm/README.md) lists what each host needs.

```bash
cargo xtask init                                  # checks out the submodules and adds the guest's musl target; the servers need it
cargo xtask build kernel server-stdio server-tcp  # the guest kernel and both servers, signed on macOS
cargo xtask install --prefix ~/.local             # what was built, into ~/.local/bin
export VIRTX_STDIO=~/.local/bin/virtx-stdio       # what Client.spawn() starts, in place of a downloaded one
```

### Clients

Every package loads one library, `libvirtx_core`, so build it first and point the package at it:

```bash
cargo xtask build client
cargo xtask install --prefix ~/.local
export VIRTX_CORE_LIB=~/.local/bin/libvirtx_core.so   # .dylib on macOS, virtx_core.dll on Windows
```

Then, in your own project, install the package for your language from the clone:

- **Python:** `pip install path/to/virtx/virtx-py`
- **Node:** `npm install path/to/virtx/virtx-node`
- **Rust:** `cargo add virtx --path path/to/virtx/virtx-rs`

- A change to the client needs nothing of `init`. A change to the servers needs `init` and the guest kernel.

## Tests

### Principles

- A test pins what is required, not what the code happens to do: each call a caller has is tested for what it promises, even where two share one implementation now.
- No mock or in-memory server stands in for a real one. What needs a server runs against a server's own `app`, or against the fixture.
- A test's steps are written in it. A helper is for setup several tests share, not for a line or a step one test takes.
- A bug fix brings a test that fails without it. A behaviour change brings the test that pins the new behaviour, in the layer *Where a test goes* puts it.

### Where a test goes

Each test goes in the lowest layer that can show what it tests, fastest first:

- **`virtx-core`: unit tests.** What it does by itself, in a `#[cfg(test)] mod tests {}` inside the module it tests. Its transport is tested from one end to the other there, since both ends are in the crate.
- **`virtx-uvm`: unit tests, and the tests that boot machines.** Its unit tests sit in its modules as `virtx-core`'s do. A machine is a process the binary it is in spawns of itself (see `machine/`), which a module's test cannot be, so the tests that boot one are `virtx-uvm/tests/machine.rs`, a test binary with a `main` that becomes the machine when spawned so. They make sandboxes of `virtx-uvm` itself, with no server in front: a job's output, end and timeout, a job cancelled, a machine parked and booted again, what a snapshot keeps, and sandboxes made, closed and mounting one volume at once.
- **`virtx-server-*`: unit tests, and the app served in the test's process.** Each server's `app` -- what its binary serves -- is served in the test's own process, over a pipe as stdio serves it or on the loopback over HTTP, and called as a client calls it: the endpoints, what they refuse, who reaches which project, and what each keeps on disk. No machine is booted here: what a server does with one, such as a build's steps, is tested end to end.
- **`virtx-rs`, `virtx-py`, `virtx-node`: end to end, against the servers.** Each package through its own runner, against the real servers served as the fixture: a client's flow through the package, the library, a transport and a server to a machine, such as a sandbox's files as its processes see them, its ports, and a build. There is no other suite of them. The flows, and what a server does in them, are `virtx-rs`'s, since every package reaches the server through the one library. `virtx-py` and `virtx-node` test their own layer: each call goes through, its arguments as the language takes them, its answer as the language's types, its log as the language iterates one, and its refusal as the language raises one -- not a server's behaviour again.

- A refusal, a bad argument or any case a server answers without a machine is tested in the server, where it is fast and exact; the end-to-end tests are for the flows a client goes through.

### Running them

```bash
cargo xtask test                                  # every suite, each summed up at the end; those that need the servers are skipped until they can be built
cargo xtask test -p virtx-core                    # one crate's tests
cargo xtask test -p virtx-uvm                     # its unit tests, and the tests that boot machines
cargo xtask test -p virtx-server-stdio            # a server's own, its app served in the test's process
cargo xtask test -p virtx                         # Rust, against the fixture
(cd virtx-py && uv run pytest)                    # Python; its conftest serves the fixture
(cd virtx-node && npm install && npm test)        # Node; `npm test` serves the fixture
cargo xtask fixture -- <command>                  # any command, against the fixture
```

- The fixture is `virtx-stdio` and `virtx-server-tcp`, built and signed, over a home of the run's own, removed once it ends, and an OpenID Connect provider of the run's own whose token the server takes. `cargo xtask fixture` serves it and names it to the tests in `VIRTX_TEST_STDIO`, `VIRTX_TEST_URL`, `VIRTX_TEST_TOKEN` and `VIRTX_TEST_PROJECT`; a test reads those, and starts no server of its own.
- A run already under the fixture, as `cargo xtask test` runs `npm test` and `uv run pytest`, serves no other.

## Lint and the gate

```bash
cargo xtask check                                 # format, clippy and the crates' unit tests: of the client always, of the servers once `init` has run
cargo xtask check --client                        # the client's alone, without `init`
```

- Run `cargo xtask check` before pushing. There is no pull-request CI, so what you run locally is the gate. `git config core.hooksPath .githooks` installs the hooks in `.githooks/`: the format check on every commit, and `cargo xtask check` on every push. `SKIP_HOOKS=1 git push` skips the hook for that one command.
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
