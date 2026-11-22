<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/brekkylab/virtx/main/assets/virtx-logo-signature-white.svg">
    <img alt="virtx" src="https://raw.githubusercontent.com/brekkylab/virtx/main/assets/virtx-logo-signature-black.svg" width="352" style="max-width: 50%;">
  </picture>
</p>

<h3 align="center">Disposable Linux VMs for your code and your AI agents.</h3>

<p align="center">
  <img src="https://cdn.simpleicons.org/python" width="16"/> <a href="https://pypi.org/project/virtx/"><img src="https://img.shields.io/pypi/v/virtx?color=blue&label=virtx" alt="PyPI"></a>
  <img src="https://cdn.simpleicons.org/nodedotjs" width="16"/> <a href="https://www.npmjs.com/package/@brekkylab/virtx"><img src="https://img.shields.io/npm/v/@brekkylab/virtx?label=@brekkylab/virtx&color=339933" alt="npm node"></a>
  <img src="https://cdn.simpleicons.org/rust/000000/ffffff" width="16"/> <a href="https://crates.io/crates/virtx"><img src="https://img.shields.io/crates/v/virtx?label=virtx&color=dea584" alt="crates.io"></a>
</p>

<p align="center">
  <a href="https://virtx.sh/docs"><img src="https://img.shields.io/badge/docs-eng-5a9cae" alt="Documentation"></a>
  <a href="https://discord.gg/27rx3EJy3P"><img src="https://img.shields.io/badge/Discord-7289DA?logo=discord&logoColor=white" alt="Discord"></a>
  <a href="https://x.com/brekkylab"><img src="https://img.shields.io/badge/X-000000?logo=x&logoColor=white" alt="X"></a>
</p>

<p align="center">
  <b>English</b> · <a href="https://github.com/brekkylab/virtx/blob/main/README.zh-Hans.md">简体中文</a> · <a href="https://github.com/brekkylab/virtx/blob/main/README.zh-Hant.md">繁體中文</a> · <a href="https://github.com/brekkylab/virtx/blob/main/README.ja.md">日本語</a> · <a href="https://github.com/brekkylab/virtx/blob/main/README.ko.md">한국어</a>
</p>

<br>

virtx lets you run tasks in disposable Linux VMs from your own code.

It's useful for jobs with heavy dependencies that you'd rather not install on your own machine, and for code you'd rather not run on it at all, such as the commands an AI agent decides to run.
Whatever a VM installs or changes is gone when you remove it, unless it wrote it to a volume you kept.

