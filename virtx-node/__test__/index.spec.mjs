// The client over `libvirtx_core`: where no server is needed, and against `virtx-core`'s
// `test_server` example over stdio for the rest. The library and the example are built with
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

const testServer = path.join(
  target,
  'examples',
  process.platform === 'win32' ? 'test_server.exe' : 'test_server',
)

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
  assert.throws(
    () => Client.connect('http://127.0.0.1:9/', null, { 'not a name': 'x' }),
    /not a name/,
  )
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
    const volume = { volume: 'v' }
    await client.createDir(volume, 'dir')
    await client.writeFile(volume, 'dir/a.txt', 'hello world')
    await client.createDirAll(volume, 'deep/er/dir')
    await client.createDirAll(volume, 'deep/er/dir')
    assert.equal((await client.statFile(volume, 'deep/er/dir')).isDir, true)

    const metadata = await client.statFile(volume, 'dir/a.txt')
    assert.equal(metadata.isDir, false)
    assert.equal(metadata.size, 11)
    assert.deepEqual(await client.readDir(volume, 'dir'), [{ name: 'a.txt', metadata }])

    assert.equal((await client.readFile(volume, 'dir/a.txt')).toString(), 'hello world')
    assert.equal((await client.readFile(volume, 'dir/a.txt', 6, 3)).toString(), 'wor')

    // Text, an etag to write over, and what is there, told without an error.
    await client.writeFile(volume, 'dir/t.txt', '텍스트')
    assert.equal(await client.readText(volume, 'dir/t.txt'), '텍스트')
    const { etag } = await client.statFile(volume, 'dir/t.txt')
    assert.ok(etag)
    await client.writeFile(volume, 'dir/t.txt', 'text 2', etag)
    await assert.rejects(client.writeFile(volume, 'dir/t.txt', 'text 3', etag), /412/)
    assert.equal(await client.exists(volume, 'dir/t.txt'), true)
    assert.equal(await client.isFile(volume, 'dir/t.txt'), true)
    assert.equal(await client.isDir(volume, 'dir/t.txt'), false)
    assert.equal(await client.isDir(volume, 'dir'), true)
    await client.removeFile(volume, 'dir/t.txt')
    assert.equal(await client.exists(volume, 'dir/t.txt'), false)
    await assert.rejects(client.removeFile(volume, 'dir/t.txt'), /404/)
    await client.removeFile(volume, 'dir/t.txt', true)

    await client.renameFile(volume, 'dir/a.txt', 'b.txt')
    await client.copyFile(volume, 'b.txt', 'c.txt')
    await assert.rejects(client.copyFile(volume, 'b.txt', 'c.txt'), /412/)
    await client.removeFile(volume, 'dir')
    await assert.rejects(client.statFile(volume, 'dir'), /404/)
    assert.equal((await client.readFile(volume, 'c.txt')).toString(), 'hello world')
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

test('a sandbox stops, starts again, is found by its name and committed', async () => {
  const client = Client.spawn(testServer)
  try {
    const made = await client.createSandbox('alpine:3.20', { name: 'work', ports: [8000] })
    assert.equal(made.state, 'running')
    assert.equal(made.name, 'work')
    assert.equal(made.ephemeral, false)
    assert.equal(made.autoStopMinutes, 15)
    assert.equal(made.autoDeleteDays, 30)
    assert.equal((await client.findSandbox('work')).id, made.id)
    assert.equal(await client.findSandbox('nobody'), undefined)
    await assert.rejects(client.createSandbox('alpine:3.20', { name: 'work' }), /409/)

    assert.equal((await client.stopSandbox(made.id)).state, 'stopped')
    const started = await client.startSandbox(made.id)
    assert.equal(started.state, 'running')
    assert.deepEqual(started.ports, made.ports)

    const kept = await client.createSandbox('alpine:3.20', { autoDeleteDays: 'never' })
    assert.equal(kept.autoDeleteDays, undefined)
    const gone = await client.createSandbox('alpine:3.20', { ephemeral: true })
    await client.stopSandbox(gone.id)
    await assert.rejects(client.getSandbox(gone.id), /404/)

    const job = await client.commitImage(made.id, 'mine:v1')
    assert.equal(job.kind, 'commit')
    await client.removeSandbox(made.id)
    await assert.rejects(client.getSandbox(made.id), /404/)
  } finally {
    await client.close()
  }
})

