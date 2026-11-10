# Contributing

Thanks for your interest in improving **virtx**. This repository is both ends of it: the client, one library (`virtx-core`, built as `libvirtx_core`) with packages for Python, Node and Rust over it, and the server that runs the VMs (`virtx-uvm` and the two servers over it). [`AGENTS.md`](AGENTS.md) says where each kind of change goes.

## Development setup

The toolchain is pinned in `rust-toolchain.toml`; `rustup` installs it on the first `cargo` call. [`AGENTS.md`](AGENTS.md) covers [building from source](AGENTS.md#building-from-source), [tests](AGENTS.md#tests) and [lint](AGENTS.md#lint-and-the-gate).

## Pull requests

1. Create a topic branch off `main`.
2. Keep changes focused; one logical change per PR.
3. Bring a test that fails without the change. A bug fix comes with the test that caught it; a behaviour change comes with the test that pins the new behaviour, in the surface it lives in.
4. The shape to aim for is `virtx-core` first, with each package connecting to it, but do not hold a PR back for it: a change that reaches one package only is welcome as it is. A maintainer may suggest moving it into `virtx-core`, and that can be a follow-up PR.
5. A change to the protocol (`virtx-core/PROTOCOL.md`) moves the client and the servers in the same PR, since both are here. Open an issue first, with the [protocol change template](.github/ISSUE_TEMPLATE/protocol.md), so the change is agreed before it is built.
6. Pass the tests and lint before opening the PR: `cargo xtask check`, and the tests of whatever the change touches, as [Tests](AGENTS.md#running-them) lists.
7. Title the PR with one sentence saying what virtx does once it is merged, with no issue number. The issue goes in the description as `Closes #N` or `Resolves #N`.
8. Fill in the pull request template, describing the finished state rather than the rounds of work behind it. Problem, cause, fix and result are for a change that fixes something: drop the ones that do not apply, and a change with no problem behind it has a Changes section in place of Fix. The verification lines and the three checkboxes always stay. `gh pr create --body` does not load the template, so start the description from [`.github/pull_request_template.md`](.github/pull_request_template.md).

## Reporting bugs and requesting features

Open an issue at https://github.com/brekkylab/virtx/issues with what you ran, what you expected, and what happened, including the language you used virtx from and the host OS. A suspected vulnerability goes through [SECURITY.md](SECURITY.md) instead, never a public issue.

## Code of Conduct

Taking part in this project means following our [Code of Conduct](CODE_OF_CONDUCT.md).

## License

By contributing, you agree that your contributions are licensed under the [Apache-2.0 License](LICENSE).
