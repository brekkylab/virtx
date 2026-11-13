// Pack the Node packages as they are published: a package per platform carrying its
// `libvirtx_core`, and `@brekkylab/virtx` with each of them as an optional dependency, which
// npm installs only on the platform it names.
//
//   node scripts/packages/pack-npm.mjs <lib-dir> <out-dir> [--allow-missing]
//
// `<lib-dir>/<platform>/` holds each platform's library, as the `pipeline` workflow's platform
// jobs lay it out. `--allow-missing` packs only the platforms that have one, and still names every
// one in the root: each of those jobs packs its own platform so, and a release packs the root
// alone so, from an empty `<lib-dir>`, beside what the platforms packed.
import { execFileSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const PLATFORMS = {
  'darwin-arm64': { os: 'darwin', cpu: 'arm64', file: 'libvirtx_core.dylib' },
  'linux-x64-gnu': { os: 'linux', cpu: 'x64', libc: 'glibc', file: 'libvirtx_core.so' },
  'linux-arm64-gnu': { os: 'linux', cpu: 'arm64', libc: 'glibc', file: 'libvirtx_core.so' },
  'win32-x64-msvc': { os: 'win32', cpu: 'x64', file: 'virtx_core.dll' },
}

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..')
const node = path.join(repo, 'virtx-node')
const libs = path.resolve(process.argv[2])
const out = path.resolve(process.argv[3])
const allowMissing = process.argv.includes('--allow-missing')
fs.mkdirSync(out, { recursive: true })

const root = JSON.parse(fs.readFileSync(path.join(node, 'package.json'), 'utf8'))
const common = {
  version: root.version,
  license: root.license,
  repository: { type: 'git', url: 'git+https://github.com/brekkylab/virtx.git', directory: 'virtx-node' },
  homepage: 'https://github.com/brekkylab/virtx/tree/main/virtx-node#readme',
  publishConfig: { registry: 'https://registry.npmjs.org/', access: 'public' },
}

// Packs `files` from their sources under `manifest`, staged in a directory of its own, with
// the repository's license, which npm packs whatever `files` says.
function pack(manifest, files) {
  const staged = fs.mkdtempSync(path.join(out, 'stage-'))
  for (const [name, from] of Object.entries(files)) fs.copyFileSync(from, path.join(staged, name))
  fs.copyFileSync(path.join(repo, 'LICENSE'), path.join(staged, 'LICENSE'))
  fs.writeFileSync(path.join(staged, 'package.json'), JSON.stringify(manifest, null, 2) + '\n')
  execFileSync('npm', ['pack', '--pack-destination', out], {
    cwd: staged,
    stdio: 'inherit',
    shell: process.platform === 'win32',
  })
  fs.rmSync(staged, { recursive: true })
}

const optional = {}
let packed = 0
for (const [platform, { os, cpu, libc, file }] of Object.entries(PLATFORMS)) {
  const name = `${root.name}-${platform}`
  optional[name] = root.version
  const lib = path.join(libs, platform, file)
  if (!fs.existsSync(lib)) {
    if (allowMissing) {
      console.log(`skipping ${name}: no ${lib}`)
      continue
    }
    throw new Error(`no ${lib}`)
  }
  const readme = path.join(out, `README-${platform}.md`)
  fs.writeFileSync(readme, `# ${name}\n\nThe \`libvirtx_core\` of [\`${root.name}\`](https://www.npmjs.com/package/${root.name}) for ${platform}.\n`)
  pack(
    {
      name,
      ...common,
      description: `${root.name}'s libvirtx_core for ${platform}.`,
      os: [os],
      cpu: [cpu],
      ...(libc ? { libc: [libc] } : {}),
      files: [file],
    },
    { [file]: lib, 'README.md': readme },
  )
  fs.rmSync(readme)
  packed++
}

const files = Object.fromEntries(root.files.map((file) => [file, path.join(node, file)]))
// Without what only this repository uses: the tools that lint it, and the script that tests it.
const { devDependencies, scripts, ...published } = root
pack({ ...published, ...common, optionalDependencies: optional }, { ...files, 'README.md': path.join(node, 'README.md') })
console.log(`packed ${packed + 1} packages into ${out}`)
