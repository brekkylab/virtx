// Builds `libvirtx_core` with cargo from the crate beside this directory, in the `dev` profile,
// and puts it beside `index.js`, which opens it there: what `npm install` and `npm test` run, to
// install and test the package from its source. Nothing where `$VIRTX_CORE_LIB` names a library,
// which `index.js` opens instead. The published packages carry the library built already, and
// neither this nor the scripts that run it.

import { execFileSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const NAMES = ['libvirtx_core.so', 'libvirtx_core.dylib', 'virtx_core.dll']

if (!process.env.VIRTX_CORE_LIB) {
  const here = path.dirname(fileURLToPath(import.meta.url))
  const manifest = path.join(here, '..', 'virtx-core', 'Cargo.toml')
  const out = execFileSync(
    'cargo',
    [
      'rustc',
      '--manifest-path',
      manifest,
      '--lib',
      '--crate-type',
      'cdylib',
      '--message-format=json-render-diagnostics',
    ],
    { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'], maxBuffer: 64 << 20 },
  )
  const built = out
    .split('\n')
    .filter((line) => line.startsWith('{'))
    .map((line) => JSON.parse(line))
    .filter((message) => message.reason === 'compiler-artifact')
    .flatMap((message) => message.filenames)
    .find((file) => NAMES.includes(path.basename(file)))
  if (!built) throw new Error('cargo built no libvirtx_core')
  for (const name of NAMES) fs.rmSync(path.join(here, name), { force: true })
  fs.copyFileSync(built, path.join(here, path.basename(built)))
}
