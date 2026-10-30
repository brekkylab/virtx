# virtx for Python

A virtx client over the C ABI of `libvirtx_core` (`virtx-core/include/virtx.h`), reached with `ctypes`: the package has no extension module of its own, and a wheel carries the library for its platform beside its Python files.

```python
import asyncio
from virtx import Client, Output, Progress, Recipe

async def main():
    with Client.spawn("virtx-server") as client:
        async for event in client.build_image(Recipe("alpine:3.20").run("apk add jq"), "jq:latest"):
            match event:
                case Progress(content=line):
                    print(line)
                case Output(built):
                    print("built", built.digest)

asyncio.run(main())
```

A sandbox's port is reached through a `PortForwarder`, opened over the client with the `url` the sandbox answered for it: an address on this machine's loopback, for any TCP client, until it is closed.

```python
from virtx import PortForwarder

sandbox = await client.init_sandbox("alpine:3.20", ports=[8000])
with PortForwarder.open(client, sandbox["ports"][0]["url"]) as forwarder:
    print("reach it at", forwarder.address)
```

The files of a volume or a snapshot are read and written through the client, each request naming its root as a `Volume` or a `Snapshot`, with a path relative to it. A file is written and read whole, or read in part; a directory lists its entries and goes with everything in it.

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

The tests build `libvirtx_core` with cargo first.
