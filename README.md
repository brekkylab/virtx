# virtx

<img src="https://cdn.simpleicons.org/python" width="16"/> <a href="https://pypi.org/project/virtx/"><img src="https://img.shields.io/pypi/v/virtx?color=blue&label=virtx" alt="PyPI"></a>
<img src="https://cdn.simpleicons.org/nodedotjs" width="16"/> <a href="https://www.npmjs.com/package/@brekkylab/virtx"><img src="https://img.shields.io/npm/v/@brekkylab/virtx?label=@brekkylab/virtx&color=339933" alt="npm node"></a>

virtx lets you run tasks in disposable Linux VMs from your own code.

It's useful for jobs with heavy dependencies that you'd rather not install on your own machine, and for code you'd rather not run on it at all.
Whatever a VM installs or changes is kept while the sandbox is, across its stops, and gone when you remove it -- unless it wrote it to a volume, or you committed it to an image.

A virtx server runs the VMs, on your machine or another one, and this repository is the client: one library, `libvirtx_core`, with packages for Python, Node and Rust over it.
What client and server say to each other is [`virtx-core/PROTOCOL.md`](virtx-core/PROTOCOL.md): plain HTTP, over TCP or over a server the client starts as a child process.

## Quickstart

Each one builds an image with `jq` in it, boots a sandbox from it, runs a command, and reads what it wrote.
`Client.spawn("virtx-uvm")` starts the server, [`virtx-uvm`](https://github.com/brekkylab/virtx-uvm), and talks to it over its stdin and stdout; `Client.connect(url)` reaches one over HTTP instead.
No release of `virtx-uvm` speaks this protocol yet, so build it from that repository's `multi-process` branch, as its README says, and install it onto your `PATH`:

```sh
cargo xtask install --prefix ~/.local   # in virtx-uvm, once built: -> ~/.local/bin/virtx-uvm
```

### Python

```sh
pip install virtx
```

```python
import asyncio

from virtx import Client, ImageJobEnd, Log, Recipe, SandboxJobEnd, Stderr, Stdout


async def main() -> None:
    with Client.spawn("virtx-uvm") as client:
        recipe = Recipe("alpine:latest").run("apk add --no-cache jq")
        async for entry in client.build_image(recipe, "jq:latest"):
            match entry.entry:
                case Log(line):
                    print(line)
                case ImageJobEnd(error=str(error)):
                    raise RuntimeError(error)

        sandbox = await client.create_sandbox("jq:latest")
        try:
            cmd = ["sh", "-c", """echo '{"hello": "virtx"}' | jq -r .hello"""]
            async for entry in client.exec_sandbox(sandbox.id, cmd):
                match entry.entry:
                    case Stdout(line) | Stderr(line):
                        print(line)
                    case SandboxJobEnd(code):
                        print("exited", code)
        finally:
            await client.remove_sandbox(sandbox.id)


asyncio.run(main())
```

### Node

```sh
npm install @brekkylab/virtx
```

```js
const { Client, Recipe } = require('@brekkylab/virtx')

async function main() {
  const client = Client.spawn('virtx-uvm')
  try {
    const recipe = new Recipe('alpine:latest').run('apk add --no-cache jq')
    for await (const entry of client.buildImage(recipe, 'jq:latest')) {
      if ('log' in entry) console.log(entry.log)
      else if ('end' in entry && entry.end.error) throw new Error(entry.end.error)
    }

    const sandbox = await client.createSandbox('jq:latest')
    try {
      const cmd = ['sh', '-c', `echo '{"hello": "virtx"}' | jq -r .hello`]
      for await (const entry of client.execSandbox(sandbox.id, cmd)) {
        if ('stdout' in entry) console.log(entry.stdout)
        else if ('stderr' in entry) console.error(entry.stderr)
        else if ('end' in entry) console.log('exited', entry.end.code)
      }
    } finally {
      await client.removeSandbox(sandbox.id)
    }
  } finally {
    await client.close()
  }
}

main()
```

### Rust

The Rust package is `virtx-rs`, not on crates.io yet:

```toml
[dependencies]
virtx-rs = { git = "https://github.com/brekkylab/virtx" }
```

It opens `libvirtx_core` at run time, from `virtx_rs::load(path)` or `$VIRTX_CORE_LIB`, so every module in a process shares one client.
Each request is a `Future` or a `Stream` any async runtime awaits, or blocks with `wait` and `wait_next` on a thread without one, as here:

```rust
use virtx_rs::{
    Client, ImageJobEntry, Recipe, SandboxCreateArgs, SandboxExecArgs, SandboxJobEntry,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::spawn("virtx-uvm", [] as [&str; 0])?;

    let recipe = Recipe::new("alpine:latest").step("apk add --no-cache jq");
    let mut log = client.build_image(recipe, "jq:latest").log();
    while let Some(entry) = log.wait_next() {
        match entry?.entry {
            ImageJobEntry::Log(line) => println!("{line}"),
            ImageJobEntry::End(end) if end.error.is_some() => Err(end.error.unwrap())?,
            _ => {}
        }
    }

    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new("jq:latest"))
        .wait()?;
    let cmd = ["sh", "-c", r#"echo '{"hello": "virtx"}' | jq -r .hello"#];
    let mut log = client
        .exec_sandbox(&sandbox.id, &SandboxExecArgs::new(cmd))
        .log();
    while let Some(entry) = log.wait_next() {
        match entry?.entry {
            SandboxJobEntry::Stdout(line) | SandboxJobEntry::Stderr(line) => println!("{line}"),
            SandboxJobEntry::End(end) => println!("exited {}", end.code),
            _ => {}
        }
    }
    client.remove_sandbox(&sandbox.id).wait()?;
    Ok(())
}
```

Or let a coding agent do it: this repo is its own plugin marketplace, so the [agent skill](skills/virtx/SKILL.md) installs with no clone first.

```bash
claude plugin marketplace add brekkylab/virtx && claude plugin install virtx@brekkylab
codex plugin marketplace add brekkylab/virtx && codex plugin add virtx@brekkylab
```

## Resources

| | What it is | Made and read with |
|---|---|---|
| **Image** | What a sandbox boots: a manifest and its layers, under a tag such as `jq:latest` | `build_image`, `pull_image`, `push_image`, `commit_image` (of a sandbox); listed, tagged, removed, downloaded and uploaded through the server's OCI registry |
| **Sandbox** | A VM booted from an image, with volumes mounted into it and a disk of its own: it stops when unused and starts again as it was left, until it is deleted | `create_sandbox`, `get_sandbox`, `find_sandbox`, `list_sandbox`, `start_sandbox`, `stop_sandbox`, `remove_sandbox`, `exec_sandbox` |
| **Volume** | Storage kept under a name, mounted into sandboxes, with remote sources such as Notion or Google Drive mounted inside it | `create_volume`, `get_volume`, `list_volume`, `remove_volume`, `*_volume_mount`; its files over WebDAV |

Names here are Python's and Rust's; Node's are the same in camelCase, such as `createSandbox`.
Building, pulling, pushing and committing an image, and running a command in a sandbox, start a [job](#jobs-and-their-logs).

## Jobs and their logs

A build, pull or push, and every command in a sandbox, is a job the server runs apart from the request that started it.
`build_image`, `pull_image`, `push_image` and `exec_sandbox` answer a started job, which is two things at once:

- Iterated (`async for` in Python, `for await` in Node, `.log()` in Rust), it is the job's log, an entry at a time until its last, `end`, which says how it ended: an image's digest or why it failed, a process's exit code.
- Awaited (`.wait()` in Rust, too), it is the job itself, whose `id` names it to every other request.

One value awaited and iterated starts one job. Its log can be read again any time with `image_job_log` or `sandbox_job_log`.

- Every entry has an `id`, its number in the log. A reader whose stream dropped asks for the log `after` the last one it saw, and picks up there.
- Any number of readers read one log, each from its start or from wherever they ask.
- A log keeps a limit's worth of entries. Past it, one `truncated` entry stands for what was dropped, and `end` still comes.
- A process's output is a line per entry, `stdout` or `stderr`, without its line ending.
- Leaving the loop leaves the job running. `remove_image_job` and `remove_sandbox_job` cancel one that runs, killing a process and its children, and forget it.

A process started with `stdin` takes what `write_sandbox_job_stdin` writes, the next write sent once the one before is answered, until one closes it with `eof`. `signal_sandbox_job` sends it a signal such as `SIGTERM`, and `timeout_ms` kills it when it runs out.

```python
run = client.exec_sandbox(sandbox.id, ["cat"], stdin=True)
job = await run
await client.write_sandbox_job_stdin(sandbox.id, job.id, "hello\n", eof=True)
async for entry in run:
    print(entry)
```

## What a sandbox keeps, and volumes

A sandbox keeps what it writes. It stops after `auto_stop_minutes` unused, or at `stop_sandbox`, and its disk is kept; a command started in it, or `start_sandbox`, starts it again as it was left, under the same id -- its files, not its processes. It is deleted at `remove_sandbox`, when it stops if made `ephemeral`, or after `auto_delete_days` stopped and unused. A `name` finds it again without keeping its id.

```python
sandbox = await client.create_sandbox("jq:latest", name="dev")
...
sandbox = await client.find_sandbox("dev")  # tomorrow: what it installed is there
```

A sandbox used for one block only is deleted on leaving it: `async with await client.create_sandbox(...) as sandbox:` in Python, `await using sandbox = await client.createSandbox(...)` in Node.

To start others from what a sandbox made, commit it to an image: its image, with what it wrote as one more layer, which any number of sandboxes boot.

```python
await client.commit_image(sandbox.id, "dev:v1")
```

A volume is mounted into any number of sandboxes at the paths you choose, read-only if you say so, and stays when they go.

```python
from virtx import SandboxVolume

await client.create_volume("data")
sandbox = await client.create_sandbox(
    "jq:latest", volumes=[SandboxVolume(volume="data", path="/data", readonly=False)]
)
```

The files of a volume are read and written from the client, without a sandbox, each request naming its root and a path in it:

```python
from virtx import Volume

volume = Volume("data")
await client.create_dir_all(volume, "in")
await client.write_file(volume, "in/notes.txt", b"ship the release")
for entry in await client.read_dir(volume, "in"):
    print(entry.name, entry.metadata.size)
print(await client.read_text(volume, "in/notes.txt"))
```

A write can name the `etag` a read gave, so it goes only over the file it read. Files move with `rename_file` and `copy_file`, and go with `remove_file`.

## Ports

`ports` at making a sandbox open TCP ports in it, each answered with a URL, there and at `start_sandbox` only; `sandbox.open_port(port)` opens one later, answering its URL the same way. A `PortForwarder` opened over the client with that URL puts the port at an address on this machine's loopback, for any TCP client:

```python
from virtx import PortForwarder

sandbox = await client.create_sandbox("jq:latest", ports=[8000])
with PortForwarder.open(client, sandbox.ports[0].url) as forwarder:
    print("reach it at", forwarder.address)
```

A port can say what it is for, and be HTTP rather than TCP: `PortSpec(5173, label="dev server", protocol="http")` in `ports`, or `open_port(5173, label=..., protocol="http")`, is answered with a URL a browser opens, from a server reachable from other machines an `https` one, rather than one for a `PortForwarder`. `ports` mixes the two, `[5900, PortSpec(5173, protocol="http")]`; Node takes `{ port, label, protocol }` there, and Rust a `PortSpec`, `8080.into()` for a plain one.

`sandbox.close_port(port)` closes one, dropping it from `sandbox.ports` -- `closePort` in Node, and `close_sandbox_port(id, port)` on the client, `closeSandboxPort` in Node and the only way in Rust: its URL reaches it no more, and it is not opened again as the sandbox starts. A TCP port is made HTTP by closing it and opening it again, at another URL.

A server reachable from other machines may also answer a TCP port with an `address`, `host:port`, where any TCP client reaches it directly, without a `PortForwarder`; one on this machine, and an HTTP port, have none. Unlike the URL it is not secret, so what is served there must authenticate whoever connects itself.

Ports are the way in. `network=False` takes away the way out.

## Images

A recipe is a base image and the steps over it, as a Dockerfile is; in Rust, `Recipe::from_dockerfile` reads one.
A build stores what it builds under its tag, and `create_sandbox` boots it by that tag, or by its digest for exactly that build.
`pull_image` and `push_image` copy an image between the server and a registry such as Docker Hub or `ghcr.io`, with that registry's credentials if it wants them.

Everything else is the server's OCI registry: `list_image`, `tag_image`, `untag_image` and `remove_image`, and `download_image` and `upload_image` to move an image to and from an OCI archive on this machine. `prune_image` deletes what no tag points at and no sandbox uses.

## GPU

The VM gets a GPU through Vulkan on every host, so it can run heavy work like deep learning.
Ask for it with `gpu=True` at making a sandbox, `gpu_memory_mib` for its memory, and install the guest's half of Vulkan in the image:

```python
recipe = Recipe("alpine:latest").run(
    # The Vulkan loader, the venus driver that reaches the host's GPU, and vulkaninfo.
    "apk add --no-cache vulkan-loader mesa-vulkan-virtio vulkan-tools"
)
async for entry in client.build_image(recipe, "vulkan:latest"):
    print(entry.entry)
sandbox = await client.create_sandbox("vulkan:latest", gpu=True, gpu_memory_mib=8192)
async for entry in client.exec_sandbox(sandbox.id, ["vulkaninfo", "--summary"]):
    print(entry.entry)
```

The host's GPU shows up in the guest as `Virtio-GPU Venus (<the host's GPU>)`. What the image needs for that:

| | Alpine | Debian |
|---|---|---|
| Vulkan loader (`libvulkan.so.1`) | `vulkan-loader` | `libvulkan1` |
| venus driver | `mesa-vulkan-virtio` | `mesa-vulkan-drivers`, trixie or later |

- **Install the loader yourself.** Without it, no program in the guest finds a Vulkan device, whatever the host has. Some packages bring it along (Debian's `vulkan-tools` does) and some don't (Alpine's doesn't), so name it rather than count on it.
- **An image with no venus driver still runs, but on the CPU.** Debian bookworm's Mesa has no venus, and Vulkan there falls back to `llvmpipe`, a software renderer. Check the `deviceName` your program picks.

A server that can't give a GPU refuses the sandbox rather than run it on the CPU.

## Coverage

| | Supported |
|---|---|
| **Languages** | <img src="https://cdn.simpleicons.org/python" width="16"/> Python · <img src="https://cdn.simpleicons.org/nodedotjs" width="16"/> Node · <img src="https://cdn.simpleicons.org/rust/000000/ffffff" width="16"/> Rust |
| **Hosts** | <img src="https://cdn.simpleicons.org/linux/000000/ffffff" width="16"/> Linux · <img src="https://cdn.simpleicons.org/apple/000000/ffffff" width="16"/> macOS (Apple silicon) · <img src="https://cdn.jsdelivr.net/gh/devicons/devicon/icons/windows11/windows11-original.svg" width="16"/> Windows 11 or later |
| **Guest** | <img src="https://cdn.simpleicons.org/linux/000000/ffffff" width="16"/> Linux, always |

Each package's own README has the rest of its API: [Python](virtx-py/README.md), [Node](virtx-node/README.md).