> [!TIP]
> Building an AI agent on virtx? [Ailoy](https://github.com/brekkylab/ailoy), our open-source agent SDK, is built on it and gives every agent a sandbox of its own, so its commands and file tools run in a VM with no setup.

## Quickstart

> [!TIP]
> Using a coding agent? Install the [virtx skill](https://github.com/brekkylab/virtx/blob/main/skills/virtx/SKILL.md) and let it write this for you:
>
> ```bash
> claude plugin marketplace add brekkylab/virtx && claude plugin install virtx@brekkylab
> codex plugin marketplace add brekkylab/virtx && codex plugin add virtx@brekkylab
> ```

Each example builds an image with `jq` in it, boots a sandbox from it, runs a command, and prints its output.

### Local

`Client.spawn()` starts a local server for you.

<details open>
<summary><b>Python</b></summary>

```sh
pip install virtx
```

```python
import asyncio

from virtx import Client, Recipe


async def main() -> None:
    async with Client.spawn() as client:
        recipe = Recipe("alpine:latest").run("apk add --no-cache jq")
        await client.build_image(recipe, "jq:latest").complete(print=True)

        sandbox = await client.create_sandbox("jq:latest")
        try:
            cmd = ["sh", "-c", """echo '{"hello": "virtx"}' | jq -r .hello"""]
            code = await sandbox.exec(cmd).complete(print=True)
            print("exited", code)
        finally:
            await sandbox.remove()


asyncio.run(main())
```

</details>

<details>
<summary><b>Node</b></summary>

```sh
npm install @brekkylab/virtx
```

```js
const { Client, Recipe } = require('@brekkylab/virtx')

async function main() {
  const client = Client.spawn()
  try {
    const recipe = new Recipe('alpine:latest').run('apk add --no-cache jq')
    await client.buildImage(recipe, 'jq:latest').complete({ print: true })

    const sandbox = await client.createSandbox('jq:latest')
    try {
      const cmd = ['sh', '-c', `echo '{"hello": "virtx"}' | jq -r .hello`]
      const code = await sandbox.exec(cmd).complete({ print: true })
      console.log('exited', code)
    } finally {
      await sandbox.remove()
    }
  } finally {
    await client.close()
  }
}

main()
```

</details>

<details>
<summary><b>Rust</b></summary>

```sh
cargo add virtx
cargo add tokio --features macros,rt-multi-thread
```

```rust
use virtx::{Client, Recipe, SandboxCreateArgs, SandboxExecArgs};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::spawn_virtx([] as [&str; 0]).await?;

    let recipe = Recipe::new("alpine:latest").step("apk add --no-cache jq");
    client.build_image(recipe, "jq:latest").complete(true).await?;

    let sandbox = client
        .create_sandbox(&SandboxCreateArgs::new("jq:latest"))
        .await?;
    let cmd = ["sh", "-c", r#"echo '{"hello": "virtx"}' | jq -r .hello"#];
    let code = sandbox.exec(&SandboxExecArgs::new(cmd)).complete(true).await?;
    println!("exited {code}");
    sandbox.remove().await?;
    Ok(())
}
```

</details>

### Remote

Set up `virtx-server-tcp` on a machine of yours, as [Running your own server](https://virtx.sh/docs/tutorials/self-hosting/) shows.

Sign in to it once with the `virtx` command, which `uvx` or `npx` runs without installing, and connect with the token it prints in place of `Client.spawn()`.
The rest is the same as the local example.

```sh
uvx virtx login https://virtx.example                 # pipx run virtx, without uv

# or

npx -y @brekkylab/virtx login https://virtx.example
```

To install `virtx` instead, see [CLI](https://virtx.sh/docs/reference/cli/#installing).

<details open>
<summary><b>Python</b></summary>

```python
from virtx import Client, CommandAuth

client = Client.connect(
    "https://virtx.example",
    CommandAuth("uvx", ("virtx", "token", "--json")),
)
```

</details>

<details>
<summary><b>Node</b></summary>

```js
const client = Client.connect(
  'https://virtx.example',
  { type: 'command', program: 'npx', args: ['-y', '@brekkylab/virtx', 'token', '--json'] }
)
```

</details>

<details>
<summary><b>Rust</b></summary>

```rust
use virtx::Auth;

let client = Client::connect(
    "https://virtx.example",
    Auth::Command { program: "uvx".into(), args: vec!["virtx".into(), "token".into(), "--json".into()] },
    None,
    [] as [(&str, &str); 0],
)?;
```

</details>

## Features

- **Keep untrusted code off your machine:** every sandbox is a microVM with its own kernel, walled off by the hypervisor, not just by namespaces, and its network can be cut entirely.
- **Optimized for building AI agents:** a sandbox is ready for AI agent work, such as RAG, browser use and GUI automation. See the guides for [RAG](https://virtx.sh/docs/guides/implementing-rag/), [browser use](https://virtx.sh/docs/guides/browser/) and [GUI programs](https://virtx.sh/docs/guides/gui-programs/).
- **Bring in outside context:** mount Notion, Google Drive or OneDrive in a sandbox as plain files.
- **Bring the GPU along:** the host's GPU is paravirtualized on Linux, macOS and Windows, so a sandbox runs deep learning models and graphics work on it.
- **Run it wherever you work:** any client works with any server on any host, and moving from a local server to a remote one or the cloud changes one line.
  - **Clients:** <img src="https://cdn.simpleicons.org/python" width="16"/> Python · <img src="https://cdn.simpleicons.org/nodedotjs" width="16"/> Node · <img src="https://cdn.simpleicons.org/rust/000000/ffffff" width="16"/> Rust
  - **Servers:** 💻 local (stdio) · 🖥️ remote (tcp) · ☁️ cloud
  - **Hosts:** <img src="https://cdn.simpleicons.org/linux/000000/ffffff" width="16"/> Linux · <img src="https://cdn.simpleicons.org/apple/000000/ffffff" width="16"/> macOS · <img src="https://cdn.jsdelivr.net/gh/devicons/devicon/icons/windows11/windows11-original.svg" width="16"/> Windows
- **Minimal setup:** install the package and you are ready to run, with no Docker and no daemon to set up.

## Requirements

### Client

Pick the one for the language you use.

- **Python:** 3.11 or later.
- **Node:** 22 or later.
- **Rust:** 1.89 or later.

### Server

These apply to the machine that runs the virtx server.
Guests are always Linux.

#### Hypervisor

- **Linux**: a kernel with KVM support, `/dev/kvm` accessible to your user, and hardware virtualization enabled in the firmware.
  KVM is built into most modern kernels, so nothing usually needs installing.
- **macOS**: Apple silicon only.
  The VM runs on `Hypervisor.framework`, which is built in, so nothing needs installing or turning on.
- **Windows**: Windows 11 or later, x86_64 only.
  The VM runs on the Windows Hypervisor Platform.
  If it is off, run this in an administrator PowerShell and restart:

  ```powershell
  Enable-WindowsOptionalFeature -Online -FeatureName HypervisorPlatform
  ```

#### (Optional) GPU

Only for sandboxes that use the GPU: a Vulkan loader on the host. The guest's half of Vulkan comes with every sandbox, so the image needs none. See [Using GPUs](https://virtx.sh/docs/tutorials/using-gpus/).

## How it works

### Client-server model

virtx follows a client-server model.
Your code, the client, tells a virtx server what to run, and the server runs it in a VM and sends the results back.

<p align="center">
  <img alt="A virtx client reaches a server that is local, on the client machine; remote, on another machine; or in the cloud, hosted by virtx." src="https://raw.githubusercontent.com/brekkylab/virtx/main/docs/images/client-server.svg" width="640">
</p>

The server can be one you run yourself, on the same machine as your code or on another one, or virtx cloud, hosted for you.
Either way, client and server speak the same plain HTTP protocol; only the transport differs.

| | Local | Remote | Cloud |
| --- | --- | --- | --- |
| Server | `virtx-stdio` | `virtx-server-tcp` | virtx cloud |
| Where it runs | Child process of client process | Daemon on another machine, separate from the client | virtx's own machines, hosted for you |
| Transport | HTTP over stdio | HTTP over TCP | HTTPS |
| Setup | None: nothing to install or keep running | Run the server, and give each user a token | Sign in and create an API key |
| Use it for | Running VMs on the machine you work on | A bigger machine, or one host shared by a team | Running VMs without hardware of your own |

Each VM is a libkrun-based microVM: a Linux guest with its own kernel, on the host's hardware virtualization (KVM on Linux, Hypervisor.framework on macOS, the Windows Hypervisor Platform on Windows).
Code in it is separated from your machine by a hypervisor, not just by namespaces as in a container, and a microVM boots in a fraction of the time a full VM takes, since there is no firmware or emulated hardware to bring up.

### Core concepts

The main resource is the **sandbox**: a VM you run commands in.

<p align="center">
  <img alt="A user sends commands to a sandbox and reads its output; the sandbox is made from a writable layer over a read-only image, and a volume is mounted into it." src="https://raw.githubusercontent.com/brekkylab/virtx/main/docs/images/core-concepts.svg" width="640">
</p>

A sandbox's root filesystem is an overlay of two layers: an **image** below, read-only, and a **writable layer** above, where its writes go.

The image is what every sandbox on it starts from.
Images are OCI images, the format Docker uses, so `alpine:latest` or any image from a registry works.

The writable layer is where a sandbox's changes pile up.
Each sandbox has its own, which starts empty and goes when the sandbox is removed.

When a sandbox needs storage apart from its root filesystem, use a **volume**.
Unlike the writable layer, a volume is mounted at a path, read-write or read-only, and can be mounted into several sandboxes at once.
It stays when they are removed, and its files can be read and written from your code without a sandbox at all.
Remote stores such as Notion, Google Drive and OneDrive can be mounted inside a volume too, read-only, so a sandbox sees them as files.

## Documentation

See the [documentation](https://virtx.sh/docs) for guides and the API reference.
Rust's API is on [docs.rs](https://docs.rs/virtx) too.

## Building from source

See [Building from source](https://github.com/brekkylab/virtx/blob/main/AGENTS.md#building-from-source) in AGENTS.md.

## Contributing

Bug reports, ideas and pull requests are welcome.
[CONTRIBUTING.md](https://github.com/brekkylab/virtx/blob/main/CONTRIBUTING.md) covers how to open a PR.
For a suspected vulnerability, please follow [SECURITY.md](https://github.com/brekkylab/virtx/blob/main/SECURITY.md) instead of opening a public issue.
Questions and show-and-tell go on [Discord](https://discord.gg/27rx3EJy3P).

## License

Apache-2.0. See [LICENSE](https://github.com/brekkylab/virtx/blob/main/LICENSE).
