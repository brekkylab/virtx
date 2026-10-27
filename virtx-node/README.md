# virtx for Node

A virtx client over the C ABI of `libvirtx_core` (`virtx-core/include/virtx.h`), reached with [koffi](https://koffi.dev): the package has no addon of its own, and carries the library for its platform beside `index.js`.

```js
const { Client, Recipe } = require('@brekkylab/virtx')

const client = Client.spawn('virtx-server')
for await (const event of client.buildImage(new Recipe('alpine:3.20').run('apk add jq'), 'jq:latest')) {
  if ('progress' in event) console.log(event.progress.content)
  else console.log('built', event.output.digest)
}
await client.close()
```

A sandbox's port is reached through a `PortForwarder`, opened over the client with the `url` the sandbox answered for it: an address on this machine's loopback, for any TCP client, until it is closed.

```js
const { PortForwarder } = require('@brekkylab/virtx')

const sandbox = await client.initSandbox('alpine:3.20', { ports: [8000] })
const forwarder = PortForwarder.open(client, sandbox.ports[0].url)
console.log('reach it at', forwarder.address)
forwarder.close()
```

The library is the one given to `load(file)`, or `$VIRTX_CORE_LIB`, or the copy in the package, or the system loader's search for `libvirtx_core`. One process opens one, so another module sharing a client opens `libraryPath()` and takes the pointer from `client.clone().intoRaw()`.

## Tests

```bash
npm install && npm test
```

The tests build `libvirtx_core` with cargo first.
