// The client over `libvirtx_core`: where no server is needed, and against `virtx-core`'s
// `files_server` example over stdio for the files. The library and the example are built with
// cargo first, unless `$VIRTX_CORE_LIB` names a library.

import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import net from 'node:net'
import path from 'node:path'
import { before, test } from 'node:test'
import { fileURLToPath } from 'node:url'

import virtx from '../index.js'

const { Client, PortForwarder } = virtx

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..')
const target = path.join(root, 'target', 'debug')
const library =
  process.platform === 'win32'
    ? 'virtx_core.dll'
    : process.platform === 'darwin'
      ? 'libvirtx_core.dylib'
      : 'libvirtx_core.so'

const filesServer = path.join(target, 'examples', process.platform === 'win32' ? 'files_server.exe' : 'files_server')

before(() => {
  execFileSync('cargo', ['build', '-p', 'virtx-core', '--lib'], { cwd: root, stdio: 'inherit' })
  execFileSync(
    'cargo',
    ['build', '-p', 'virtx-core', '--features', 'testing', '--example', 'files_server'],
    { cwd: root, stdio: 'inherit' },
  )
  virtx.load(process.env[virtx.LIBRARY_ENV] || path.join(target, library))
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
  assert.deepEqual(virtx.imageName('sha256:0'), { type: 'tag', tag: 'sha256:0' })
})

// What reading a connection to `host:port` gets before it ends, or why it could not connect.
function read(host, port) {
  return new Promise((resolve, reject) => {
    const socket = net.connect(port, host)
    let read = ''
    socket.on('data', (data) => (read += data))
    socket.on('end', () => resolve(read))
    socket.on('error', reject)
  })
}

test('a header that cannot be sent is refused at connect', () => {
  assert.throws(() => Client.connect('http://127.0.0.1:9/', null, { 'not a name': 'x' }), /not a name/)
})

test('a token does not go in the clear to another machine', async () => {
  const token = { type: 'bearer', token: 't' }
  assert.throws(() => Client.connect('http://example.com/', token), /in the clear/)
  const client = Client.connect('http://127.0.0.1:9/', token)
  client.setAuth(null)
  client.setAuth(token)
  await client.close()
})

test('a header that cannot be sent is refused when set', async () => {
  const client = Client.connect('http://127.0.0.1:9/')
  client.setHeaders({ authorization: 'Bearer x' })
  assert.throws(() => client.setHeaders({ 'not a name': 'x' }), /not a name/)
  await client.close()
})

test('the files of a volume are written, listed, moved and read back', async () => {
  const client = Client.spawn(filesServer)
  try {
    const volume = { volume: 'v' }
    await client.createDir(volume, 'dir')
    await client.writeFile(volume, 'dir/a.txt', 'hello world')

    const metadata = await client.statFile(volume, 'dir/a.txt')
    assert.equal(metadata.isDir, false)
    assert.equal(metadata.size, 11)
    assert.deepEqual(await client.readDir(volume, 'dir'), [{ name: 'a.txt', metadata }])

    assert.equal((await client.readFile(volume, 'dir/a.txt')).toString(), 'hello world')
    assert.equal((await client.readFile(volume, 'dir/a.txt', 6, 3)).toString(), 'wor')

    await client.renameFile(volume, 'dir/a.txt', 'b.txt')
    await client.copyFile(volume, 'b.txt', 'c.txt')
    await assert.rejects(client.copyFile(volume, 'b.txt', 'c.txt'), /412/)
    await client.removeFile(volume, 'dir')
    await assert.rejects(client.statFile(volume, 'dir'), /404/)
    assert.equal((await client.readFile(volume, 'c.txt')).toString(), 'hello world')

    await assert.rejects(client.readDir({ snapshot: 'locked' }, ''), /423/)
  } finally {
    await client.close()
  }
})

test('a forwarder is on the loopback until it is closed', async () => {
  // Nothing listens there: each connection to the forwarder is closed.
  const client = Client.connect('http://127.0.0.1:9/')
  const forwarder = PortForwarder.open(client, 'ports/abc')
  assert.equal(forwarder.host, '127.0.0.1')
  assert.equal(await read(forwarder.host, forwarder.port), '')

  forwarder.close()
  await client.close()
  await assert.rejects(read(forwarder.host, forwarder.port), /ECONNREFUSED/)
})
