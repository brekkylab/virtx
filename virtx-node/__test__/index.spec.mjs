// The client over `libvirtx_core`: where no server is needed, and against `virtx-core`'s
// `test_server` example over stdio for the rest. The library and the example are built with
// cargo first, unless `$VIRTX_CORE_LIB` names a library.

import assert from 'node:assert/strict'
import { execFileSync, spawnSync } from 'node:child_process'
import fs from 'node:fs'
import net from 'node:net'
import os from 'node:os'
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

const testServer = path.join(target, 'examples', process.platform === 'win32' ? 'test_server.exe' : 'test_server')

before(() => {
  execFileSync('cargo', ['build', '-p', 'virtx-core', '--lib'], { cwd: root, stdio: 'inherit' })
  execFileSync(
    'cargo',
    ['build', '-p', 'virtx-core', '--features', 'testing', '--example', 'test_server'],
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
  const client = Client.spawn(testServer)
  try {
    const volume = await client.getVolume('v')
    await volume.createDir('dir')
    await volume.writeFile('dir/a.txt', 'hello world')
    await volume.createDirAll('deep/er/dir')
    await volume.createDirAll('deep/er/dir')
    assert.equal((await volume.statFile('deep/er/dir')).isDir, true)

    const metadata = await volume.statFile('dir/a.txt')
    assert.equal(metadata.isDir, false)
    assert.equal(metadata.size, 11)
    assert.deepEqual(await volume.readDir('dir'), [{ name: 'a.txt', metadata }])

    assert.equal((await volume.readFile('dir/a.txt')).toString(), 'hello world')
    assert.equal((await volume.readFile('dir/a.txt', 6, 3)).toString(), 'wor')

    // Text, an etag to write over, and what is there, told without an error.
    await volume.writeFile('dir/t.txt', '텍스트')
    assert.equal(await volume.readText('dir/t.txt'), '텍스트')
    const { etag } = await volume.statFile('dir/t.txt')
    assert.ok(etag)
    await volume.writeFile('dir/t.txt', 'text 2', etag)
    await assert.rejects(volume.writeFile('dir/t.txt', 'text 3', etag), { status: 412 })
    assert.equal(await volume.exists('dir/t.txt'), true)
    assert.equal(await volume.isFile('dir/t.txt'), true)
    assert.equal(await volume.isDir('dir/t.txt'), false)
    assert.equal(await volume.isDir('dir'), true)
    await volume.removeFile('dir/t.txt')
    assert.equal(await volume.exists('dir/t.txt'), false)
    await assert.rejects(volume.removeFile('dir/t.txt'), { status: 404 })
    await volume.removeFile('dir/t.txt', true)

    await volume.renameFile('dir/a.txt', 'b.txt')
    await volume.copyFile('b.txt', 'c.txt')
    await assert.rejects(volume.copyFile('b.txt', 'c.txt'), { status: 412 })
    await volume.removeFile('dir')
    await assert.rejects(volume.statFile('dir'), { status: 404 })
    assert.equal((await volume.readFile('c.txt')).toString(), 'hello world')

    await assert.rejects((await client.getSnapshot('locked')).readDir(''), { status: 423 })
  } finally {
    await client.close()
  }
})

test("a sandbox's files are its own and go with it", async () => {
  const client = Client.spawn(testServer)
  try {
    const sandbox = await client.createSandbox('alpine:3.20')
    await sandbox.createDirAll('work/dir')
    await sandbox.writeFile('work/dir/a.txt', 'hello')
    assert.equal(await sandbox.readText('work/dir/a.txt'), 'hello')
    assert.deepEqual(await sandbox.readFile('work/dir/a.txt', 1, 3), Buffer.from('ell'))
    assert.deepEqual(
      (await (await client.getSandbox(sandbox.id)).readDir('work/dir')).map((entry) => entry.name),
      ['a.txt'],
    )
    await sandbox.removeFile('work/dir/a.txt')
    await sandbox.remove()
    await assert.rejects(client.getSandbox(sandbox.id), { status: 404 })
    await assert.rejects(sandbox.readFile(''), { status: 404 })
  } finally {
    await client.close()
  }
})

test('a volume carries its client to its files and mounts', async () => {
  const client = Client.spawn(testServer)
  try {
    const volume = await client.createVolume('w')
    assert.ok(volume instanceof virtx.Volume && volume instanceof virtx.Files)
    assert.equal(volume.client, client)
    assert.deepEqual({ ...volume }, { name: 'w', size: 0 })
    await volume.createDirAll('in/dir')
    await volume.writeFile('in/a.txt', 'hello')
    await volume.renameFile('in/a.txt', 'in/b.txt')
    await volume.copyFile('in/b.txt', 'c.txt')
    assert.equal(await (await client.getVolume('w')).readText('c.txt'), 'hello')
    assert.equal(await volume.isDir('in/dir'), true)
    assert.deepEqual((await volume.readDir('in')).map((entry) => entry.name).sort(), ['b.txt', 'dir'])

    const mount = await volume.mount('notion', { type: 'notion', apiKey: 'k' })
    assert.deepEqual(mount, { path: 'notion', mountType: 'notion' })
    assert.deepEqual(await (await client.getVolume('w')).mounts(), [mount])
    await volume.unmount('notion')
    assert.deepEqual(await (await client.getVolume('w')).mounts(), [])

    const got = await client.getVolume('w')
    assert.deepEqual(got, volume)
    assert.ok((await client.listVolume()).some((v) => v.name === 'w'))
    await volume.remove()
    await assert.rejects(client.getVolume('w'), { status: 404 })
    await assert.rejects(got.statFile('c.txt'), { status: 404 })
  } finally {
    await client.close()
  }
})

test('a snapshot carries its client to its files', async () => {
  const client = Client.spawn(testServer)
  try {
    const snapshot = await client.createSnapshot('t')
    await snapshot.writeFile('a.txt', 'kept')
    assert.equal(await (await client.getSnapshot('t')).readText('a.txt'), 'kept')
    await snapshot.remove()
    await assert.rejects(client.getSnapshot('t'), { status: 404 })

    const handed = await client.createSnapshot('handed')
    const sandbox = await client.createSandbox('alpine:3.20', {
      snapshot: handed,
      volumes: [{ volume: await client.getVolume('v'), path: '/data', readonly: true }],
    })
    assert.equal(sandbox.snapshot, 'handed')
    assert.equal((await client.getSnapshot('handed')).inUse, true)
    await sandbox.remove()

    const locked = await client.getSnapshot('locked')
    assert.equal(locked.inUse, true)
    await assert.rejects(locked.readDir(''), { status: 423 })
  } finally {
    await client.close()
  }
})

// Each entry of `log`, until and with its end.
async function entries(log) {
  const read = []
  for await (const entry of log) read.push(entry)
  return read
}

test('a snapshot is created, got and removed', async () => {
  const client = Client.spawn(testServer)
  try {
    const snapshot = await client.createSnapshot('made', 1)
    assert.equal(snapshot.name, 'made')
    assert.equal(snapshot.inUse, false)
    assert.deepEqual(await client.getSnapshot('made'), snapshot)
    assert.ok((await client.listSnapshot()).some((s) => s.name === 'made'))
    assert.equal((await client.getSnapshot('locked')).inUse, true)
    await snapshot.remove()
    await assert.rejects(client.getSnapshot('made'), { status: 404 })
  } finally {
    await client.close()
  }
})

test('a volume is created and got', async () => {
  const client = Client.spawn(testServer)
  try {
    const volume = await client.createVolume('w')
    assert.equal(volume.name, 'w')
    assert.deepEqual(await client.getVolume('w'), volume)
    await volume.remove()
    await assert.rejects(client.getVolume('w'), { status: 404 })
  } finally {
    await client.close()
  }
})

test('a command in a sandbox is a job whose log ends with its code', async () => {
  const client = Client.spawn(testServer)
  try {
    const sandbox = await client.createSandbox('alpine:3.20')
    assert.equal(sandbox.running, true)
    assert.equal((await client.getSandbox(sandbox.id)).id, sandbox.id)

    const job = await sandbox.exec(['echo', 'hi'])
    assert.ok(job instanceof virtx.SandboxJob)
    assert.equal(job.sandboxId, sandbox.id)
    assert.equal(job.kind, 'exec')
    assert.deepEqual(job.cmd, ['echo', 'hi'])
    const log = await entries(job.log())
    assert.deepEqual(
      log.map(({ id, ...entry }) => entry),
      [{ stdout: 'hi' }, { end: { code: 0 } }],
    )
    // Read again after the first entry, from the second.
    assert.deepEqual(await entries(job.log(log[0].id)), log.slice(1))
    assert.equal((await job.get()).code, 0)
    const got = await client.getSandbox(sandbox.id)
    assert.equal((await got.job(job.id)).code, 0)
    assert.ok((await got.jobs()).some((j) => j.id === job.id))

    const failed = await sandbox.exec(['fail'])
    const end = (await entries(failed.log())).at(-1)
    assert.deepEqual(end.end, { code: 1 })

    await (await sandbox.job(job.id)).remove()
    await assert.rejects(got.job(job.id), { status: 404 })
    await sandbox.remove()
    await assert.rejects(client.getSandbox(sandbox.id), { status: 404 })
  } finally {
    await client.close()
  }
})

test('a job reads its stdin and is signalled', async () => {
  const client = Client.spawn(testServer)
  try {
    const sandbox = await client.createSandbox('alpine:3.20')
    const cat = await sandbox.exec(['cat'], { stdin: true })
    await cat.writeStdin('one\n')
    await cat.writeStdin('two\n', true)
    const log = await entries(cat.log())
    assert.deepEqual(
      log.filter((entry) => 'stdout' in entry).map((entry) => entry.stdout),
      ['one', 'two'],
    )

    const sleep = await sandbox.exec(['sleep'])
    await sleep.signal('SIGTERM')
    const end = (await entries(sleep.log())).at(-1)
    assert.ok('end' in end)
    assert.equal((await (await client.getSandbox(sandbox.id)).job(sleep.id)).running, false)
  } finally {
    await client.close()
  }
})

test('an image build is a job whose log ends with its digest', async () => {
  const client = Client.spawn(testServer)
  try {
    const job = await client.buildImage(new virtx.Recipe('alpine:3.20').run('true'), 'built:latest')
    assert.equal(job.kind, 'build')
    assert.equal(job.reference, 'built:latest')
    assert.ok(job instanceof virtx.ImageJob)
    assert.equal(job.client, client)
    assert.ok(job.digest)
    const log = await entries(job.log())
    assert.deepEqual(
      log.map(({ id, ...entry }) => entry),
      [{ log: 'pulling alpine:3.20' }, { end: { digest: job.digest } }],
    )
    assert.deepEqual(await entries(job.log(1)), log.slice(1))
    assert.equal((await job.get()).digest, job.digest)
    assert.equal((await client.getImageJob(job.id)).digest, job.digest)
    assert.ok((await client.listImageJob()).some((j) => j.id === job.id))

    const missing = await client.buildImage(new virtx.Recipe('missing'), 'no:latest')
    const end = (await entries(missing.log())).at(-1)
    assert.ok(end.end.error)

    await job.remove()
    await assert.rejects(job.get(), { status: 404 })
  } finally {
    await client.close()
  }
})

test('a started build is iterated to its end', async () => {
  const client = Client.spawn(testServer)
  try {
    const log = await entries(client.buildImage(new virtx.Recipe('alpine:3.20').run('true'), 'iterated:latest'))
    const built = (await client.listImage()).find((image) => image.reference === 'iterated:latest')
    assert.ok(built)
    assert.deepEqual(
      log.map(({ id, ...entry }) => entry),
      [{ log: 'pulling alpine:3.20' }, { end: { digest: built.digest } }],
    )
  } finally {
    await client.close()
  }
})

test('a started command is iterated to its end', async () => {
  const client = Client.spawn(testServer)
  try {
    const sandbox = await client.createSandbox('alpine:3.20')
    const log = await entries(sandbox.exec(['echo', 'hi']))
    assert.deepEqual(
      log.map(({ id, ...entry }) => entry),
      [{ stdout: 'hi' }, { end: { code: 0 } }],
    )
  } finally {
    await client.close()
  }
})

test('a started job awaited and iterated is one job', async () => {
  const client = Client.spawn(testServer)
  try {
    const before = (await client.listImageJob()).length
    const build = client.buildImage(new virtx.Recipe('alpine:3.20').run('true'), 'once:latest')
    const job = await build
    assert.equal(job.kind, 'build')
    assert.ok(job.digest)
    const log = await entries(build)
    assert.deepEqual(log.at(-1).end, { digest: job.digest })
    const jobs = await client.listImageJob()
    assert.equal(jobs.length, before + 1)
    assert.equal((await client.getImageJob(job.id)).digest, job.digest)
  } finally {
    await client.close()
  }
})

// Every tag on the server's images, with the digest it points at.
async function tags(client) {
  return Object.fromEntries((await client.listImage()).map((image) => [image.reference, image.digest]))
}

test('an image is tagged, downloaded, removed and uploaded back', async () => {
  const client = Client.spawn(testServer)
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'virtx-node-'))
  try {
    const digest = (await tags(client))['alpine:3.20']
    await client.tagImage('alpine:3.20', 'copy:1')
    assert.equal((await tags(client))['copy:1'], digest)

    const archive = path.join(dir, 'copy.tar')
    assert.deepEqual((await entries(client.downloadImage('copy:1', archive))).at(-1), { output: { digest } })
    await client.removeImage('copy:1')
    assert.ok(!('copy:1' in (await tags(client))))
    assert.deepEqual((await entries(client.uploadImage(archive, 'copy:1'))).at(-1), { output: { digest } })
    assert.equal((await tags(client))['copy:1'], digest)

    await client.untagImage('copy:1')
    assert.ok(!('copy:1' in (await tags(client))))
  } finally {
    await client.close()
    fs.rmSync(dir, { recursive: true, force: true })
  }
})

