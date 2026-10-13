# AGENTS.md

virtx is a library for running tasks in disposable Linux micro-VMs from your own code. A console runs commands in a VM booted from an image you declare, and a filesystem you compose, mounted on the host or into the VM, is what the task reads and writes. One Rust crate carries the behaviour; the Python and Node packages are bindings over it and expose the same API in their own idiom. The console server is a prebuilt binary, not code in this repository: `ensure_virtx()` fetches the release this build was tested with into `$VIRTX_HOME/bin` the first time, and `src/ensure.rs` is the whole of how. Read this before changing anything.

## One behaviour, three surfaces

- The library is `src/`. A feature goes into the crate first and the bindings connect to it; `bindings/python/` and `bindings/node/` carry only what one language needs of its own.
- A change to what the library does moves the bindings in the same pull request, and, when a public API they show changes, the three Quickstart blocks in `README.md`, each binding's own `README.md`, and the examples under `examples/`.
- That is the standard for a change you make: the crate first, every binding connected, before the work is done. It is not the gate for a person's pull request: `CONTRIBUTING.md` accepts a change that reaches one binding only, or one binding alone, and a review holds a PR to that, with the crate-first shape as a suggestion or a follow-up.
- Names follow each language's convention and nothing else: `mount_readonly` and `ensure_virtx` in Rust and Python, `mountReadonly` and `ensureVirtx` in Node. The thing they name is the same.
- The wire between client and server is `src/protocol/`, and both ends of it are here: the messages, the client, and the `StdioServer` the released server is built on. A change there can be made and unit-tested in this repository, but it reaches a user only through a new server release, which a maintainer makes: open an issue first, with the protocol change template, so the two can be planned together.

## Where a change goes

- `src/console.rs` — the console client: the builder (image, mounts, network, GPU, resources) and `exec`, `read`, `write` on a session.
- `src/protocol/` — the messages and the stdio channel a console client and server speak. `message/` is the vocabulary, `stdio/` the transport.
- `src/fs/` — what a session reads and writes: the in-memory `Directory`, the `FileSystem` trait and its backends under `filesystem/impl/` (host passthrough, S3, Notion, Google Drive, OneDrive), and the per-OS mount bindings under `mount/impl/` (FUSE on Linux, FUSE-T on macOS, Dokany on Windows).
- `src/image.rs` — recipes, references and digests, and the client that builds and lists images.
- `src/ensure.rs` — fetching the console server binary into `$VIRTX_HOME/bin`: the release is `$VIRTX_UVM_VERSION` if set, else the one this build was pinned to, else `latest`, from `$VIRTX_DIST_URL` or the default bucket.
- `contrib/fuse_t/` — the C shim FUSE-T is loaded through on macOS, and the ABI check against its headers.
- `bindings/python/src/` and `bindings/node/src/` — the wrappers, in Rust. The Python package itself is `bindings/python/python/virtx/`; the Node package is `bindings/node/index.js` and its `.d.ts`.
- `tests/` — integration tests: real mounts (`host_mount`, `mount_teardown`, `mount_sigchld`) and the remote backends (`s3_endpoint`, `gdrive_endpoint`, `onedrive_endpoint`). Unit tests are in-module under `#[cfg(test)]`.
- `scripts/packages/` — what the release workflow runs to pack and smoke-test every package.

## Tests and lint

Run these before pushing. There is no pull-request CI, so what you run locally is the gate.

```bash
cargo fmt --all --check
cargo clippy --workspace -- -D warnings
cargo test
cargo test -- --ignored                                                # real mounts; needs a FUSE provider
(cd bindings/python && uv run maturin develop && uv run pytest)        # Python
(cd bindings/node && npm install && npm run build:debug && npm test)   # Node
```

- `git config core.hooksPath .githooks` installs the hooks in `.githooks/`: the format check on every commit, and clippy plus the library's tests on every push. `SKIP_HOOKS=1 git push` skips the hook for that one command.
- A bare `cargo test` runs the default features' tests. A change under a feature (`s3`, `notion`, `gdrive`, `onedrive`) runs with that feature on, and a change to shared code runs the sweep in `Cargo.toml`'s `[features]` comment.
- The mount tests are `#[ignore]` because they mount a real filesystem: on macOS they need FUSE-T 1.2.9 or later, on Windows Dokany, on Linux `fusermount3`. Run them with `--ignored` whenever `src/fs/mount/` changes.
- The endpoint tests skip without their service: `ONEDRIVE_CLIENT_ID` and `ONEDRIVE_REFRESH_TOKEN`, `BACKLOT_URL` for Google Drive, a reachable object store for S3. Say which ran when you report a run.
- A bug fix brings a test that fails without it. A behaviour change brings the test that pins the new behaviour, in the same surface the behaviour lives in: the library's tests are in-module or under `tests/`, the bindings' are `bindings/python/tests/` and `bindings/node/__test__/`.

## Comments and docs

- A comment states what the code beside it does now and why, in as few words as that takes. Not what it replaced, not the issue it fixed, not the conversation that produced it.
- A comment longer than the code it explains is a comment to cut.
- Every relative link in every markdown file resolves on disk.
- A module with submodules is a directory whose `mod.rs` holds its documentation and re-exports; the code is in its siblings. `src/lib.rs` says so, and new modules follow it.

## Commits and PRs

- Commit titles are `type(scope): summary`, as in `fix(fs): a mount leaves the process's SIGCHLD handling as it found it` or `docs: add CONTRIBUTING.md`: a lower-case imperative summary of the new state, with no issue number (that goes in the PR).
- A PR's title and description follow Pull requests in `CONTRIBUTING.md`: one sentence saying what virtx does once it is merged, and a description started from `.github/pull_request_template.md`, which `gh pr create --body` bypasses.
- PR and issue bodies are written one paragraph per line, never wrapped at a column: GitHub renders every newline as a line break, so a wrapped paragraph shows as ragged lines.
