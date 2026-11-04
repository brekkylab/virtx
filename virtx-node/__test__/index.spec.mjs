// The client over `libvirtx_core`: where no server is needed, and against `virtx-core`'s
// `test_server` example over stdio for the rest. The library and the example are built with
// cargo first, unless `$VIRTX_CORE_LIB` names a library.

import assert from 'node:assert/strict'
import { execFileSync, spawnSync } from 'node:child_process'
import http from 'node:http'
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
    assert.deepEqual((await volume.readDir('in')).map((entry) => entry.name).sort(), [
      'b.txt',
      'dir',
    ])

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

test('a sandbox takes a volume by its handle or its name', async () => {
  const client = Client.spawn(testServer)
  try {
    const sandbox = await client.createSandbox('alpine:3.20', {
      volumes: [
        { volume: await client.getVolume('v'), path: '/data', readonly: true },
        { volume: 's', path: '/shared' },
      ],
    })
    assert.equal((await client.getSandbox(sandbox.id)).id, sandbox.id)
    await sandbox.remove()
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
    assert.ok(made instanceof virtx.Sandbox)
    assert.equal(made.client, client)
    assert.equal(made.state, 'running')
    assert.equal(made.name, 'work')
    assert.equal(made.ephemeral, false)
    assert.equal(made.autoStopMinutes, 15)
    assert.equal(made.autoDeleteDays, 30)
    const found = await client.findSandbox('work')
    assert.ok(found instanceof virtx.Sandbox)
    assert.equal(found.id, made.id)
    assert.equal(await client.findSandbox('nobody'), undefined)
    await assert.rejects(client.createSandbox('alpine:3.20', { name: 'work' }), { status: 409 })

    const stopped = await made.stop()
    assert.ok(stopped instanceof virtx.Sandbox)
    assert.equal(stopped.state, 'stopped')
    const started = await stopped.start()
    assert.equal(started.state, 'running')
    assert.deepEqual(started.ports, made.ports)
    assert.equal((await client.stopSandbox(made.id)).state, 'stopped')
    assert.equal((await client.startSandbox(made.id)).state, 'running')

    const kept = await client.createSandbox('alpine:3.20', { autoDeleteDays: 'never' })
    assert.equal(kept.autoDeleteDays, undefined)
    const gone = await client.createSandbox('alpine:3.20', { ephemeral: true })
    await gone.stop()
    await assert.rejects(client.getSandbox(gone.id), { status: 404 })

    const job = await made.commit('mine:v1')
    assert.ok(job instanceof virtx.ImageJob)
    assert.equal(job.kind, 'commit')
    assert.equal((await client.commitImage(made, 'mine:v2')).kind, 'commit')
    await made.remove()
    await assert.rejects(client.getSandbox(made.id), { status: 404 })
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
    await (await client.getSandbox(sandbox.id)).closePort(8080)
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
    const renamed = await sandbox.openPort(8080, { label: 'manual' })
    assert.deepEqual([renamed.label, renamed.protocol], ['manual', 'http'])
    const plain = await sandbox.openPort(9000)
    assert.deepEqual([plain.label, plain.protocol], [undefined, undefined])
  } finally {
    await client.close()
  }
})

// The test server is on the client's machine and gives no port an address: a server that does
// is played by one answering a sandbox, and a port opened in it with such an address.
test('a TCP port answered with an address has it', async () => {
  const answer = { port: 22, url: 'ports/x', address: 'tcp.example:20001' }
  const sandbox = {
    id: 'sb',
    image: 'sha256:0',
    state: 'running',
    ephemeral: false,
    auto_stop_minutes: 15,
    auto_delete_days: 30,
    used_at: '2026-01-01T00:00:00Z',
  }
  const server = http.createServer((request, response) => {
    const opening = request.method === 'PUT'
    response.writeHead(opening ? 201 : 200, { 'content-type': 'application/json' })
    response.end(JSON.stringify(opening ? answer : sandbox))
  })
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
  const client = Client.connect(`http://127.0.0.1:${server.address().port}/`)
  try {
    assert.deepEqual(await (await client.getSandbox('sb')).openPort(22), answer)
  } finally {
    await client.close()
    server.close()
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
    assert.equal(sandbox.state, 'running')
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
    const log = await entries(job.log())
    assert.deepEqual(
      log.map(({ id, ...entry }) => entry),
      [{ log: 'pulling alpine:3.20' }, { end: { digest: 'sha256:0' } }],
    )
    assert.deepEqual(await entries(job.log(1)), log.slice(1))
    assert.equal((await job.get()).digest, 'sha256:0')
    assert.equal((await client.getImageJob(job.id)).digest, 'sha256:0')
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
    const sandbox = await client.createSandbox('alpine:3.20')
    const echo = sandbox.exec(['echo', 'hi'])
    const log = await entries(echo)
    assert.deepEqual(log.at(-1), { id: 2, end: { code: 0 } })
    const job = await echo
    assert.ok(job instanceof virtx.SandboxJob)
    assert.equal(job.sandboxId, sandbox.id)
    assert.deepEqual(job.cmd, ['echo', 'hi'])
    assert.deepEqual(
      (await sandbox.jobs()).map((j) => j.id),
      [job.id],
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

// The `virtx` command the package installs, run as a process of its own.
function virtxCommand(...args) {
  return spawnSync(process.execPath, [path.join(root, 'virtx-node', 'cli.js'), ...args], {
    encoding: 'utf8',
    env: { ...process.env, [virtx.LIBRARY_ENV]: virtx.libraryPath() },
  })
}

test('the command runs against a server', () => {
  const pulled = virtxCommand('--server', testServer, 'pull', '-q', 'alpine:3.20')
  assert.deepEqual([pulled.status, pulled.stdout], [0, 'alpine:3.20 sha256:0\n'], pulled.stderr)
})

test('a bad argument exits as clap does', () => {
  const refused = virtxCommand('images', '--bogus')
  assert.equal(refused.status, 2)
  assert.match(refused.stderr, /unexpected argument '--bogus'/)
})