test('a push ends with the digest its reference points at', async () => {
  const client = Client.spawn(testServer)
  try {
    const digest = (await tags(client))['alpine:3.20']
    const log = await entries(client.pushImage('alpine:3.20', { type: 'bearer', token: 'secret' }))
    assert.deepEqual(
      log.map(({ id, ...entry }) => entry),
      [{ log: 'pushing alpine:3.20 as bearer' }, { end: { digest } }],
    )
    const missing = await client.pushImage('nothing:here')
    assert.equal(missing.error, 'no image nothing:here')
  } finally {
    await client.close()
  }
})

test('a mount at a path taken is refused', async () => {
  const client = Client.spawn(testServer)
  try {
    const volume = await client.getVolume('v')
    const source = { type: 'notion', apiKey: 'secret' }
    await volume.mount('notion', source)
    await assert.rejects(volume.mount('notion', source), /409/)
    await volume.unmount('notion')
    assert.deepEqual(await volume.mounts(), [])
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

// The `virtx` command the package installs, run as a process of its own.
function virtxCommand(...args) {
  return spawnSync(process.execPath, [path.join(root, 'virtx-node', 'cli.js'), ...args], {
    encoding: 'utf8',
    env: { ...process.env, [virtx.LIBRARY_ENV]: virtx.libraryPath() },
  })
}

test('the command runs against a server', async () => {
  const client = Client.spawn(testServer)
  const alpine = (await client.listImage()).find((image) => image.reference === 'alpine:3.20')
  await client.close()
  const pulled = virtxCommand('--server', testServer, 'pull', '-q', 'alpine:3.20')
  assert.deepEqual([pulled.status, pulled.stdout], [0, `alpine:3.20 ${alpine.digest}\n`], pulled.stderr)
})

test('a bad argument exits as clap does', () => {
  const refused = virtxCommand('images', '--bogus')
  assert.equal(refused.status, 2)
  assert.match(refused.stderr, /unexpected argument '--bogus'/)
})
