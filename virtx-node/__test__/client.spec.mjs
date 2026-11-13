// The client where no request is sent, and over HTTP to the fixture's server.

import assert from 'node:assert/strict'
import { test } from 'node:test'

import virtx from '../index.js'

import { fixture, unique } from './fixture.mjs'

const { Client } = virtx

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
  const headers = { 'virtx-project': project }
  const client = Client.connect(url, { type: 'bearer', token }, headers)
  try {
    const volume = await client.createVolume(unique('over-http'))
    assert.ok((await client.listVolume()).some((v) => v.name === volume.name))
    await volume.remove()
  } finally {
    await client.close()
  }
  const anonymous = Client.connect(url, null, headers)
  try {
    await assert.rejects(anonymous.listVolume(), { status: 401 })
  } finally {
    await anonymous.close()
  }
})
