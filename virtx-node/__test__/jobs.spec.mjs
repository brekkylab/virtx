// Images, snapshots, volumes and their mounts, sandboxes, and the jobs of images and sandboxes,
// against the fixture's `virtx-stdio`.

import assert from 'node:assert/strict'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { afterEach, beforeEach, describe, test } from 'node:test'

import virtx from '../index.js'

import { ALPINE, alpine, fixture, tags, unique } from './fixture.mjs'

const { Client } = virtx

// Each test here has a client of a `virtx-stdio` of its own, closed after it.
describe('against virtx-stdio', () => {
  let client
  beforeEach(() => {
    client = Client.spawn(fixture().stdio)
  })
  afterEach(() => client.close())

  test('a snapshot is created, got and removed', async () => {
    const name = unique('made')
    const snapshot = await client.createSnapshot(name, 1)
    assert.equal(snapshot.name, name)
    assert.equal(snapshot.inUse, false)
    assert.deepEqual(await client.getSnapshot(name), snapshot)
    assert.ok((await client.listSnapshot()).some((s) => s.name === name))
    await snapshot.remove()
    await assert.rejects(client.getSnapshot(name), { status: 404 })
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
    const sandbox = await client.createSandbox(ALPINE, { abins: ['index'] })
    assert.equal(sandbox.running, true)
    assert.deepEqual(sandbox.abins, ['index'])
    assert.deepEqual((await client.getSandbox(sandbox.id)).abins, ['index'])
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

  test(
    'a job reads its stdin until it is closed',
    { skip: 'no server starts a process with a stdin to write to yet: `exec` with `stdin` is refused' },
    async () => {
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
    },
  )

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
    const job = await client.buildImage(new virtx.Recipe(ALPINE).run(`echo ${reference} > /built`), reference)
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
    const log = await Array.fromAsync(client.buildImage(new virtx.Recipe(ALPINE).run(`echo ${reference} > /built`), reference))
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

  test('a started job awaited and iterated is one job', async () => {
    await alpine(client)
    const reference = `${unique('once')}:latest`
    const before = (await client.listImageJob()).length
    const build = client.buildImage(new virtx.Recipe(ALPINE).run(`echo ${reference} > /built`), reference)
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
      assert.deepEqual((await Array.fromAsync(client.downloadImage(reference, archive))).at(-1), { output: { digest } })
      await client.removeImage(reference)
      assert.ok(!(reference in (await tags(client))))
      assert.deepEqual((await Array.fromAsync(client.uploadImage(archive, reference))).at(-1), { output: { digest } })
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
