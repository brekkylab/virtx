// Images, volumes and their mounts, sandboxes and their ports, and the jobs of images and
// sandboxes, against the fixture's `virtx-stdio`, and its `virtx-server-tcp` where a port is
// forwarded.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import net from 'node:net'
import os from 'node:os'
import path from 'node:path'
import { afterEach, beforeEach, describe, test } from 'node:test'

import virtx from '../index.js'

import { ALPINE, alpine, fixture, tags, unique } from './fixture.mjs'

const { Client, PortForwarder, VirtxError } = virtx

// Each test here has a client of a `virtx-stdio` of its own, closed after it.
describe('against virtx-stdio', () => {
  let client
  beforeEach(() => {
    client = Client.spawn(fixture().stdio)
  })
  afterEach(() => client.close())

  test('a sandbox stops, starts again, is found by its name and committed', async () => {
    await alpine(client)
    const name = unique('work')
    const made = await client.createSandbox(ALPINE, { name, ports: [8000] })
    assert.ok(made instanceof virtx.Sandbox)
    assert.equal(made.client, client)
    assert.equal(made.state, 'running')
    assert.equal(made.name, name)
    assert.equal(made.ephemeral, false)
    assert.equal(made.autoStopMinutes, 15)
    assert.equal(made.autoDeleteDays, 30)
    const found = await client.findSandbox(name)
    assert.ok(found instanceof virtx.Sandbox)
    assert.equal(found.id, made.id)
    assert.equal(await client.findSandbox(unique('nobody')), undefined)
    await assert.rejects(client.createSandbox(ALPINE, { name }), { status: 409, code: 'exists' })

    const stopped = await made.stop()
    assert.ok(stopped instanceof virtx.Sandbox)
    assert.equal(stopped.state, 'stopped')
    const started = await stopped.start()
    assert.equal(started.state, 'running')
    assert.deepEqual(
      started.ports.map(({ port }) => port),
      [8000],
    )
    assert.equal((await client.stopSandbox(made.id)).state, 'stopped')
    assert.equal((await client.startSandbox(made.id)).state, 'running')

    const kept = await client.createSandbox(ALPINE, { autoDeleteDays: 'never' })
    assert.equal(kept.autoDeleteDays, undefined)
    await kept.remove()
    const gone = await client.createSandbox(ALPINE, { ephemeral: true })
    await gone.stop()
    await assert.rejects(client.getSandbox(gone.id), { status: 404 })

    const [first, second] = [`${unique('mine')}:v1`, `${unique('mine')}:v2`]
    const job = await made.commit(first)
    assert.ok(job instanceof virtx.ImageJob)
    assert.equal(job.kind, 'commit')
    const end = (await Array.fromAsync(job.log())).at(-1).end
    assert.deepEqual(end, { digest: (await tags(client))[first] })
    const again = await client.commitImage(made, second)
    assert.equal(again.kind, 'commit')
    await Array.fromAsync(again.log())
    await client.removeImage(first)
    await client.removeImage(second)
    await made.remove()
    await assert.rejects(client.getSandbox(made.id), { status: 404 })
  })

  test("a port opened after is kept in the sandbox's ports", async () => {
    await alpine(client)
    const sandbox = await client.createSandbox(ALPINE, { ports: [8000] })
    const opened = await sandbox.openPort(8080)
    assert.equal(opened.port, 8080)
    assert.deepEqual(
      sandbox.ports.map(({ port }) => port),
      [8000, 8080],
    )
    assert.deepEqual(await sandbox.openPort(8080), opened)
    assert.deepEqual(
      (await client.startSandbox(sandbox.id)).ports.map(({ port }) => port),
      [8000, 8080],
    )

    const got = await client.getSandbox(sandbox.id)
    assert.equal(got.ports, undefined)
    assert.deepEqual(await got.openPort(8000), sandbox.ports[0])
    assert.deepEqual(got.ports, [sandbox.ports[0]])
    await sandbox.remove()
  })

  test("a port closed is dropped from the sandbox's ports", async () => {
    await alpine(client)
    const sandbox = await client.createSandbox(ALPINE, { ports: [8000, 8080] })
    const kept = sandbox.ports[1]
    await sandbox.closePort(8000)
    assert.deepEqual(sandbox.ports, [kept])
    await (await client.getSandbox(sandbox.id)).closePort(8080)
    assert.deepEqual((await client.startSandbox(sandbox.id)).ports ?? [], [])
    await assert.rejects(sandbox.closePort(8080), { status: 404, code: 'not_found' })
    assert.deepEqual(sandbox.ports, [kept])
    await sandbox.remove()
  })

  test('a port says what it is for, and whether it is HTTP', async () => {
    await alpine(client)
    const sandbox = await client.createSandbox(ALPINE, {
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
    await sandbox.remove()
  })

  test('inbound rules are refused by a server that keeps none', async () => {
    await alpine(client)
    const ports = [{ port: 22, inbound: [{ source: '203.0.113.7', description: 'office' }] }]
    await assert.rejects(client.createSandbox(ALPINE, { ports }), (error) => {
      assert.deepEqual([error.status, error.code], [400, 'invalid'])
      assert.match(error.message, /virtx cloud's/)
      return true
    })
  })

  // What `await using` calls on leaving its block, called as it would be, so the test parses on
  // a Node without that syntax.
  test('a sandbox is deleted when disposed', { skip: !Symbol.asyncDispose }, async () => {
    await alpine(client)
    const sandbox = await client.createSandbox(ALPINE)
    assert.equal((await client.getSandbox(sandbox.id)).state, 'running')
    await sandbox[Symbol.asyncDispose]()
    await assert.rejects(client.getSandbox(sandbox.id), { status: 404 })

    // One deleted already is left so.
    await sandbox[Symbol.asyncDispose]()
  })

  test('a volume is created and got', async () => {
    const name = unique('volume')
    const volume = await client.createVolume(name)
    assert.equal(volume.name, name)
    assert.deepEqual(await client.getVolume(name), volume)
    await volume.remove()
    await assert.rejects(client.getVolume(name), { status: 404 })
  })

  test('a command in a sandbox is a job whose log ends with its code', async () => {
    await alpine(client)
    const sandbox = await client.createSandbox(ALPINE)
    assert.equal(sandbox.state, 'running')
    assert.equal((await client.getSandbox(sandbox.id)).id, sandbox.id)

    const job = await sandbox.exec(['echo', 'hi'])
    assert.ok(job instanceof virtx.SandboxJob)
    assert.equal(job.sandboxId, sandbox.id)
    assert.equal(job.kind, 'exec')
    assert.deepEqual(job.cmd, ['echo', 'hi'])
    const log = await Array.fromAsync(job.log())
    assert.deepEqual(
      log.map(({ id, ...entry }) => entry),
      [{ stdout: 'hi' }, { end: { code: 0 } }],
    )
    // Read again after the first entry, from the second.
    assert.deepEqual(await Array.fromAsync(job.log(log[0].id)), log.slice(1))
    assert.equal((await job.get()).code, 0)
    const got = await client.getSandbox(sandbox.id)
    assert.equal((await got.job(job.id)).code, 0)
    assert.ok((await got.jobs()).some((j) => j.id === job.id))

    const failed = await sandbox.exec(['sh', '-c', 'echo failed >&2; exit 1'])
    assert.deepEqual(
      (await Array.fromAsync(failed.log())).map(({ id, ...entry }) => entry),
      [{ stderr: 'failed' }, { end: { code: 1 } }],
    )

    await (await sandbox.job(job.id)).remove()
    await assert.rejects(got.job(job.id), { status: 404 })
    await sandbox.remove()
    await assert.rejects(client.getSandbox(sandbox.id), { status: 404 })
  })

  test('a job reads its stdin until it is closed', async () => {
    await alpine(client)
    const sandbox = await client.createSandbox(ALPINE)
    try {
      const cat = await sandbox.exec(['cat'], { stdin: true })
      await cat.writeStdin('one\n')
      await cat.writeStdin('two\n', true)
      const log = await Array.fromAsync(cat.log())
      assert.deepEqual(
        log.filter((entry) => 'stdout' in entry).map((entry) => entry.stdout),
        ['one', 'two'],
      )
    } finally {
      await sandbox.remove()
    }
  })

  test('a job is signalled', async () => {
    await alpine(client)
    const sandbox = await client.createSandbox(ALPINE)
    const sleep = await sandbox.exec(['sleep', '60'])
    await sleep.signal('SIGTERM')
    const end = (await Array.fromAsync(sleep.log())).at(-1)
    assert.deepEqual(end.end, { code: 128 + 15 })
    assert.equal((await (await client.getSandbox(sandbox.id)).job(sleep.id)).running, false)
    await sandbox.remove()
  })

  test('an image build is a job whose log ends with its digest', async () => {
    await alpine(client)
    const reference = `${unique('built')}:latest`
    const job = await client.buildImage(
      new virtx.Recipe(ALPINE).run(`echo ${reference} > /built`),
      reference,
    )
    assert.equal(job.kind, 'build')
    assert.equal(job.reference, reference)
    assert.ok(job instanceof virtx.ImageJob)
    assert.equal(job.client, client)
    const log = await Array.fromAsync(job.log())
    const digest = (await tags(client))[reference]
    assert.deepEqual(log.at(-1).end, { digest })
    assert.deepEqual(await Array.fromAsync(job.log(log[0].id)), log.slice(1))
    assert.equal((await job.get()).digest, digest)
    assert.equal((await client.getImageJob(job.id)).digest, digest)
    assert.ok((await client.listImageJob()).some((j) => j.id === job.id))

    await job.remove()
    await assert.rejects(job.get(), { status: 404 })
    await client.removeImage(reference)
  })

  test('a started build is iterated to its end', async () => {
    await alpine(client)
    const reference = `${unique('iterated')}:latest`
    const log = await Array.fromAsync(
      client.buildImage(new virtx.Recipe(ALPINE).run(`echo ${reference} > /built`), reference),
    )
    assert.deepEqual(log.at(-1).end, { digest: (await tags(client))[reference] })
    await client.removeImage(reference)
  })

  test('a started command is iterated to its end', async () => {
    await alpine(client)
    const sandbox = await client.createSandbox(ALPINE)
    const log = await Array.fromAsync(sandbox.exec(['echo', 'hi']))
    assert.deepEqual(
      log.map(({ id, ...entry }) => entry),
      [{ stdout: 'hi' }, { end: { code: 0 } }],
    )
    await sandbox.remove()
  })

  test('a command iterated then awaited answers the job it read', async () => {
    await alpine(client)
    const sandbox = await client.createSandbox(ALPINE)
    const echo = sandbox.exec(['echo', 'hi'])
    const log = await Array.fromAsync(echo)
    assert.deepEqual(log.at(-1).end, { code: 0 })
    const job = await echo
    assert.ok(job instanceof virtx.SandboxJob)
    assert.equal(job.sandboxId, sandbox.id)
    assert.deepEqual(job.cmd, ['echo', 'hi'])
    assert.deepEqual(
      (await sandbox.jobs()).map((j) => j.id),
      [job.id],
    )
    await sandbox.remove()
  })

  test('an exec iterated at once whose start fails throws in the loop, and nowhere else', async () => {
    const unheard = []
    const hear = (reason) => unheard.push(reason)
    process.on('unhandledRejection', hear)
    try {
      await alpine(client)
      const sandbox = await client.createSandbox(ALPINE)
      await assert.rejects(
        (async () => {
          for await (const _ of sandbox.exec(['no-such-command'])) {
            // no entry: its start fails
          }
        })(),
        { status: 422, code: 'not_started' },
      )
      await sandbox.remove()
      // A turn of the event loop for an unheard rejection to be told.
      await new Promise((resolve) => setTimeout(resolve, 50))
      assert.deepEqual(unheard, [])
    } finally {
      process.off('unhandledRejection', hear)
    }
  })

  test('a started job awaited and iterated is one job', async () => {
    await alpine(client)
    const reference = `${unique('once')}:latest`
    const before = (await client.listImageJob()).length
    const build = client.buildImage(
      new virtx.Recipe(ALPINE).run(`echo ${reference} > /built`),
      reference,
    )
    const job = await build
    assert.equal(job.kind, 'build')
    const log = await Array.fromAsync(build)
    const digest = (await tags(client))[reference]
    assert.deepEqual(log.at(-1).end, { digest })
    assert.equal((await client.listImageJob()).length, before + 1)
    assert.equal((await client.getImageJob(job.id)).digest, digest)
    await client.removeImage(reference)
  })

  test('an image is tagged, downloaded, removed and uploaded back', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'virtx-node-'))
    try {
      const digest = await alpine(client)
      const reference = `${unique('copy')}:1`
      await client.tagImage(ALPINE, reference)
      assert.equal((await tags(client))[reference], digest)

      const archive = path.join(dir, 'copy.tar')
      assert.deepEqual((await Array.fromAsync(client.downloadImage(reference, archive))).at(-1), {
        output: { digest },
      })
      await client.removeImage(reference)
      assert.ok(!(reference in (await tags(client))))
      assert.deepEqual((await Array.fromAsync(client.uploadImage(archive, reference))).at(-1), {
        output: { digest },
      })
      assert.equal((await tags(client))[reference], digest)

      await client.untagImage(reference)
      assert.ok(!(reference in (await tags(client))))
    } finally {
      fs.rmSync(dir, { recursive: true, force: true })
    }
  })

  test('a push is a job that ends with its end', async () => {
    await alpine(client)
    // A registry that does not resolve, so the push fails before it reaches anyone.
    const reference = `registry.invalid/${unique('pushed')}:latest`
    await client.tagImage(ALPINE, reference)
    const push = await client.pushImage(reference, { type: 'bearer', token: 'test-secret' })
    assert.equal(push.kind, 'push')
    assert.ok((await Array.fromAsync(push.log())).at(-1).end.error)
    await client.removeImage(reference)
  })
})

