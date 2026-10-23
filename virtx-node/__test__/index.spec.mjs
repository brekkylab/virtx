// The client against `virtx-rs/examples/test_server.rs` over stdio. `libvirtx_core` and the
// server are built with cargo first, unless `$VIRTX_CORE_LIB` names the library.

import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import path from 'node:path'
import { after, before, test } from 'node:test'
import { fileURLToPath } from 'node:url'

import virtx from '../index.js'

const { Client, Recipe, VirtxError } = virtx

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..')
const target = path.join(root, 'target', 'debug')
const server = path.join(target, 'examples', process.platform === 'win32' ? 'test_server.exe' : 'test_server')
const library =
  process.platform === 'win32'
    ? 'virtx_core.dll'
    : process.platform === 'darwin'
      ? 'libvirtx_core.dylib'
      : 'libvirtx_core.so'

let client

before(() => {
  execFileSync('cargo', ['build', '-p', 'virtx-core', '--lib', '-p', 'virtx-rs', '--example', 'test_server'], {
    cwd: root,
    stdio: 'inherit',
  })
  virtx.load(process.env[virtx.LIBRARY_ENV] || path.join(target, library))
  client = Client.spawn(server)
})

after(() => client.close())

async function collect(events) {
  const all = []
  for await (const event of events) all.push(event)
  return all
}

test('a call answers its output', async () => {
  assert.deepEqual(await client.listImage(), [{ reference: 'a:latest', digest: 'sha256:0', size: 1 }])
})

test("a call rejects with the server's error", async () => {
  await assert.rejects(client.removeImage('a:latest'), (e) => e instanceof VirtxError && /no such image/.test(e.message))
})

test('a build yields its log then its output', async () => {
  const events = await collect(client.buildImage(new Recipe('alpine:3.20').run('true'), 'jq:latest'))
  assert.deepEqual(events, [
    { progress: 'pulling alpine:3.20' },
    { output: { reference: 'jq:latest', digest: 'sha256:0' } },
  ])
})

test('a failed build throws after its log', async () => {
  const build = client.buildImage(new Recipe('missing'))
  assert.deepEqual((await build.next()).value, { progress: 'pulling missing' })
  await assert.rejects(build.next(), /no image missing/)
})

test('bytes go and come back as bytes', async () => {
  await client.writeVolumeFile('v', 'f', Buffer.from([0, 0xff, 0x68, 0x69]))
  const data = await client.readVolumeFile('v', 'f')
  assert.ok(data instanceof Uint8Array)
  assert.deepEqual([...data], [0, 0xff, 0x68, 0x69])
  assert.equal(Buffer.from(await client.readVolumeFile('v', 'hello', 1, 3)).toString(), 'ell')
})

test('an exec yields its output then how it ended', async () => {
  const events = await collect(client.execSandbox('s', ['echo', 'hi']))
  assert.deepEqual(events, [{ progress: 'hi' }, { output: { code: 0 } }])
})

test('calls run at once', async () => {
  const lists = await Promise.all(Array.from({ length: 20 }, () => client.listImage()))
  assert.ok(lists.every((images) => images.length === 1))
})

test('leaving a stream early cancels it', async () => {
  for await (const event of client.execSandbox('s', ['echo', 'hi'])) {
    assert.deepEqual(event, { progress: 'hi' })
    break
  }
  assert.equal((await client.listImage()).length, 1)
})

test('a client handed on as a pointer is the same session', async () => {
  const raw = client.clone().intoRaw()
  const taken = Client.fromRaw(raw)
  assert.equal(taken.asRaw(), client.asRaw())
  await taken.close()
  assert.equal((await client.listImage()).length, 1, 'still held')
})

test('a closed client refuses calls', async () => {
  const closed = Client.spawn(server)
  await closed.close()
  await assert.rejects(closed.listImage(), /closed/)
})

test('spawning nothing throws a message', () => {
  assert.throws(() => Client.spawn('/nonexistent/virtx-server'), /starting the server/)
})

test('a second library is refused', () => {
  assert.throws(() => virtx.load('/nonexistent/libvirtx_core'), /is open already/)
  assert.notEqual(virtx.libraryPath(), null)
})

test('an image is named by its spelling', () => {
  const digest = 'sha256:' + '0'.repeat(64)
  assert.deepEqual(virtx.imageName(digest), { type: 'digest', digest })
  assert.deepEqual(virtx.imageName('sha256:0'), { type: 'reference', reference: 'sha256:0' })
})
