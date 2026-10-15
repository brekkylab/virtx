# Contributing

Thanks for your interest in improving **virtx**. One Rust crate carries the behaviour; the Python and Node packages are bindings over it, so a change usually touches the crate and then each binding. [`AGENTS.md`](AGENTS.md) says where each kind of change goes.

## Development setup

The toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it on the first `cargo` call.

```bash
git clone https://github.com/brekkylab/virtx
cd virtx
cargo build
(cd bindings/python && uv run maturin develop)       # Python
(cd bindings/node && npm install && npm run build)   # Node
```

A console runs on the console server, a prebuilt binary that is not in this repository: `ensure_virtx()` fetches the release this build was tested with into `$VIRTX_HOME/bin` on first use, and `VIRTX_UVM_VERSION` names another release to fetch instead. Mounting a tree on the host needs a FUSE provider: `fusermount3` on Linux, FUSE-T 1.2.9 or later on macOS (`brew install --cask fuse-t`), Dokany on Windows.

## Running tests and lint

```bash
cargo fmt --all --check
cargo clippy --workspace -- -D warnings
cargo test
cargo test -- --ignored                 # the real-mount tests; needs the FUSE provider above
(cd bindings/python && uv run pytest)   # after `maturin develop`
(cd bindings/node && npm test)          # after `npm run build:debug`
```

There is no pull-request CI, so what you run locally is the gate. `git config core.hooksPath .githooks` installs the hooks in `.githooks/`: the format check on every commit, and clippy plus the library's tests on every push. `SKIP_HOOKS=1 git push` skips the hook for that one command.

A bare `cargo test` covers the default features. A change under `s3`, `notion`, `gdrive` or `onedrive` runs with that feature on, and the `[features]` comment in `Cargo.toml` has the full sweep. The endpoint tests under `tests/` skip without their service and say so, and `tests/console_jobs.rs` and the bindings' console tests skip without `VIRTX_CONSOLE`, the path of a console server to run them against; name which ran when you report a run.

## Pull requests

1. Create a topic branch off `main`.
2. Keep changes focused; one logical change per PR.
3. Bring a test that fails without the change. A bug fix comes with the test that caught it; a behaviour change comes with the test that pins the new behaviour, in the surface it lives in.
4. The shape to aim for is the crate first, with each binding connecting to it, but do not hold a PR back for it: a change that reaches one binding only, or one binding alone, is welcome as it is. A maintainer may suggest moving it into the crate, and that can be a follow-up PR.
5. A change under `src/protocol/` can be made and tested here, but reaches a user only through a new release of the console server, which a maintainer makes: open an issue first, with the [protocol change template](.github/ISSUE_TEMPLATE/protocol.md), so the two can be planned together.
6. Run the checks above before opening the PR.
7. Title the PR with one sentence saying what virtx does once it is merged, with no issue number. The issue goes in the description as `Closes #N` or `Resolves #N`.
8. Fill in the pull request template, describing the finished state rather than the rounds of work behind it. Problem, cause, fix and result are for a change that fixes something: drop the ones that do not apply, and a change with no problem behind it has a Changes section in place of Fix. The verification lines and the three checkboxes always stay. `gh pr create --body` does not load the template, so start the description from [`.github/pull_request_template.md`](.github/pull_request_template.md).

## Reporting bugs and requesting features

Open an issue at https://github.com/brekkylab/virtx/issues with what you ran, what you expected, and what happened, including the language you used virtx from and the host OS. A suspected vulnerability goes through [SECURITY.md](SECURITY.md) instead, never a public issue.

## Code of Conduct

Taking part in this project means following our [Code of Conduct](CODE_OF_CONDUCT.md).

## License

By contributing, you agree that your contributions are licensed under the [Apache-2.0 License](LICENSE).
