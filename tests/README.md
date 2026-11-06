# End-to-end tests

Each package's own suite runs against `virtx-core`'s in-memory `test_server`. These run the
same client code against the real server, `virtx-uvm`, booting a VM: a pull from the registry,
a sandbox on the image, a command in it, and the CLI each package ships.

```sh
cargo xtask e2e
```

That builds `libvirtx_core`, `virtx-uvm` (with the guest kernel beside it and signed, as a
client needs it) and the `virtx` CLI, and runs every suite here with:

| Variable         | What it names                                              |
|------------------|------------------------------------------------------------|
| `VIRTX_UVM`      | the server a client spawns when it names none              |
| `VIRTX_CORE_LIB` | `libvirtx_core`, which the Python, Node and Rust packages open |
| `VIRTX_HOME`     | a home of this run's own, removed afterwards               |

The suites need those set, so they are run through `cargo xtask e2e` rather than directly.
`cargo xtask build kernel` first, once per machine; `uv` for Python, and `node` with
`npm install` run in `virtx-node/`.

| Suite             | Runs                                                     |
|-------------------|----------------------------------------------------------|
| `python/`         | `uv run pytest`, from `virtx-py/`                        |
| `node/`           | `node --test`                                            |
| `rust/`           | `cargo test -p virtx-e2e`, over `virtx-rs`               |