test("a port opened after is kept in the sandbox's ports", async () => {
  const client = Client.spawn(testServer)
  try {
    const sandbox = await client.createSandbox('alpine:3.20', { ports: [8000] })
    const opened = await sandbox.openPort(8080)
    assert.equal(opened.port, 8080)
    assert.deepEqual(
      sandbox.ports.map(({ port }) => port),
      [8000, 8080],
    )
    assert.deepEqual(await sandbox.openPort(8080), opened)
    assert.deepEqual(sandbox.ports, (await client.startSandbox(sandbox.id)).ports)

    const got = await client.getSandbox(sandbox.id)
    assert.equal(got.ports, undefined)
    assert.deepEqual(await got.openPort(8000), sandbox.ports[0])
    assert.deepEqual(got.ports, [sandbox.ports[0]])
  } finally {
    await client.close()
  }
})

test("a port closed is dropped from the sandbox's ports", async () => {
  const client = Client.spawn(testServer)
  try {
    const sandbox = await client.createSandbox('alpine:3.20', { ports: [8000, 8080] })
    const kept = sandbox.ports[1]
    await sandbox.closePort(8000)
    assert.deepEqual(sandbox.ports, [kept])
    await client.closeSandboxPort(sandbox.id, 8080)
    assert.deepEqual((await client.startSandbox(sandbox.id)).ports ?? [], [])
    await assert.rejects(sandbox.closePort(8080), /404/)
    assert.deepEqual(sandbox.ports, [kept])
  } finally {
    await client.close()
  }
})

test('a port says what it is for, and whether it is HTTP', async () => {
  const client = Client.spawn(testServer)
  try {
    const sandbox = await client.createSandbox('alpine:3.20', {
      ports: [
        5900,
        { port: 5173, label: 'dev server', protocol: 'http' },
        { port: 3000, label: 'api' },
      ],
    })
    const [vnc, dev, api] = sandbox.ports
    assert.deepEqual([vnc.port, vnc.label, vnc.protocol], [5900, undefined, undefined])
    assert.deepEqual([dev.port, dev.label, dev.protocol], [5173, 'dev server', 'http'])
    assert.deepEqual([api.port, api.label, api.protocol], [3000, 'api', undefined])

    const web = await sandbox.openPort(8080, { label: 'docs', protocol: 'http' })
    assert.deepEqual(web, { port: 8080, url: web.url, label: 'docs', protocol: 'http' })
    assert.deepEqual(sandbox.ports.at(-1), web)
    const renamed = await client.openSandboxPort(sandbox.id, 8080, { label: 'manual' })
    assert.deepEqual([renamed.label, renamed.protocol], ['manual', 'http'])
    const plain = await client.openSandboxPort(sandbox.id, 9000)
    assert.deepEqual([plain.label, plain.protocol], [undefined, undefined])
  } finally {
    await client.close()
  }
})

// What `await using` calls on leaving its block, called as it would be, so the test parses on a
// Node without that syntax.
test('a sandbox is deleted when disposed', { skip: !Symbol.asyncDispose }, async () => {
  const client = Client.spawn(testServer)
  try {
    const sandbox = await client.createSandbox('alpine:3.20')
    assert.equal((await client.getSandbox(sandbox.id)).state, 'running')
    await sandbox[Symbol.asyncDispose]()
    await assert.rejects(client.getSandbox(sandbox.id), /404/)

    // One deleted already is left so.
    await sandbox[Symbol.asyncDispose]()
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
    await client.removeVolume('w')
  } finally {
    await client.close()
  }
})

