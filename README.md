<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/virtx-logo-signature-white.svg">
    <img alt="virtx" src="assets/virtx-logo-signature-black.svg" width="352" style="max-width: 50%;">
  </picture>
</p>

<h3 align="center">Disposable Linux VMs for your code and your AI agents.</h3>

<p align="center">
  <img src="https://cdn.simpleicons.org/python" width="16"/> <a href="https://pypi.org/project/virtx/"><img src="https://img.shields.io/pypi/v/virtx?color=blue&label=virtx" alt="PyPI"></a>
  <img src="https://cdn.simpleicons.org/nodedotjs" width="16"/> <a href="https://www.npmjs.com/package/@brekkylab/virtx"><img src="https://img.shields.io/npm/v/@brekkylab/virtx?label=@brekkylab/virtx&color=339933" alt="npm node"></a>
</p>

<p align="center">
  <a href="https://discord.gg/27rx3EJy3P"><img src="https://img.shields.io/badge/Discord-7289DA?logo=discord&logoColor=white" alt="Discord"></a>
  <a href="https://x.com/brekkylab"><img src="https://img.shields.io/badge/X-000000?logo=x&logoColor=white" alt="X"></a>
</p>

<br>

virtx lets you run tasks in disposable Linux VMs from your own code.

It's useful for jobs with heavy dependencies that you'd rather not install on your own machine, and for code you'd rather not run on it at all, such as the commands an AI agent decides to run.
Whatever a VM installs or changes is gone when you remove it, unless it wrote it to a snapshot or a volume you kept.

