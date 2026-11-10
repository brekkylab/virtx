# AGENTS.md

virtx runs tasks in disposable Linux micro-VMs from your own code. This repository is both ends of it: the client, one library with packages for Python, Node and Rust over it, and the server that runs the VMs. Read this before changing anything.

## One behaviour, two ends

- The client's behaviour is `virtx-core`, built as `libvirtx_core`. `virtx` (the Rust crate in `virtx-rs/`), `virtx-py` and `virtx-node` load that library at run time through its C ABI and carry only what one language needs of its own. A feature goes into `virtx-core` first and the packages connect to it.
- A change to what the client does moves the packages in the same pull request, and, when a public API they show changes, the Quickstart blocks in `README.md` and its translations (`README.*.md`), each package's own `README.md`, and the examples.
- That is the standard for a change you make: the library first, every package connected, before the work is done. It is not the gate for a person's pull request: `CONTRIBUTING.md` accepts a change that reaches one package only, and a review holds a PR to that, with the library-first shape as a suggestion or a follow-up.
- Names follow each language's convention and nothing else: `create_sandbox` in Rust and Python, `createSandbox` in Node. The thing they name is the same.
- The wire between client and server is `virtx-core/PROTOCOL.md`, and both ends of it are here: the types and both transports in `virtx-core`, the servers that answer them beside it. A change to the protocol moves both ends in the same pull request.

## Where a change goes

- `virtx-core/` — the methods' args and outputs (`image/`, `sandbox/`, `snapshot/`, `volume/`), the client and server ends of the transport (`transport/`, plain HTTP over TCP or over a child's stdio), the C ABI (`ffi/`, declared by `include/virtx.h`, which the build writes), and the in-memory test server (`testing.rs`, served by `examples/test_server.rs`) every package's tests run against.
- `virtx-rs/`, `virtx-py/`, `virtx-node/` — the packages: Rust over `libloading`, Python over `ctypes`, Node over `koffi`.
- `virtx-uvm/` — the server's machinery: images and their store (`image/`), the filesystems a disk is made of (`fstree/`), machines over libkrun (`machine/`), the sandboxes the servers make of them and the jobs run in each (`sandbox/`), volumes and their remote stores (`volume/`), and the WebDAV files endpoints (`files/`).
- `virtx-server-stdio/` and `virtx-server-tcp/` — the endpoints over `virtx-uvm`, served over stdio (the `virtx-stdio` binary a client spawns) and over HTTP. Each binary is also the process a VM runs in.
- `virtx-uvm-guest/` — the agent inside every VM, cross-compiled to musl and embedded by `virtx-uvm`'s build script. `virtx-abins/` — the programs every VM gets at `/abin`.
- `3rdparty/` — virglrenderer, MoltenVK, SPIRV-Cross and libepoxy as submodules, built by `xtask/scripts/build-deps.*` for the server's `gpu` feature. Each is a brekkylab fork pinned to its `virtx` branch: the upstream commit, then one commit per change of ours; libkrun and rutabaga_gfx are forks of the same shape, which cargo fetches as git dependencies.
- `xtask/` — every build, check and release step, as `cargo xtask`.
- `scripts/packages/` — what the `packages` workflow runs to pack and smoke-test the client's packages. `.github/workflows/dist.yml` builds the server for every platform and attaches the archives to the tag's GitHub release.

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

## Tests and lint

```bash
cargo xtask check                                 # the gate: format, clippy and tests, of the client always and of the servers once `init` has run
cargo xtask test                                  # the client's tests, the e2e scenarios against the servers once `init` has run
cargo xtask test -p virtx-server-stdio            # the servers' own tests, which boot VMs
(cd virtx-py && uv run pytest)                    # Python
(cd virtx-node && npm install && npm test)        # Node
```

- Run `cargo xtask check` before pushing. `git config core.hooksPath .githooks` installs the hooks in `.githooks/`: the format check on every commit, and `cargo xtask check` on every push. `SKIP_HOOKS=1 git push` skips the hook for that one command.
- A change to the client needs nothing of `init`; `cargo xtask check --client` checks it alone. A change to the servers needs `init`, and a change that reaches a VM runs `cargo xtask test` for the server it touches.
- A bug fix brings a test that fails without it. A behaviour change brings the test that pins the new behaviour, in the surface the behaviour lives in: `virtx-core`'s in-module tests and `tests/`, the servers' `tests/`, `virtx-py/tests/` and `virtx-node/__test__/`.

## Comments and docs

- A comment states what the code beside it does now and why, in as few words as that takes. Not what it replaced, not the issue it fixed, not the conversation that produced it.
- A comment longer than the code it explains is a comment to cut.
- Every relative link in every markdown file resolves on disk.
- A module with submodules is a directory whose `mod.rs` holds its documentation and re-exports; the code is in its siblings. `src/lib.rs` says so, and new modules follow it.

## Commits and PRs

- Commit titles are `type(scope): summary`, as in `fix(fs): a mount leaves the process's SIGCHLD handling as it found it` or `docs: add CONTRIBUTING.md`: a lower-case imperative summary of the new state, with no issue number (that goes in the PR).
- A PR's title and description follow Pull requests in `CONTRIBUTING.md`: one sentence saying what virtx does once it is merged, and a description started from `.github/pull_request_template.md`, which `gh pr create --body` bypasses.
- PR and issue bodies are written one paragraph per line, never wrapped at a column: GitHub renders every newline as a line break, so a wrapped paragraph shows as ragged lines.