test('a forwarded port nothing listens on says so', async () => {
  const { url, token, project } = fixture()
  const client = Client.connect(url, { type: 'bearer', token }, { 'virtx-project': project })
  const warned = []
  const warning = (w) => w.name === 'VirtxWarning' && warned.push(w.message)
  process.on('warning', warning)
  try {
    await alpine(client)
    const sandbox = await client.createSandbox(ALPINE, { ports: [7000] })
    const forwarder = PortForwarder.open(client, sandbox.ports[0])
    try {
      assert.equal(forwarder.lastError, null)
      const read = await new Promise((resolve, reject) => {
        const socket = net.connect(forwarder.port, forwarder.host)
        const chunks = []
        socket.on('data', (chunk) => chunks.push(chunk))
        socket.on('end', () => resolve(Buffer.concat(chunks)))
        socket.on('error', reject)
      })
      assert.equal(read.length, 0)

      // Kept once the connection that failed has closed, and warned of: looked for a minute.
      const deadline = Date.now() + 60_000
      while (forwarder.lastError === null && Date.now() < deadline) {
        await new Promise((resolve) => setTimeout(resolve, 10))
      }
      const error = forwarder.lastError
      assert.ok(error instanceof VirtxError, 'no error was kept')
      assert.ok(
        error.message.startsWith(`port 7000, forwarded at ${forwarder.address}: `),
        error.message,
      )
      assert.match(error.message, /nothing listens on the port/)
      assert.ok(!error.message.includes(sandbox.ports[0].url))
      while (!warned.includes(error.message) && Date.now() < deadline) {
        await new Promise((resolve) => setTimeout(resolve, 10))
      }
      assert.ok(warned.includes(error.message), `not warned of: ${warned}`)
    } finally {
      forwarder.close()
      await sandbox.remove()
    }
  } finally {
    process.off('warning', warning)
    await client.close()
  }
})
