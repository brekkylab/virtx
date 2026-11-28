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
      const forwarder = await PortForwarder.open(sandbox, 8000)
      assert.deepEqual([forwarder.port, forwarder.on], [8000, 'sandbox'])
      const at = forwarder.local.lastIndexOf(':')
      const [host, port] = [forwarder.local.slice(0, at), Number(forwarder.local.slice(at + 1))]
      assert.equal(host, '127.0.0.1')
      // What connecting comes to: `connected`, or the error's code.
      const connect = () =>
        new Promise((resolve) => {
          const stream = net.connect(port, host)
          stream.on('connect', () => {
            stream.destroy()
            resolve('connected')
          })
          stream.on('error', (error) => resolve(error.code))
        })
      // A connection is taken, whatever the sandbox does with it.
      assert.equal(await connect(), 'connected')
      // At an address asked for.
      const free = net.createServer()
      await new Promise((resolve) => free.listen(0, '127.0.0.1', resolve))
      const asked = `127.0.0.1:${free.address().port}`
      await new Promise((resolve) => free.close(resolve))
      const atAsked = await PortForwarder.open(sandbox, 8000, { local: asked })
      assert.equal(atAsked.local, asked)
      atAsked.close()

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

test('a package is kept, makes a sandbox, and is removed', async () => {
  const client = Client.spawn(fixture().stdio)
  try {
    await alpine(client)
    const name = unique('package')
    const access = [
      { type: 'public' },
      { type: 'jwt', issuer: 'https://idp.example', audience: 'app', claims: { repo_name: 'a' } },
    ]
    const pkg = await client.putPackage(name, ALPINE, {
      lifetime: 600,
      max: { vcpus: 8 },
      access,
      origins: ['https://app.example'],
      limits: { perSubject: 1 },
    })
    assert.ok(pkg instanceof virtx.Package)
    assert.equal(pkg.name, name)
    assert.equal(pkg.sandbox.lifetime, 600)
    assert.deepEqual(pkg.access, access, 'its claims as they were given')
    assert.deepEqual(pkg.limits, { perSubject: 1 })
    assert.equal((await client.getPackage(pkg.id)).name, name)
    assert.ok((await client.listPackage()).some((kept) => kept.id === pkg.id))

    const made = await pkg.createSandbox({ name: unique('of-package') })
    assert.ok(made.sandbox instanceof virtx.Sandbox)
    assert.equal(made.grant, null, 'a server over stdio gives no grant')
    await made.sandbox.remove()
    await pkg.remove()
    await assert.rejects(client.getPackage(pkg.id), { status: 404 })
  } finally {
    await client.close()
  }
})

test('an app client reaches the sandbox a package lets it make', async () => {
  const { url, token, project } = fixture()
  const client = Client.connect(url, { type: 'bearer', token }, project)
  try {
    await alpine(client)
    const members = await client.putPackage(unique('members'), ALPINE)
    await assert.rejects(virtx.AppClient.connect(url, members.id), { status: 401 })
    await members.remove()

    const pkg = await client.putPackage(unique('public'), ALPINE, {
      access: [{ type: 'public' }],
    })
    const app = await virtx.AppClient.connect(url, pkg.id)
    const { id } = app.sandbox
    try {
      assert.equal(await app.sandbox.exec(['true']).complete(), 0)
    } finally {
      await app.close()
    }
    await assert.rejects(client.getSandbox(id), { status: 404 })
    await pkg.remove()
  } finally {
    await client.close()
  }
})
