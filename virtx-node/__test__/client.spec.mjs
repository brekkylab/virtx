// The client where no request is sent, and over HTTP to the fixture's server.

import assert from 'node:assert/strict'
import net from 'node:net'
import { test } from 'node:test'
import { setTimeout as sleep } from 'node:timers/promises'

import virtx from '../index.js'

import { ALPINE, alpine, fixture, unique } from './fixture.mjs'

const { Client, PortForwarder } = virtx

test('a second library is refused', () => {
  assert.throws(() => virtx.load('/nonexistent/libvirtx_core'), /is open already/)
  assert.notEqual(virtx.libraryPath(), null)
})

test('an image is named by its spelling', () => {
  const digest = `sha256:${'0'.repeat(64)}`
  assert.deepEqual(virtx.imageName(digest), { type: 'digest', digest })
  assert.deepEqual(virtx.imageName('sha256:0'), { type: 'tag', tag: 'sha256:0' })
})

test('a client over HTTP sends its token and project', async () => {
  const { url, token, project } = fixture()
  const client = Client.connect(url, { type: 'bearer', token }, project)
  try {
    const volume = await client.createVolume(unique('over-http'))
    assert.ok((await client.listVolume()).some((v) => v.name === volume.name))
    await volume.remove()
  } finally {
    await client.close()
  }
  const anonymous = Client.connect(url, null, project)
  try {
    await assert.rejects(anonymous.listVolume(), { status: 401 })
  } finally {
    await anonymous.close()
  }
})

test('a port forwarder listens on the loopback until it is closed', async () => {
  const { url, token, project } = fixture()
  const client = Client.connect(url, { type: 'bearer', token }, project)
  try {
    await alpine(client)
    const sandbox = await client.createSandbox(ALPINE, { ports: [8000] })
    try {
      const forwarder = PortForwarder.open(client, sandbox.ports[0].url)
      assert.equal(typeof forwarder.port, 'number')
      assert.equal(forwarder.address, `${forwarder.host}:${forwarder.port}`)
      assert.equal(forwarder.host, '127.0.0.1')
      // What connecting comes to: `connected`, or the error's code.
      const connect = () =>
        new Promise((resolve) => {
          const stream = net.connect(forwarder.port, forwarder.host)
          stream.on('connect', () => {
            stream.destroy()
            resolve('connected')
          })
          stream.on('error', (error) => resolve(error.code))
        })
      // A connection is taken, whatever the sandbox does with it.
      assert.equal(await connect(), 'connected')

      forwarder.close()
      forwarder.close()
      // Its listener goes a moment after.
      let connected = 'connected'
      const until = Date.now() + 5_000
      while (connected !== 'ECONNREFUSED' && Date.now() < until) {
        connected = await connect()
        await sleep(50)
      }
      assert.equal(connected, 'ECONNREFUSED')
    } finally {
      await sandbox.remove()
    }
  } finally {
    await client.close()
  }
})

test('a project is made, its members set and taken away, and it is removed', async () => {
  const { url, users } = fixture()
  const name = unique('made')
  const alice = Client.connect(url, { type: 'bearer', token: JSON.parse(users).alice }, name)
  try {
    const made = await alice.createProject(name)
    assert.ok(made instanceof virtx.Project)
    assert.deepEqual({ ...made }, { id: made.id, name, owner: 'alice@example.com', role: 'owner' })
    assert.ok((await alice.listProject()).some((p) => p.id === made.id))
    assert.equal((await alice.getProject(name)).role, 'owner')
    assert.equal((await alice.getProject(made.id)).name, name)
    await assert.rejects(alice.createProject(name), {
      name: 'VirtxError',
      source: 'server',
      status: 409,
    })

    assert.deepEqual(await made.setMember('bob@example.com', 'member'), {
      user: 'bob@example.com',
      role: 'member',
    })
    assert.deepEqual(await made.members(), [
      { user: 'alice@example.com', role: 'owner' },
      { user: 'bob@example.com', role: 'member' },
    ])
    await made.removeMember('bob@example.com')
    assert.deepEqual(await made.members(), [{ user: 'alice@example.com', role: 'owner' }])

    await made.remove()
    await assert.rejects(alice.getProject(name), { status: 403 })
  } finally {
    await alice.close()
  }
})

test('setConnections sends through as many and refuses none', async () => {
  const { url, token, project } = fixture()
  const client = Client.connect(url, { type: 'bearer', token }, project)
  try {
    client.setConnections(4)
    const names = Array.from({ length: 8 }, () => unique('over-four'))
    const made = await Promise.all(names.map((name) => client.createVolume(name)))
    assert.deepEqual(made.map((v) => v.name).sort(), [...names].sort())
    assert.throws(() => client.setConnections(0), { name: 'VirtxError', source: 'client' })
    assert.throws(() => client.setConnections(-1), { name: 'VirtxError' })
    const listed = new Set((await client.listVolume()).map((v) => v.name))
    assert.ok(names.every((name) => listed.has(name)))
  } finally {
    await client.close()
  }
})

test('setProject changes the project a request reaches', async () => {
  const { url, users } = fixture()
  const name = unique('set')
  const alice = Client.connect(url, { type: 'bearer', token: JSON.parse(users).alice })
  try {
    const made = await alice.createProject(name)
    alice.setProject(name)
    const volume = await alice.createVolume('data')
    alice.setProject('other')
    await assert.rejects(alice.listVolume(), { status: 403 })
    assert.throws(() => alice.setProject('a\nb'), { name: 'VirtxError', source: 'client' })
    await assert.rejects(alice.listVolume(), { status: 403 })
    alice.setProject(name)
    assert.ok((await alice.listVolume()).some((v) => v.name === volume.name))
    await made.remove()
  } finally {
    await alice.close()
  }
})
