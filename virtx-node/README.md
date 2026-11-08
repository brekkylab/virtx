# virtx for Node

A virtx client over the C ABI of `libvirtx_core` (`virtx-core/include/virtx.h`), reached with [koffi](https://koffi.dev): the package has no addon of its own, and the library for its platform comes in that platform's package, `@brekkylab/virtx-<platform>`, which npm installs as an optional dependency.

The package also installs the `virtx` command, the one in `libvirtx_core`: `npx @brekkylab/virtx images`, as the [repository README](../README.md#from-a-shell) describes.

```js
const { Client, Recipe } = require('@brekkylab/virtx')

const client = Client.spawn()
for await (const entry of client.buildImage(new Recipe('alpine:3.20').run('apk add jq'), 'jq:latest')) {
  if ('log' in entry) console.log(entry.log)
  else if ('end' in entry) console.log('ended', entry.end)
}
await client.close()
```

A build, pull or push is a job the server runs. Its request answers a `StartedJob`, which starts the job at the call and is both a promise of the job and an async iterable of its log, entry by entry until its `end`; used both ways, it is still the one job. Awaited, it gives an `ImageJob` that carries its client, whose `log` reads its log again, or from after an entry's `id` to pick it up again. Leaving a loop leaves the job running; its `remove` cancels it. A command in a sandbox is a job of the sandbox's, its output read the same way. Awaited, it is a `SandboxJob` that carries its client: `signal`, `writeStdin`, `log` and `remove` act on it.

```js
const sandbox = await client.createSandbox('alpine:3.20')
const run = sandbox.exec(['echo', 'hi'])
const job = await run
console.log('started', job.id)
for await (const entry of run) {
  if ('stdout' in entry) console.log(entry.stdout)
  else if ('end' in entry) console.log('exited', entry.end.code)
}
await sandbox.remove()
```

```js
const cat = await sandbox.exec(['cat'], { stdin: true })
await cat.writeStdin('hello\n', true)
for await (const entry of cat.log()) console.log(entry)
```

A sandbox's port is reached through a `PortForwarder`, opened over the client with the `url` the sandbox answered for it: an address on this machine's loopback, for any TCP client, until it is closed.

```js
const { PortForwarder } = require('@brekkylab/virtx')

const sandbox = await client.createSandbox('alpine:3.20', { ports: [8000] })
const forwarder = PortForwarder.open(client, sandbox.ports[0].url)
console.log('reach it at', forwarder.address)
forwarder.close()
```

A volume and a snapshot are answered the same way, as a `Volume` and a `Snapshot` that carry their client. Those two and a `Sandbox` read and write their files, with a path relative to their root. A sandbox's are its root as its processes see it, while its VM runs. A file is written and read whole, or read in part; a directory lists its entries and goes with everything in it.

```js
const volume = await client.createVolume('data')
await volume.createDir('in')
await volume.writeFile('in/notes.txt', 'hello')
for (const entry of await volume.readDir('in')) console.log(entry.name, entry.metadata.size)
console.log((await volume.readFile('in/notes.txt')).toString())
```

What acts on a sandbox, a volume, a snapshot or a job is its handle: code that holds only a name or an id gets the handle first, as `await client.getVolume('data')` or `await (await client.getSandbox(id)).job(jobId)`.

The library is the one given to `load(file)`, or `$VIRTX_CORE_LIB`, or a copy beside `index.js`, or the one in the platform's package, or the system loader's search for `libvirtx_core`. One process opens one, so another module sharing a client opens `libraryPath()` and takes the pointer from `client.clone().intoRaw()`.

## Tests

```bash
npm install && npm test
```

The tests build `libvirtx_core` and `virtx-core`'s `test_server` example with cargo first, and run against that server over stdio.
