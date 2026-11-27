// Builds `libvirtx_core` from `virtx-cabi` and copies it out: beside `index.js`, which opens it
// there (`npm install` and `npm test` run this), or, with `--package <platform>`, into
// `npm/<platform>/` as that platform's package, which the release packs.
//
//   node build.js [--release] [--package <platform>]
//
// cargo is run as setuptools-rust runs it for `virtx-py`, so the two share one build in `target/`
// (on macOS all but the last link, to which setuptools-rust adds arguments of its own), and
// `CARGO_BUILD_TARGET` picks the target for both.

const { execFileSync } = require('node:child_process')
const fs = require('node:fs')
const path = require('node:path')

const NAMES = ['libvirtx_core.so', 'libvirtx_core.dylib', 'virtx_core.dll']

const args = process.argv.slice(2)
const release = args.includes('--release')
const platform = args.includes('--package') ? args[args.indexOf('--package') + 1] : null

const out = execFileSync(
  'cargo',
  [
    'rustc',
    '--lib',
    '--message-format=json-render-diagnostics',
    '--manifest-path',
    path.join(__dirname, '..', 'virtx-cabi', 'Cargo.toml'),
    ...(release ? ['--release'] : []),
    '--crate-type',
    'cdylib',
  ],
  { encoding: 'utf8', stdio: ['ignore', 'pipe', 'inherit'], maxBuffer: 256 << 20 },
)
const built = out
  .split('\n')
  .filter((line) => line.startsWith('{'))
  .map((line) => JSON.parse(line))
  .filter((message) => message.reason === 'compiler-artifact')
  .flatMap((message) => message.filenames)
  .find((file) => NAMES.includes(path.basename(file)))
if (!built) throw new Error('cargo built no libvirtx_core')

const dir = platform ? path.join(__dirname, 'npm', platform) : __dirname
fs.mkdirSync(dir, { recursive: true })
const file = path.basename(built)
// Removed first: macOS kills a process that opens a signed file overwritten in place.
for (const name of NAMES) fs.rmSync(path.join(dir, name), { force: true })
fs.copyFileSync(built, path.join(dir, file))

if (platform) {
  const root = require('./package.json')
  const name = `${root.name}-${platform}`
  const [os, cpu, abi] = platform.split('-')
  const manifest = {
    name,
    version: root.version,
    description: `${root.name}'s libvirtx_core for ${platform}.`,
    license: root.license,
    homepage: root.homepage,
    repository: root.repository,
    publishConfig: root.publishConfig,
    os: [os],
    cpu: [cpu],
    ...(abi === 'gnu' ? { libc: ['glibc'] } : {}),
    files: [file],
  }
  fs.writeFileSync(path.join(dir, 'package.json'), `${JSON.stringify(manifest, null, 2)}\n`)
  fs.writeFileSync(
    path.join(dir, 'README.md'),
    `# ${name}\n\nThe \`libvirtx_core\` of [\`${root.name}\`](https://www.npmjs.com/package/${root.name}) for ${platform}.\n`,
  )
  fs.copyFileSync(path.join(__dirname, '..', 'LICENSE'), path.join(dir, 'LICENSE'))
}