virtx is also the VM layer under [Ailoy](https://github.com/brekkylab/ailoy), our open-source agent SDK. In Ailoy, every agent gets its own virtx VM as its sandbox: the agent runs on your host, and its commands and file tools run inside the VM. You can use virtx on its own too, with any agent or none at all.

## How it works

A virtx server runs the VMs, on your machine or another one. This repository is both ends: the client, one library, `libvirtx_core`, with packages for Python, Node and Rust over it, and the server, [`virtx-stdio`](virtx-stdio/README.md).

Your code asks the server for a sandbox, a VM booted from an image you name. It then runs commands in it as jobs and reads their output as a log. Volumes and snapshots hold whatever should outlive the sandbox, and anything else is gone when you remove it.

Client and server speak plain HTTP, described in [`virtx-core/PROTOCOL.md`](virtx-core/PROTOCOL.md). The client either starts the server as a child process and talks to it over stdin and stdout, so there is nothing to install or keep running, or connects to one by URL.

## Requirements

These apply to the machine that runs the virtx server.

- **Host:** Linux, macOS on Apple silicon, or Windows 11 or later. Guests are always Linux.
- **Linux:** a kernel with KVM support, `/dev/kvm` accessible to your user, hardware virtualization enabled in the firmware, and FUSE. KVM is built into most modern kernels, so nothing usually needs installing.
- **Windows:** the VM runs on the Windows Hypervisor Platform, which is off by default. Turn it on from an administrator PowerShell, then restart, with virtualization enabled in the firmware:

  ```powershell
  Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
  ```

- **Clients:** Python 3.10 or later, Node 18 or later, or Rust.
- **GPU:** a Vulkan loader on the host, plus the guest's half of Vulkan in the image. See [GPU](#gpu).

## Quickstart

Each one builds an image with `jq` in it, boots a sandbox from it, runs a command, and reads what it wrote.
`Client.spawn()` starts the server, `virtx-stdio`, and talks to it over its stdin and stdout; `Client.connect(url)` reaches one over HTTP instead.
The server is released with the client, so a client fetches the one of its own version the first time it is asked for, from this repository's releases into a cache, and `Client.spawn("virtx-stdio")` runs one you installed yourself instead.
To build it, as [`CONTRIBUTING.md`](CONTRIBUTING.md) says, and have a client use that build:

```sh
cargo xtask init
cargo xtask build kernel server-stdio
cargo xtask install --prefix ~/.local   # -> ~/.local/bin/virtx-stdio, beside its kernel
export VIRTX_STDIO=~/.local/bin/virtx-stdio  # what Client.spawn() starts, in place of a fetch
```

### Python

```sh
pip install virtx
```

```python
import asyncio

from virtx import Client, ImageJobEnd, Log, Recipe, SandboxJobEnd, Stderr, Stdout


async def main() -> None:
    with Client.spawn() as client:
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
            async for entry in sandbox.exec(cmd):
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
  const client = Client.spawn()
  try {
    const recipe = new Recipe('alpine:latest').run('apk add --no-cache jq')
    for await (const entry of client.buildImage(recipe, 'jq:latest')) {
      if ('log' in entry) console.log(entry.log)
      else if ('end' in entry && entry.end.error) throw new Error(entry.end.error)
    }

    const sandbox = await client.createSandbox('jq:latest')
    try {
      const cmd = ['sh', '-c', `echo '{"hello": "virtx"}' | jq -r .hello`]
      for await (const entry of sandbox.exec(cmd)) {
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
    let client = Client::spawn_virtx([] as [&str; 0])?;

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
    let mut log = sandbox.exec(&SandboxExecArgs::new(cmd)).log();
    while let Some(entry) = log.wait_next() {
        match entry?.entry {
            SandboxJobEntry::Stdout(line) | SandboxJobEntry::Stderr(line) => println!("{line}"),
            SandboxJobEntry::End(end) => println!("exited {}", end.code),
            _ => {}
        }
    }
    sandbox.remove().wait()?;
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
| **Image** | What a sandbox boots: a manifest and its layers, under a tag such as `jq:latest` | `build_image`, `pull_image`, `push_image`; listed, tagged, removed, downloaded and uploaded through the server's OCI registry |
| **Sandbox** | A VM booted from an image, on a snapshot if given, with volumes mounted into it | `create_sandbox`, `get_sandbox`, `list_sandbox`, `remove_sandbox`, `exec_sandbox`; its files over WebDAV |
| **Snapshot** | The writable layer of a sandbox's root filesystem, kept under a name | `create_snapshot`, `get_snapshot`, `list_snapshot`, `remove_snapshot`; its files over WebDAV |
| **Volume** | Storage kept under a name, mounted into sandboxes, with remote sources such as Notion or Google Drive mounted inside it | `create_volume`, `get_volume`, `list_volume`, `remove_volume`, `*_volume_mount`; its files over WebDAV |

Names here are Python's and Rust's; Node's are the same in camelCase, such as `createSandbox`.
A sandbox, a snapshot and a volume are answered as a `Sandbox`, a `Snapshot` and a `Volume` that carry their client, so each is all a caller hands on, and `remove` removes it:

- All three are `Files`: `stat_file`, `exists`, `is_file`, `is_dir`, `read_dir`, `read_file`, `read_text`, `write_file`, `create_dir`, `create_dir_all`, `remove_file`, `rename_file` and `copy_file` act on their files, and `root` names those files for the client's own files requests. In Rust, `use virtx_rs::Files` brings them into scope.
- A `Sandbox` runs a process with `exec`, and lists its jobs with `jobs` and gets one with `job`.
- A `Volume` lists its mounts with `mounts`, and adds one with `mount` and takes it away with `unmount`.
- A process in a sandbox is a `SandboxJob` that carries its client and sandbox: `get`, `signal`, `write_stdin`, `log` and `remove` act on it.

Building, pulling and pushing an image, and running a command in a sandbox, start a [job](#jobs-and-their-logs).

## Jobs and their logs

A build, pull or push, and every command in a sandbox, is a job the server runs apart from the request that started it.
`build_image`, `pull_image`, `push_image` and `exec_sandbox` answer a started job, which is two things at once:

- Iterated (`async for` in Python, `for await` in Node, `.log()` in Rust), it is the job's log, an entry at a time until its last, `end`, which says how it ended: an image's digest or why it failed, a process's exit code.
- Awaited (`.wait()` in Rust, too), it is the job itself, whose `id` names it to every other request.

One value awaited and iterated starts one job. The job carries its client, and its log can be read again any time with its `log`.

- Every entry has an `id`, its number in the log. A reader whose stream dropped asks for the log `after` the last one it saw, and picks up there.
- Any number of readers read one log, each from its start or from wherever they ask.
- A log keeps a limit's worth of entries. Past it, one `truncated` entry stands for what was dropped, and `end` still comes.
- A process's output is a line per entry, `stdout` or `stderr`, without its line ending.
- Leaving the loop leaves the job running. The job's `remove` cancels one that runs, killing a process and its children, and forgets it.

A process started with `stdin` takes what its job's `write_stdin` writes, the next write sent once the one before is answered, until one closes it with `eof`. Its `signal` sends it a signal such as `SIGTERM`, and `timeout_ms` kills it when it runs out.

```python
run = sandbox.exec(["cat"], stdin=True)
job = await run
await job.write_stdin("hello\n", eof=True)
async for entry in run:
    print(entry)
```

## Snapshots and volumes

A sandbox made on a snapshot starts with the changes in it and writes into it, so what one sandbox installed the next one finds.
While a sandbox holds it, its files are the sandbox's: reading them, and removing the snapshot, are refused until the sandbox is removed.

```python
snapshot = await client.create_snapshot("dev")
sandbox = await client.create_sandbox("jq:latest", snapshot=snapshot)
```

A volume is mounted into any number of sandboxes at the paths you choose, read-only if you say so, and stays when they go. A snapshot or a volume is named by its handle or its name; in Rust, `SandboxCreateArgs::new("jq:latest").snapshot(snapshot.name()).readonly_volume(volume.name(), "/data")`.

```python
from virtx import SandboxVolume

volume = await client.create_volume("data")
sandbox = await client.create_sandbox(
    "jq:latest", volumes=[SandboxVolume(volume, "/data", readonly=False)]
)
```

The files of a volume or a snapshot are read and written from the client, without a sandbox, through the volume or snapshot, with a path in it. A sandbox's own, its root as its processes see it, are reached the same way while its VM runs, as `sandbox.read_text("etc/hostname")`:

```python
volume = await client.get_volume("data")
await volume.create_dir_all("in")
await volume.write_file("in/notes.txt", b"ship the release")
for entry in await volume.read_dir("in"):
    print(entry.name, entry.metadata.size)
print(await volume.read_text("in/notes.txt"))
```

The client's own files requests take the root instead: `client.read_text(volume.root, "in/notes.txt")`, or `VolumeFiles("data")` in Python and `{ volume: 'data' }` in Node for a name alone.

A write can name the `etag` a read gave, so it goes only over the file it read. Files move with `rename_file` and `copy_file`, and go with `remove_file`.

## Ports

`ports` at making a sandbox open TCP ports in it, each answered with a URL, there only. A `PortForwarder` opened over the client with that URL puts the port at an address on this machine's loopback, for any TCP client:

```python
from virtx import PortForwarder

sandbox = await client.create_sandbox("jq:latest", ports=[8000])
with PortForwarder.open(client, sandbox.ports[0].url) as forwarder:
    print("reach it at", forwarder.address)
```

Ports are the way in. `network=False` takes away the way out.

## Images

A recipe is a base image and the steps over it, as a Dockerfile is; in Rust, `Recipe::from_dockerfile` reads one.
A build stores what it builds under its tag, and `create_sandbox` boots it by that tag, or by its digest for exactly that build.
`pull_image` and `push_image` copy an image between the server and a registry such as Docker Hub or `ghcr.io`, with that registry's credentials if it wants them.

Everything else is the server's OCI registry: `list_image`, `tag_image`, `untag_image` and `remove_image`, and `download_image` and `upload_image` to move an image to and from an OCI archive on this machine. `prune_image` deletes what no tag points at and no sandbox uses.

### From a shell

The `virtx` command does the same to a server's images with the commands `docker` has for them: `build`, `images`, `inspect`, `tag`, `rmi`, `pull`, `push`, `save`, `load`, each also as `virtx image <command>`, and `virtx image prune`. The Python and Node packages install it, as `libvirtx_core` carries it, and [`virtx-cli`](virtx-cli/) builds it as a binary of its own.

```sh
uvx virtx images                               # or `pip install virtx`
npx @brekkylab/virtx images                    # or `npm install -g @brekkylab/virtx`
cargo install --path virtx-cli

virtx build -t jq:latest .                     # ./Dockerfile; the log on stderr, `jq:latest sha256:…` on stdout
virtx images
virtx tag jq:latest ghcr.io/me/jq:1
echo "$GHCR_TOKEN" | virtx push -u me ghcr.io/me/jq:1
virtx save -o jq.tar jq:latest
virtx rmi jq:latest
```

It starts `virtx-stdio` of its own version, fetched into the cache the first time, or the program `--server` (`$VIRTX_STDIO`) names; `--host URL` reaches a server over HTTP instead, with `$VIRTX_TOKEN` as its bearer token. Ctrl-C during a build, pull or push cancels the job.

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

## Contributing

Bug reports, ideas and pull requests are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers setup, tests and how to open a PR. For a suspected vulnerability, please follow [SECURITY.md](SECURITY.md) instead of opening a public issue. Questions and show-and-tell go on [Discord](https://discord.gg/27rx3EJy3P).

## License

Apache-2.0. See [LICENSE](LICENSE).
