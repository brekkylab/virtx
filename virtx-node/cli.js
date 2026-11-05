#!/usr/bin/env node
'use strict'
// The `virtx` command: the one `libvirtx_core` carries, run with this process's arguments.
//
// Nothing here touches `process.stdout` or `process.stderr`: opening either can make the
// descriptor non-blocking, and the library writes to it directly.

const { _library, VirtxError } = require('./index.js')

let lib
try {
  lib = _library().lib
} catch (e) {
  if (!(e instanceof VirtxError)) throw e
  require('node:fs').writeSync(2, `virtx: ${e.message}\n`)
  process.exit(1)
}
const run = lib.func('virtx_cli_main', 'int', ['const char **', 'size_t'])
const argv = ['virtx', ...process.argv.slice(2)]
// Ctrl-C is the command's to take while it runs: it cancels the job it interrupts.
process.exit(run(argv, argv.length))
