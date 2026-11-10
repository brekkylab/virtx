# virtx for Python

A virtx client over the C ABI of `libvirtx_core` (`virtx-core/include/virtx.h`), reached with `ctypes`: the package has no extension module of its own, and a wheel carries the library for its platform beside its Python files.

The package also installs the `virtx` command, the one in `libvirtx_core`: `uvx virtx images`.

A build, pull, push or commit of an image, and a process in a sandbox, is a job the server runs apart from the request that started it. The method that starts one answers a `StartedJob`, sent when it is first awaited or iterated: iterated, it yields the job's log, each entry numbered so a reader can resume `after` one, until its end entry; awaited, it is the job. Stopping early leaves the job running; `ImageJob.remove` or `SandboxJob.remove` cancels it.

```python
import asyncio
from virtx import Client, ImageJobEnd, Log, Recipe

async def main():
    with Client.spawn() as client:
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

Awaited, the same call answers the job, an `ImageJob` that carries its client, and its log is read after with its `log`, from its start or `after` an entry: by another reader, or after a reconnect.

```python
job = await client.pull_image("alpine:3.20")
async for entry in job.log(after=1):  # from its second entry
    ...
```

A sandbox runs commands as jobs the same way: `create_sandbox` answers a `Sandbox` that carries its client, and its `exec` starts one, and iterated it yields its output, a line as it is written, then how it ended. Awaited, it is a `SandboxJob` that carries its client too: what signals the process, writes to its stdin and reads its log again. One value awaited and then iterated reads the log of the one job it started.

```python
from virtx import SandboxJobEnd, Stderr, Stdout

sandbox = await client.create_sandbox("alpine:3.20")
run = sandbox.exec(["echo", "hi"])
job = await run
print("started", job.id)
async for entry in run:
    match entry.entry:
        case Stdout(line) | Stderr(line):
            print(line)
        case SandboxJobEnd(code):
            print("exited", code)
await sandbox.remove()
```

```python
cat = await sandbox.exec(["cat"], stdin=True)
await cat.write_stdin("hello\n", eof=True)
async for entry in cat.log():
    ...
```

A sandbox is kept, stopped, once unused, until it is deleted. One the client answered is also an async context manager that deletes it on leaving, so a sandbox for one block is one line; one deleted already in the block is left so.

```python
async with await client.create_sandbox("alpine:3.20") as sandbox:
    await client.exec_sandbox(sandbox.id, ["echo", "hi"])
```

A sandbox's port is reached through a `PortForwarder`, opened over the client with the port `create_sandbox` answered, or `open_port` for one opened after, or its `url`: an address on this machine's loopback, for any TCP client, until it is closed. `PortForwarder.open` blocks; `await PortForwarder.aopen(...)` opens it off the event loop. A connection that reaches nothing in the sandbox -- its WebSocket refused, or closed before the port sent a byte, as when nothing listens there -- is closed, and `forwarder.last_error` says why, a `VirtxError` naming the port, also logged as a warning by the `virtx` logger.

```python
from virtx import PortForwarder

sandbox = await client.create_sandbox("alpine:3.20", ports=[8000])
with await PortForwarder.aopen(client, sandbox.ports[0]) as forwarder:
    print("reach it at", forwarder.address)
port = await sandbox.open_port(9000)  # kept in sandbox.ports too
await sandbox.close_port(9000)  # its url reaches it no more, and it leaves sandbox.ports
```

A port can say what it is for, and be HTTP, reached at its `url` by a browser rather than through a `PortForwarder`: `PortSpec(5173, label="dev server", protocol="http")` in `ports`, beside plain numbers, or `open_port(5173, label="dev server", protocol="http")`. Each answered port has its `label` and `protocol` (`PortProtocol.TCP` unless said).

A TCP port may also have an `address`, `host:port`, from a server reachable from other machines: any TCP client reaches it there directly, without a `PortForwarder`. Unlike the `url` it is not secret, so what is served there must authenticate whoever connects itself. It is `None` when the server gives none.

On virtx cloud, a port has inbound rules: the addresses a TCP port lets in at its `address`, and an HTTP one at its `url`. `PortSpec(22, inbound=[InboundRule("203.0.113.7", "office")])` or `open_port(22, inbound=[...])` sets them as it opens, `await sandbox.set_inbound(22, rules)` replaces them all (`[]` lets in no one), and `await sandbox.inbound(22)` reads them, as a `PortInbound`. A self-hosted server keeps none, and refuses `inbound`.

A volume is answered the same way, as a `Volume` that carries its client. It and a `Sandbox` read and write their files, with a path relative to their root. A sandbox's are its root as its processes see it, while its VM runs. A file is written and read whole, or read in part; a directory lists its entries and goes with everything in it.

```python
volume = await client.create_volume("data")
await volume.create_dir("in")
await volume.write_file("in/notes.txt", b"hello")
for entry in await volume.read_dir("in"):
    print(entry.name, entry.metadata.size)
print(await volume.read_file("in/notes.txt"))
```

What acts on a sandbox, a volume or a sandbox job is its handle. One known only by its id or name is had again with `get_sandbox`, `find_sandbox`, `get_volume` or a sandbox's `job`: `await (await client.get_volume("data")).read_file("in/notes.txt")`.

The library is the one given to `virtx.load(path)`, or `$VIRTX_CORE_LIB`, or the copy in the package, or the system loader's search for `libvirtx_core`. One process opens one, so another module sharing a client opens `virtx.library_path()` and takes the pointer from `client.clone().into_raw()`.

## Tests

```bash
uv run pytest
```

The tests build `libvirtx_core` with cargo first, and run against the real servers, which `cargo xtask fixture` builds and serves over a home of the run's own for them.
They boot machines, so they need `cargo xtask init` and `cargo xtask build kernel` first.
