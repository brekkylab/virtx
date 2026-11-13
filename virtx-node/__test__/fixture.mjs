// What every test file stands on, as Python's `conftest.py` is: `libvirtx_core`, built with
// cargo and opened, unless `$VIRTX_CORE_LIB` names one; the servers `cargo xtask fixture`
// serves, as it names them in the environment, which `npm test` runs the tests under; and the
// steps tests share.

import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

import virtx from '../index.js'

export const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..')
const target = process.env.CARGO_TARGET_DIR || path.join(root, 'target')
const library =
  process.platform === 'win32'
    ? 'virtx_core.dll'
    : process.platform === 'darwin'
      ? 'libvirtx_core.dylib'
      : 'libvirtx_core.so'
execFileSync('cargo', ['build', '-p', 'virtx-core', '--lib'], { cwd: root, stdio: 'inherit' })
virtx.load(process.env[virtx.LIBRARY_ENV] || path.join(target, 'debug', library))

// What `cargo xtask fixture` names in the environment: `stdio`, the path of `virtx-stdio`, and
// `url`, `token` and `project`, the `virtx-server-tcp` it serves.
export function fixture() {
  const named = {}
  for (const [key, name] of [
    ['stdio', 'VIRTX_TEST_STDIO'],
    ['url', 'VIRTX_TEST_URL'],
    ['token', 'VIRTX_TEST_TOKEN'],
    ['project', 'VIRTX_TEST_PROJECT'],
  ]) {
    assert.ok(
      process.env[name],
      `${name} is not set: run these with \`npm test\`, under \`cargo xtask fixture\``,
    )
    named[key] = process.env[name]
  }
  return named
}

// The image every test that boots a sandbox pulls first.
export const ALPINE = 'alpine:3.20'

let names = 0
// A name no other test gives, of this file or another: each file runs as a process of its own.
export function unique(what) {
  return `test-${what}-${process.pid}-${names++}`
}

// `reference` without the registry and namespace Docker Hub's names are filled in with, which a
// server may answer: `docker.io/library/alpine:3.20` is `alpine:3.20`.
export function familiar(reference) {
  if (!reference.startsWith('docker.io/')) return reference
  const rest = reference.slice('docker.io/'.length)
  const name = rest.startsWith('library/') ? rest.slice('library/'.length) : rest
  return name.includes('/') ? rest : name
}

// Every tag on the server's images, as the tests name it, with the digest it points at.
export async function tags(client) {
  return Object.fromEntries(
    (await client.listImage()).map((image) => [familiar(image.reference), image.digest]),
  )
}

// `ALPINE` on the server, pulled unless it is there already, and its digest. Docker Hub limits
// the pulls an address makes, so a test that only needs the image takes the one there.
export async function alpine(client) {
  const there = (await tags(client))[ALPINE]
  if (there) return there
  const end = (await Array.fromAsync(client.pullImage(ALPINE))).at(-1).end
  assert.ok(end.digest, end.error)
  return end.digest
}
