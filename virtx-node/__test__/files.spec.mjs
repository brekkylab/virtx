// The files of a volume, a snapshot and a sandbox, against the fixture's `virtx-stdio`.

import assert from 'node:assert/strict'
import { afterEach, beforeEach, describe, test } from 'node:test'

import virtx from '../index.js'

import { ALPINE, alpine, fixture, unique } from './fixture.mjs'

const { Client } = virtx

// Each of a handle's calls on its files, in `base`: a directory of them, or its root.
async function everyCall(files, base) {
  const at = (path) => (base ? `${base}/${path}` : path)
  await files.createDir(at('dir'))
  await files.writeFile(at('dir/a.txt'), 'hello world')
  await files.createDirAll(at('deep/er/dir'))
  await files.createDirAll(at('deep/er/dir'))
  assert.equal((await files.statFile(at('deep/er/dir'))).isDir, true)

  const metadata = await files.statFile(at('dir/a.txt'))
  assert.equal(metadata.isDir, false)
  assert.equal(metadata.size, 11)
  assert.deepEqual(await files.readDir(at('dir')), [{ name: 'a.txt', metadata }])

  assert.equal((await files.readFile(at('dir/a.txt'))).toString(), 'hello world')
  assert.equal((await files.readFile(at('dir/a.txt'), 6, 3)).toString(), 'wor')

  // Text, an etag to write over, and what is there, told without an error.
  await files.writeFile(at('dir/t.txt'), '텍스트')
  assert.equal(await files.readText(at('dir/t.txt')), '텍스트')
  const { etag } = await files.statFile(at('dir/t.txt'))
  assert.ok(etag)
  await files.writeFile(at('dir/t.txt'), 'text 2', etag)
  assert.equal(await files.exists(at('dir/t.txt')), true)
  assert.equal(await files.isFile(at('dir/t.txt')), true)
  assert.equal(await files.isDir(at('dir/t.txt')), false)
  assert.equal(await files.isDir(at('dir')), true)
  await files.removeFile(at('dir/t.txt'))
  assert.equal(await files.exists(at('dir/t.txt')), false)
  await files.removeFile(at('dir/t.txt'), true)

  await files.renameFile(at('dir/a.txt'), at('b.txt'))
  await files.copyFile(at('b.txt'), at('c.txt'))
  await files.removeFile(at('dir'))
  await assert.rejects(files.statFile(at('dir')), { status: 404 })
  assert.equal((await files.readFile(at('c.txt'))).toString(), 'hello world')
}

// Each test here has a client of a `virtx-stdio` of its own, closed after it.
describe('against virtx-stdio', () => {
  let client
  beforeEach(() => {
    client = Client.spawn(fixture().stdio)
  })
  afterEach(() => client.close())

  test("a volume's files answer every call", async () => {
    const volume = await client.createVolume(unique('files'))
    await everyCall(volume, '')
    await volume.remove()
  })

  test("a snapshot's files answer every call", async () => {
    const snapshot = await client.createSnapshot(unique('files'))
    await everyCall(snapshot, '')
    await snapshot.remove()
  })

  test("a sandbox's files answer every call", async () => {
    await alpine(client)
    const sandbox = await client.createSandbox(ALPINE)
    await everyCall(sandbox, 'root')
    await sandbox.remove()
  })

  test("a sandbox's files are its own and go with it", async () => {
    await alpine(client)
    const sandbox = await client.createSandbox(ALPINE)
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
  })

  test('a volume carries its client to its files and mounts', async () => {
    const name = unique('carried')
    const volume = await client.createVolume(name)
    assert.ok(volume instanceof virtx.Volume && volume instanceof virtx.Files)
    assert.equal(volume.client, client)
    assert.deepEqual({ ...volume }, { name, size: 0 })
    await volume.createDirAll('in/dir')
    await volume.writeFile('in/a.txt', 'hello')
    await volume.renameFile('in/a.txt', 'in/b.txt')
    await volume.copyFile('in/b.txt', 'c.txt')
    assert.equal(await (await client.getVolume(name)).readText('c.txt'), 'hello')
    assert.equal(await volume.isDir('in/dir'), true)
    assert.deepEqual((await volume.readDir('in')).map((entry) => entry.name).sort(), [
      'b.txt',
      'dir',
    ])

    const mount = await volume.mount('notion', { type: 'notion', apiKey: 'k' })
    assert.deepEqual(mount, { path: 'notion', mountType: 'notion' })
    assert.deepEqual(await (await client.getVolume(name)).mounts(), [mount])
    await volume.unmount('notion')
    assert.deepEqual(await (await client.getVolume(name)).mounts(), [])

    const got = await client.getVolume(name)
    assert.ok((await client.listVolume()).some((v) => v.name === name))
    await volume.remove()
    await assert.rejects(client.getVolume(name), { status: 404 })
    await assert.rejects(got.statFile('c.txt'), { status: 404 })
  })

  test('a snapshot carries its client to its files', async () => {
    const name = unique('snapshot')
    const snapshot = await client.createSnapshot(name)
    await snapshot.writeFile('a.txt', 'kept')
    assert.equal(await (await client.getSnapshot(name)).readText('a.txt'), 'kept')
    await snapshot.remove()
    await assert.rejects(client.getSnapshot(name), { status: 404 })
  })
})
