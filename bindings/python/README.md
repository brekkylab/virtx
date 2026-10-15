# virtx for Python

Python bindings for virtx: the same `ConsoleClient`, `ImageClient`, `Recipe`,
`Directory` and host mount as the Rust crate, with every call that waits as an awaitable.

```python
from virtx import ConsoleClient, Directory, HostMount, Recipe

mount = HostMount(Directory().with_file("SKILL.md", "..."), "/tmp/skill")

async with await (
    ConsoleClient.builder()
    .cmd(["virtx-uvm"])
    .image(Recipe("python:3.12-slim-trixie").step("pip install duckdb"))
    .mount_readonly(mount, "/skills/example")
    .mount("./artifacts", "/artifacts")
    .network(False)
    .vcpus(2)
    .memory_mib(2048)
    .build()
) as console:
    result = await console.exec(["sh", "-c", "ls /skills/example"], timeout_ms=10_000)
    print(result.code, result.stdout.decode())
```

A command's output can also be read while it runs. `exec_stream` yields what arrived since the
last chunk, the last one carrying the exit `code`; `spawn` leaves a command running as a `Job`
whose output you poll, while the console takes other calls:

```python
async for chunk in console.exec_stream(["sh", "-c", "make test"], timeout_ms=600_000):
    print(chunk.stdout.decode(), end="")

async with await console.spawn(["python", "-u", "-m", "http.server", "8000"]) as server:
    print((await server.poll(wait_ms=1000)).stdout.decode())  # what it has written so far
    ...  # test against it
# Leaving the block discards the job: killed, and let go of by the server.
```

The server holds a job until the poll that carries its `code`, up to a bound on how many it
holds. A job that should outlive the code that started it is spawned without `async with`, and
`await job.discard()` ends it later; if the `Job` itself was let go of, `console.job(id)` makes
another handle from the id it had.

Breaking out of an `exec_stream` leaves its command running until the stream is closed
(`await stream.aclose()`, or `contextlib.aclosing`) or garbage-collected, which kills it. A
command killed at its `timeout_ms`, by `exec` or a stream, raises `ConsoleRefused` with
`code == ErrorCode.TIMED_OUT` and what it wrote until then in `stdout` and `stderr`.

Without `.cmd(..)` a console runs `virtx-uvm` from the `bin` directory of virtx's cache
(`$VIRTX_HOME`, or the host's cache directory: see [Cache](https://github.com/brekkylab/virtx#cache)).

An image can also be built ahead of the session that runs on it, and then named by its ref or
its digest:

```python
from virtx import ImageClient, ImageSource, Recipe

async with await ImageClient.try_new() as images:
    built = await images.build(Recipe("alpine:3.20").step("apk add jq"), "myimg:latest")
    print(built.reference, built.digest)

    for image in await images.list():
        print(image.digest, image.refs)

console = await ConsoleClient.builder().image(ImageSource.digest(built.digest)).build()
```

A `ConsoleClient` from this package can be handed to an [ailoy](https://github.com/brekkylab/ailoy)
agent as is (`AgentBuilder(...).console(console)`): the agent shares its session, taking turns
with your own calls one at a time, and `close()` ends the session for both.

## Building

The extension is built with [maturin](https://www.maturin.rs). The `mount` feature is on by
default and needs the host's FUSE provider, as the crate does — see the top-level README.

```sh
cd bindings/python
uv sync                        # makes .venv and installs the dev group
uv run maturin develop         # builds the extension into it
uv run pytest
```

`uv run maturin build --release` makes a wheel. The crate's other features pass through:
`maturin develop --features s3,notion`.

The console tests at the end of `tests/test_virtx.py` need a console server, named by
`$VIRTX_CONSOLE`; without one they are skipped.
