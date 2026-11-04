# virtx for Python

A virtx client over the C ABI of `libvirtx_core` (`virtx-core/include/virtx.h`), reached with `ctypes`: the package has no extension module of its own, and a wheel carries the library for its platform beside its Python files.

A build, pull or push of an image, and a process in a sandbox, is a job the server runs apart from the request that started it. The method that starts one answers a `StartedJob`, sent when it is first awaited or iterated: iterated, it yields the job's log, each entry numbered so a reader can resume `after` one, until its end entry; awaited, it is the job. Stopping early leaves the job running; `remove_image_job` or `remove_sandbox_job` cancels it.

```python
import asyncio
from virtx import Client, ImageJobEnd, Log, Recipe

async def main():
    with Client.spawn("virtx-uvm") as client:
        recipe = Recipe("alpine:3.20").run("apk add jq")
        async for entry in client.build_image(recipe, "jq:latest"):
            match entry.entry:
                case Log(line):
                    print(line)
                case ImageJobEnd(error=None, digest=digest):
                    print("built", digest)
                case ImageJobEnd(error=error):
                    print("failed", error)

asyncio.run(main())
```

Awaited, the same call answers the job, an `ImageJob`, and its log is read after with `image_job_log`, from its start or `after` an entry: by another reader, or after a reconnect.

```python
job = await client.pull_image("alpine:3.20")
async for entry in client.image_job_log(job.id, after=1):  # from its second entry
    ...
```

A sandbox runs commands as jobs the same way: `exec_sandbox` starts one, a `SandboxJob`, and iterated it yields its output, a line as it is written, then how it ended. One value awaited and then iterated reads the log of the one job it started.

```python
from virtx import SandboxJobEnd, Stderr, Stdout

sandbox = await client.create_sandbox("alpine:3.20")
run = client.exec_sandbox(sandbox.id, ["echo", "hi"])
job = await run
print("started", job.id)
async for entry in run:
    match entry.entry:
        case Stdout(line) | Stderr(line):
            print(line)
        case SandboxJobEnd(code):
            print("exited", code)
await client.remove_sandbox(sandbox.id)
```

A sandbox is kept, stopped, once unused, until it is deleted. One the client answered is also an async context manager that deletes it on leaving, so a sandbox for one block is one line; one deleted already in the block is left so.

```python
async with await client.create_sandbox("alpine:3.20") as sandbox:
    await client.exec_sandbox(sandbox.id, ["echo", "hi"])
```

A sandbox's port is reached through a `PortForwarder`, opened over the client with the `url` `create_sandbox` answered for it, or `open_port` for one opened after: an address on this machine's loopback, for any TCP client, until it is closed.

```python
from virtx import PortForwarder

sandbox = await client.create_sandbox("alpine:3.20", ports=[8000])
with PortForwarder.open(client, sandbox.ports[0].url) as forwarder:
    print("reach it at", forwarder.address)
port = await sandbox.open_port(9000)  # kept in sandbox.ports too
```

A port can say what it is for, and be HTTP, reached at its `url` by a browser rather than through a `PortForwarder`: `PortSpec(5173, label="dev server", protocol="http")` in `ports`, beside plain numbers, or `open_port(5173, label="dev server", protocol="http")`. Each answered port has its `label` and `protocol` (`PortProtocol.TCP` unless said).

The files of a volume are read and written through the client, each request naming its root as a `Volume`, with a path relative to it. A file is written and read whole, or read in part; a directory lists its entries and goes with everything in it.

```python
from virtx import Volume

volume = Volume("data")
await client.create_dir(volume, "in")
await client.write_file(volume, "in/notes.txt", b"hello")
for entry in await client.read_dir(volume, "in"):
    print(entry.name, entry.metadata.size)
print(await client.read_file(volume, "in/notes.txt"))
```

The library is the one given to `virtx.load(path)`, or `$VIRTX_CORE_LIB`, or the copy in the package, or the system loader's search for `libvirtx_core`. One process opens one, so another module sharing a client opens `virtx.library_path()` and takes the pointer from `client.clone().into_raw()`.

## Tests

```bash
uv run pytest
```

The tests build `libvirtx_core`, and `virtx-core`'s `test_server` example they spawn, with cargo first.
