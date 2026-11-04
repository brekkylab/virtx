// The Node package as a user installs it, on the platform this runs on: the library it opens
// is the one its platform's package carries, and a call into it answers.
//
// Run from a project that installed `@brekkylab/virtx`, so it resolves the way that project's
// code would.
import { createRequire } from 'node:module'
import path from 'node:path'

const require = createRequire(path.join(process.cwd(), 'noop.js'))
const failures = []
const check = (ok, what) => {
  console.log(`${ok ? 'PASS' : 'FAIL'} ${what}`)
  if (!ok) failures.push(what)
}

const virtx = require('@brekkylab/virtx')
check(typeof virtx.Client === 'function', `the package loads from ${path.dirname(require.resolve('@brekkylab/virtx'))}`)

// Opening the library is the first call that needs it; a server that cannot start is the one
// answer that needs no server.
try {
  virtx.Client.spawn(path.join(process.cwd(), 'no-such-virtx-server'))
  check(false, 'spawning a missing server throws')
} catch (e) {
  check(e instanceof virtx.VirtxError && /starting the server/.test(e.message), `spawning a missing server throws: ${e.message}`)
}
const opened = virtx.libraryPath()
check(
  opened !== null && opened.includes(`${path.sep}@brekkylab${path.sep}virtx-`),
  `the library is the platform package's: ${opened}`,
)

const digest = 'sha256:' + '0'.repeat(64)
check(virtx.imageName(digest).type === 'digest', 'an image name reads as a digest')

if (failures.length) {
  console.log(`\n${failures.length} failed`)
  process.exit(1)
}
