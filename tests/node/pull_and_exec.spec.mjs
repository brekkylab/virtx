// The Node package against `virtx-uvm`: the server `Client.spawn()` starts when it names
// none, which `$VIRTX_UVM` points at the one built here. `cargo xtask e2e` sets it, with
// `$VIRTX_CORE_LIB` and `$VIRTX_HOME`.

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import path from 'node:path'
import { before, test } from 'node:test'
import { fileURLToPath } from 'node:url'

import virtx from '../../virtx-node/index.js'

const { Client } = virtx

const IMAGE = 'alpine:3.20'
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..')

before(() => {
  const missing = ['VIRTX_CORE_LIB', 'VIRTX_UVM', 'VIRTX_HOME'].filter((name) => !process.env[name])
  assert.deepEqual(missing, [], `${missing} unset: run these with \`cargo xtask e2e\``)
  virtx.load(process.env[virtx.LIBRARY_ENV])
})

async function entries(log) {
  const read = []
  for await (const { id, ...entry } of log) read.push(entry)
  return read
}

test('a pulled image boots a sandbox that runs a command', async () => {
  const client = Client.spawn()
  try {
    const { end } = (await entries(client.pullImage(IMAGE))).at(-1)
    assert.match(end.digest ?? '', /^sha256:/, end.error)
    assert.ok((await client.listImage()).some((image) => image.digest === end.digest))

    const sandbox = await client.createSandbox(IMAGE)
    try {
      assert.equal(sandbox.running, true)
      assert.equal(sandbox.image, end.digest)
      const log = await entries(client.execSandbox(sandbox.id, ['sh', '-c', 'echo one; echo two >&2; exit 3']))
      // stdout and stderr are read apart, so one's lines may come before the other's.
      assert.deepEqual(
        log.slice(0, -1).sort((a, b) => JSON.stringify(a).localeCompare(JSON.stringify(b))),
        [{ stderr: 'two' }, { stdout: 'one' }],
      )
      assert.deepEqual(log.at(-1), { end: { code: 3 } })
    } finally {
      await client.removeSandbox(sandbox.id)
    }
    await assert.rejects(client.getSandbox(sandbox.id), /404/)
  } finally {
    await client.close()
  }
})

test('the command pulls through the server it starts', () => {
  const pulled = spawnSync(process.execPath, [path.join(root, 'virtx-node', 'cli.js'), 'pull', '-q', IMAGE], {
    encoding: 'utf8',
  })
  assert.equal(pulled.status, 0, pulled.stderr)
  const [reference, digest] = pulled.stdout.split(/\s+/)
  assert.equal(reference, IMAGE)
  assert.match(digest, /^sha256:/)
})
