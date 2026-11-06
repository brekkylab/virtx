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

A build, pull or push is a job the server runs. Its request answers a `StartedJob`, which starts the job at the call and is both a promise of the job and an async iterable of its log, entry by entry until its `end`; used both ways, it is still the one job. Awaited, it gives the job, whose log `imageJobLog` reads again, or from after an entry's `id` to pick it up again. Leaving a loop leaves the job running; `removeImageJob` cancels it. A command in a sandbox is a job of the sandbox's, its output read the same way, or with `sandboxJobLog`.

```js
const sandbox = await client.createSandbox('alpine:3.20')
const run = client.execSandbox(sandbox.id, ['echo', 'hi'])
const job = await run
console.log('started', job.id)
for await (const entry of run) {
  if ('stdout' in entry) console.log(entry.stdout)
  else if ('end' in entry) console.log('exited', entry.end.code)
}
await client.removeSandbox(sandbox.id)
```

A sandbox's port is reached through a `PortForwarder`, opened over the client with the `url` the sandbox answered for it: an address on this machine's loopback, for any TCP client, until it is closed.

```js
const { PortForwarder } = require('@brekkylab/virtx')

const sandbox = await client.createSandbox('alpine:3.20', { ports: [8000] })
const forwarder = PortForwarder.open(client, sandbox.ports[0].url)
console.log('reach it at', forwarder.address)
forwarder.close()
```

The files of a volume, a snapshot or a sandbox are read and written through the client, each request naming its root as `{ volume }`, `{ snapshot }` or `{ sandbox }`, with a path relative to it. A sandbox's are its root as its processes see it, while its VM runs. A file is written and read whole, or read in part; a directory lists its entries and goes with everything in it.

```js
const volume = { volume: 'data' }
await client.createDir(volume, 'in')
await client.writeFile(volume, 'in/notes.txt', 'hello')
for (const entry of await client.readDir(volume, 'in')) console.log(entry.name, entry.metadata.size)
console.log((await client.readFile(volume, 'in/notes.txt')).toString())
```

The library is the one given to `load(file)`, or `$VIRTX_CORE_LIB`, or a copy beside `index.js`, or the one in the platform's package, or the system loader's search for `libvirtx_core`. One process opens one, so another module sharing a client opens `libraryPath()` and takes the pointer from `client.clone().intoRaw()`.

## Tests

```bash
npm install && npm test
```

The tests build `libvirtx_core` and `virtx-core`'s `test_server` example with cargo first, and run against that server over stdio.