test('a command in a sandbox is a job whose log ends with its code', async () => {
  const client = Client.spawn(testServer)
  try {
    const sandbox = await client.createSandbox('alpine:3.20')
    assert.equal(sandbox.state, 'running')
    assert.equal((await client.getSandbox(sandbox.id)).id, sandbox.id)

    const job = await client.execSandbox(sandbox.id, ['echo', 'hi'])
    assert.equal(job.kind, 'exec')
    assert.deepEqual(job.cmd, ['echo', 'hi'])
    const log = await entries(client.sandboxJobLog(sandbox.id, job.id))
    assert.deepEqual(
      log.map(({ id, ...entry }) => entry),
      [{ stdout: 'hi' }, { end: { code: 0 } }],
    )
    // Read again after the first entry, from the second.
    assert.deepEqual(
      await entries(client.sandboxJobLog(sandbox.id, job.id, log[0].id)),
      log.slice(1),
    )
    assert.equal((await client.getSandboxJob(sandbox.id, job.id)).code, 0)
    assert.ok((await client.listSandboxJob(sandbox.id)).some((j) => j.id === job.id))

    const failed = await client.execSandbox(sandbox.id, ['fail'])
    const end = (await entries(client.sandboxJobLog(sandbox.id, failed.id))).at(-1)
    assert.deepEqual(end.end, { code: 1 })

    await client.removeSandboxJob(sandbox.id, job.id)
    await assert.rejects(client.getSandboxJob(sandbox.id, job.id), /404/)
    await client.removeSandbox(sandbox.id)
    await assert.rejects(client.getSandbox(sandbox.id), /404/)
  } finally {
    await client.close()
  }
})

test('a job reads its stdin and is signalled', async () => {
  const client = Client.spawn(testServer)
  try {
    const { id: sandbox } = await client.createSandbox('alpine:3.20')
    const cat = await client.execSandbox(sandbox, ['cat'], { stdin: true })
    await client.writeSandboxJobStdin(sandbox, cat.id, 'one\n')
    await client.writeSandboxJobStdin(sandbox, cat.id, 'two\n', true)
    const log = await entries(client.sandboxJobLog(sandbox, cat.id))
    assert.deepEqual(
      log.filter((entry) => 'stdout' in entry).map((entry) => entry.stdout),
      ['one', 'two'],
    )

    const sleep = await client.execSandbox(sandbox, ['sleep'])
    await client.signalSandboxJob(sandbox, sleep.id, 'SIGTERM')
    const end = (await entries(client.sandboxJobLog(sandbox, sleep.id))).at(-1)
    assert.ok('end' in end)
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
    const log = await entries(client.imageJobLog(job.id))
    assert.deepEqual(
      log.map(({ id, ...entry }) => entry),
      [{ log: 'pulling alpine:3.20' }, { end: { digest: 'sha256:0' } }],
    )
    assert.equal((await client.getImageJob(job.id)).digest, 'sha256:0')
    assert.ok((await client.listImageJob()).some((j) => j.id === job.id))

    const missing = await client.buildImage(new virtx.Recipe('missing'), 'no:latest')
    const end = (await entries(client.imageJobLog(missing.id))).at(-1)
    assert.ok(end.end.error)

    await client.removeImageJob(job.id)
    await assert.rejects(client.getImageJob(job.id), /404/)
  } finally {
    await client.close()
  }
})

test('a started build is iterated to its end', async () => {
  const client = Client.spawn(testServer)
  try {
    const log = await entries(
      client.buildImage(new virtx.Recipe('alpine:3.20').run('true'), 'iterated:latest'),
    )
    assert.deepEqual(
      log.map(({ id, ...entry }) => entry),
      [{ log: 'pulling alpine:3.20' }, { end: { digest: 'sha256:0' } }],
    )
  } finally {
    await client.close()
  }
})

test('a command iterated then awaited answers the job it read', async () => {
  const client = Client.spawn(testServer)
  try {
    const { id: sandbox } = await client.createSandbox('alpine:3.20')
    const echo = client.execSandbox(sandbox, ['echo', 'hi'])
    const log = await entries(echo)
    assert.deepEqual(log.at(-1), { id: 2, end: { code: 0 } })
    const job = await echo
    assert.deepEqual(job.cmd, ['echo', 'hi'])
    assert.deepEqual(
      (await client.listSandboxJob(sandbox)).map((j) => j.id),
      [job.id],
    )
  } finally {
    await client.close()
  }
})

test('a started command is iterated to its end', async () => {
  const client = Client.spawn(testServer)
  try {
    const { id: sandbox } = await client.createSandbox('alpine:3.20')
    const log = await entries(client.execSandbox(sandbox, ['echo', 'hi']))
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
    const log = await entries(build)
    assert.deepEqual(log.at(-1).end, { digest: 'sha256:0' })
    const jobs = await client.listImageJob()
    assert.equal(jobs.length, before + 1)
    assert.equal((await client.getImageJob(job.id)).digest, 'sha256:0')
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
