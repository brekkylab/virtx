// The `virtx` command the package installs, run as a process of its own against the fixture's
// `virtx-stdio`.

import assert from 'node:assert/strict'
import { spawnSync } from 'node:child_process'
import path from 'node:path'
import { test } from 'node:test'

import virtx from '../index.js'

import { ALPINE, familiar, fixture, root, tags } from './fixture.mjs'

const { Client } = virtx

// The `virtx` command the package installs, run as a process of its own.
function virtxCommand(...args) {
  return spawnSync(process.execPath, [path.join(root, 'virtx-node', 'cli.js'), ...args], {
    encoding: 'utf8',
    env: { ...process.env, [virtx.LIBRARY_ENV]: virtx.libraryPath() },
  })
}

test('the command runs against a server', async () => {
  const pulled = virtxCommand('--server', fixture().stdio, 'pull', '-q', ALPINE)
  assert.equal(pulled.status, 0, pulled.stderr)
  const client = Client.spawn(fixture().stdio)
  const digest = (await tags(client))[ALPINE]
  await client.close()
  const [reference, said] = pulled.stdout.trim().split(' ')
  assert.deepEqual([familiar(reference), said], [ALPINE, digest])
})

test('a bad argument exits as clap does', () => {
  const refused = virtxCommand('images', '--bogus')
  assert.equal(refused.status, 2)
  assert.match(refused.stderr, /unexpected argument '--bogus'/)
})
