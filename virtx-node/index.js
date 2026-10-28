'use strict'
// A virtx client for Node, over the C ABI of `libvirtx_core` (`virtx-core/include/virtx.h`),
// reached with koffi: the package has no addon of its own, and carries the library for its
// platform beside this file.
//
// One library per process: every client is made by the one first opened, which stays open until
// the process ends. A client handed to another module is only good in the same library, so a
// module that shares one opens the path `libraryPath()` answers.
//
// A request is polled with a waker the library wakes from its own threads. koffi queues that
// call onto the main thread, so a wake only runs once the event loop does: nothing here blocks
// the main thread on the library's threads, and releasing a client, which waits for them, runs
// on a worker.

const fs = require('node:fs')
const path = require('node:path')
const koffi = require('koffi')

const OK = 0
const ERROR = -1
const PENDING = 1
const END = 2

// The variable naming the library to open when `load` was not called first.
const LIBRARY_ENV = 'VIRTX_CORE_LIB'

const FILE_NAME =
  process.platform === 'win32'
    ? 'virtx_core.dll'
    : process.platform === 'darwin'
      ? 'libvirtx_core.dylib'
      : 'libvirtx_core.so'

class VirtxError extends Error {
  constructor(message) {
    super(message)
    this.name = 'VirtxError'
  }
}

// Handles (`VirtxClient*`, `VirtxFuture*`, `VirtxStream*`, `VirtxPortForwarder*`) are held as
// their addresses.
const VirtxBytes = koffi.struct('VirtxBytes', { data: 'uint8_t *', len: 'size_t' })
const Callback = koffi.proto('void VirtxCallback(uintptr_t data)')
const VirtxWaker = koffi.struct('VirtxWaker', {
  data: 'uintptr_t',
  wake: koffi.pointer(Callback),
  release: koffi.pointer(Callback),
})
const Out = koffi.out(koffi.pointer(VirtxBytes))

const PLAIN = ['uintptr_t', ['uintptr_t']]
const WITH_ARGS = ['uintptr_t', ['uintptr_t', 'uint8_t *', 'size_t']]
const SIGNATURES = {
  virtx_client_spawn: [
    'int',
    ['const char *', 'const char **', 'size_t', koffi.out(koffi.pointer('uintptr_t')), Out],
  ],
  virtx_client_connect: ['int', ['const char *', koffi.out(koffi.pointer('uintptr_t')), Out]],
  virtx_client_retain: ['void', ['uintptr_t']],
  virtx_client_release: ['void', ['uintptr_t']],
  virtx_client_build_image: WITH_ARGS,
  virtx_client_pull_image: WITH_ARGS,
  virtx_client_push_image: WITH_ARGS,
  virtx_client_list_image: PLAIN,
  virtx_client_tag_image: WITH_ARGS,
  virtx_client_untag_image: WITH_ARGS,
  virtx_client_remove_image: WITH_ARGS,
  virtx_client_download_image: WITH_ARGS,
  virtx_client_upload_image: WITH_ARGS,
  virtx_client_prune_image: PLAIN,
  virtx_client_list_snapshot: PLAIN,
  virtx_client_create_snapshot: WITH_ARGS,
  virtx_client_remove_snapshot: WITH_ARGS,
  virtx_client_list_volume: PLAIN,
  virtx_client_create_volume: WITH_ARGS,
  virtx_client_remove_volume: WITH_ARGS,
  virtx_client_list_volume_mount: WITH_ARGS,
  virtx_client_create_volume_mount: WITH_ARGS,
  virtx_client_remove_volume_mount: WITH_ARGS,
  virtx_client_list_sandbox: PLAIN,
  virtx_client_init_sandbox: WITH_ARGS,
  virtx_client_exec_sandbox: WITH_ARGS,
  virtx_client_close_sandbox: WITH_ARGS,
  virtx_port_forwarder_open: [
    'int',
    ['uintptr_t', 'uint8_t *', 'size_t', koffi.out(koffi.pointer('uintptr_t')), Out],
  ],
  virtx_port_forwarder_free: ['void', ['uintptr_t']],
  virtx_bytes_free: ['void', [VirtxBytes]],
  virtx_future_poll: ['int', ['uintptr_t', VirtxWaker, Out]],
  virtx_future_free: ['void', ['uintptr_t']],
  virtx_stream_poll_next: ['int', ['uintptr_t', VirtxWaker, Out]],
  virtx_stream_free: ['void', ['uintptr_t']],
}

// The pending polls, by the key their waker carries as its `data`: what settles each.
const pending = new Map()
let nextKey = 1
// Holds the event loop open while a poll waits on a wake, which no handle of Node's does.
let keepAlive = null

function settle(key, release) {
  const resolve = pending.get(key)
  if (release) {
    pending.delete(key)
    if (pending.size === 0 && keepAlive !== null) {
      clearInterval(keepAlive)
      keepAlive = null
    }
  }
  if (resolve !== undefined) resolve()
}

// Registered once and kept for the process: wakers are only ever these two.
const wake = koffi.register((key) => settle(key, false), koffi.pointer(Callback))
const release = koffi.register((key) => settle(key, true), koffi.pointer(Callback))

class Library {
  constructor(file) {
    this.path = file
    const lib = koffi.load(file)
    for (const [name, [result, args]] of Object.entries(SIGNATURES)) {
      this[name] = lib.func(name, result, args)
    }
  }

  // What the library wrote for `status`, read and freed: the output, or thrown.
  take(status, out) {
    let data = Buffer.alloc(0)
    if (out.data !== null) {
      data = Buffer.from(new Uint8Array(koffi.view(out.data, out.len)))
      this.virtx_bytes_free(out)
    }
    if (status === OK) return JSON.parse(data.toString('utf8'))
    if (status === ERROR) {
      throw new VirtxError(data.length ? JSON.parse(data.toString('utf8')) : 'the call failed')
    }
    throw new VirtxError(`unknown status ${status}`)
  }

  poll(poll, handle) {
    const key = nextKey++
    const woken = new Promise((resolve) => pending.set(key, resolve))
    if (keepAlive === null) keepAlive = setInterval(() => {}, 1 << 30)
    const out = {}
    const status = poll(handle, { data: key, wake, release }, out)
    return { status, out, woken }
  }

  // Await the future `handle`, freeing it after.
  async answer(handle) {
    try {
      for (;;) {
        const { status, out, woken } = this.poll(this.virtx_future_poll, handle)
        if (status !== PENDING) return this.take(status, out)
        await woken
      }
    } finally {
      this.virtx_future_free(handle)
    }
  }

  // Each item of the stream `handle`, freeing it after, or on `return`, which cancels it.
  async *items(handle) {
    try {
      for (;;) {
        const { status, out, woken } = this.poll(this.virtx_stream_poll_next, handle)
        if (status === END) return
        if (status !== PENDING) {
          yield this.take(status, out)
          continue
        }
        await woken
      }
    } finally {
      this.virtx_stream_free(handle)
    }
  }
}

let opened = null

// Open the library at `file` for this process's clients. Opening the one already open does
// nothing; another is refused, since a process has one.
function load(file) {
  if (opened === null) {
    try {
      opened = new Library(file)
    } catch (e) {
      throw new VirtxError(`opening ${file}: ${e.message}`)
    }
  } else if (opened.path !== file) {
    throw new VirtxError(`${opened.path} is open already, so ${file} is not opened`)
  }
}

// The path the open library was opened by; `null` before the first client or `load`.
function libraryPath() {
  return opened === null ? null : opened.path
}

// The open library, opening it first if none is: `$VIRTX_CORE_LIB`, else the one in this
// package, else the platform's file name for the system's loader to look up.
function library() {
  if (opened === null) {
    const bundled = path.join(__dirname, FILE_NAME)
    load(process.env[LIBRARY_ENV] || (fs.existsSync(bundled) ? bundled : FILE_NAME))
  }
  return opened
}

// camelCase keys to the wire's snake_case, and back, through plain objects and arrays.
function rekey(value, key) {
  if (Array.isArray(value)) return value.map((item) => rekey(item, key))
  if (value === null || typeof value !== 'object' || ArrayBuffer.isView(value)) return value
  const rekeyed = {}
  for (const [name, item] of Object.entries(value)) {
    if (item !== undefined && item !== null) rekeyed[key(name)] = rekey(item, key)
  }
  return rekeyed
}
const snake = (name) => name.replace(/[A-Z]/g, (c) => `_${c.toLowerCase()}`)
const camel = (name) => name.replace(/_([a-z])/g, (_, c) => c.toUpperCase())

function encode(args) {
  return Buffer.from(JSON.stringify(rekey(args, snake)))
}

const DIGEST = /^(sha256:[0-9a-f]{64}|sha512:[0-9a-f]{128})$/

// `image` as the wire names one: a full `sha256:` or `sha512:` digest is a digest, any other
// string a tag. An object is taken as already named.
function imageName(image) {
  if (typeof image !== 'string') return image
  return DIGEST.test(image)
    ? { type: 'digest', digest: image }
    : { type: 'tag', tag: image }
}

// A base and the steps over it, which the server builds: `new Recipe('alpine:3.20').run('apk add
// jq')`. Each step returns the recipe, so they chain.
class Recipe {
  constructor(base, steps = []) {
    this.base = base
    this.steps = [...steps]
  }

  run(command) {
    this.steps.push({ run: command })
    return this
  }

  env(key, value) {
    this.steps.push({ env: { key, value } })
    return this
  }

  workdir(directory) {
    this.steps.push({ workdir: directory })
    return this
  }

  toJSON() {
    return { v: 1, base: this.base, steps: this.steps }
  }
}

// Each hold not let go of, released when its client is collected.
const holds = new FinalizationRegistry(({ lib, raw }) => {
  lib.virtx_client_release.async(raw, () => {})
})

function opening(lib, status, raw, error) {
  if (status === OK) return new Client(raw[0], lib)
  lib.take(status, error)
  throw new VirtxError(`unknown status ${status}`)
}

// A client of one server, over stdio or Streamable HTTP, made by `libvirtx_core`.
//
// Each method returns a promise, or an async iterable for a request that streams its progress.
// Holds of one session are counted: `clone` takes another, `close` lets this one go, and the
// session ends with the last, which answers every request still waiting with an error.
class Client {
  constructor(raw, lib) {
    this._raw = raw
    this._lib = lib
    this._token = {}
    holds.register(this, { lib, raw }, this._token)
  }

  // Start `program` with `args` as the server, and talk to it over its stdin and stdout.
  static spawn(program, args = []) {
    const lib = library()
    const raw = [0]
    const error = {}
    const status = lib.virtx_client_spawn(program, args, args.length, raw, error)
    return opening(lib, status, raw, error)
  }

  // Reach the server at `url`, its base `http` or `https` URL (`/rpc` is under it).
  static connect(url) {
    const lib = library()
    const raw = [0]
    const error = {}
    const status = lib.virtx_client_connect(url, raw, error)
    return opening(lib, status, raw, error)
  }

  // Take over one hold of the `VirtxClient*` `raw`, as `intoRaw` or C's `virtx_client_retain`
  // gave it, made by the library this process opened.
  static fromRaw(raw) {
    return new Client(raw, library())
  }

  // The `VirtxClient*` this holds, still held by it.
  asRaw() {
    return this._held()
  }

  // Give up this hold of the `VirtxClient*`, for whoever takes the pointer to release. This
  // client is closed after.
  intoRaw() {
    const raw = this._held()
    this._forget()
    return raw
  }

  // Another hold of the same session.
  clone() {
    const raw = this._held()
    this._lib.virtx_client_retain(raw)
    return new Client(raw, this._lib)
  }

  // Let go of this hold; the last ends the session, which the promise waits for. Closing twice
  // does nothing.
  close() {
    const raw = this._raw
    if (raw === null) return Promise.resolve()
    this._forget()
    return new Promise((resolve, reject) => {
      this._lib.virtx_client_release.async(raw, (error) => (error ? reject(error) : resolve()))
    })
  }

  _forget() {
    this._raw = null
    holds.unregister(this._token)
  }

  _held() {
    if (this._raw === null) throw new VirtxError('the client is closed')
    return this._raw
  }

  async _call(name, args) {
    const start = this._lib[name]
    const raw = this._held()
    let handle
    if (args === undefined) {
      handle = start(raw)
    } else {
      const bytes = encode(args)
      handle = start(raw, bytes, bytes.length)
    }
    return rekey(await this._lib.answer(handle), camel)
  }

  async *_events(name, args) {
    const bytes = encode(args)
    const handle = this._lib[name](this._held(), bytes, bytes.length)
    for await (const item of this._lib.items(handle)) {
      yield 'progress' in item
        ? { progress: rekey(item.progress, camel) }
        : { output: rekey(item.output, camel) }
    }
  }

  buildImage(recipe, reference) {
    const declared = recipe instanceof Recipe ? recipe.toJSON() : recipe
    return this._events('virtx_client_build_image', { recipe: declared, reference })
  }

  pullImage(reference, auth) {
    return this._events('virtx_client_pull_image', { reference, auth })
  }

  pushImage(reference, auth) {
    return this._events('virtx_client_push_image', { reference, auth })
  }

  listImage() {
    return this._call('virtx_client_list_image')
  }

  async tagImage(source, target) {
    await this._call('virtx_client_tag_image', { source, target })
  }

  async untagImage(reference) {
    await this._call('virtx_client_untag_image', { reference })
  }

  async removeImage(reference) {
    await this._call('virtx_client_remove_image', { reference })
  }

  downloadImage(reference, path) {
    return this._events('virtx_client_download_image', { reference, path })
  }

  uploadImage(path, reference) {
    return this._events('virtx_client_upload_image', { path, reference })
  }

  pruneImage() {
    return this._call('virtx_client_prune_image')
  }

  listSnapshot() {
    return this._call('virtx_client_list_snapshot')
  }

  createSnapshot(name, diskGib) {
    return this._call('virtx_client_create_snapshot', { name, diskGib })
  }

  async removeSnapshot(name) {
    await this._call('virtx_client_remove_snapshot', { name })
  }

  listVolume() {
    return this._call('virtx_client_list_volume')
  }

  createVolume(name) {
    return this._call('virtx_client_create_volume', { name })
  }

  async removeVolume(name) {
    await this._call('virtx_client_remove_volume', { name })
  }

  listVolumeMount(volume) {
    return this._call('virtx_client_list_volume_mount', { volume })
  }

  async createVolumeMount(volume, at, source) {
    await this._call('virtx_client_create_volume_mount', { volume, path: at, source })
  }

  async removeVolumeMount(volume, at) {
    await this._call('virtx_client_remove_volume_mount', { volume, path: at })
  }

  listSandbox() {
    return this._call('virtx_client_list_sandbox')
  }

  initSandbox(image, options = {}) {
    return this._call('virtx_client_init_sandbox', { ...options, image: imageName(image) })
  }

  execSandbox(sandbox, cmd, timeoutMs) {
    return this._events('virtx_client_exec_sandbox', { sandbox, cmd, timeoutMs })
  }

  async closeSandbox(sandbox) {
    await this._call('virtx_client_close_sandbox', { sandbox })
  }
}

// `await using`, where the runtime has it.
if (Symbol.asyncDispose) Client.prototype[Symbol.asyncDispose] = Client.prototype.close

// Each forwarder not closed, freed when it is collected.
const forwarders = new FinalizationRegistry(({ lib, raw }) => {
  lib.virtx_port_forwarder_free(raw)
})

// A sandbox's port at an address on this machine's loopback, until it is closed.
//
// It runs on its client's session, so the client's last close stops it. Over stdio the server is
// on this machine and its own address is the forwarder's; over HTTP each connection to the
// forwarder is carried to the port in a WebSocket.
class PortForwarder {
  constructor(raw, lib, address) {
    this._raw = raw
    this._lib = lib
    // Where the port is reached, as `host:port`.
    this.address = address
    this._token = {}
    forwarders.register(this, { lib, raw }, this._token)
  }

  // Forward the port a sandbox answered `url` for, as `initSandbox` and `listSandbox` answer one
  // in `ports`, over `client`'s session. Blocks until it is open.
  static open(client, url) {
    const lib = client._lib
    const bytes = encode({ url })
    const raw = [0]
    const out = {}
    const status = lib.virtx_port_forwarder_open(client._held(), bytes, bytes.length, raw, out)
    const { address } = lib.take(status, out)
    return new PortForwarder(raw[0], lib, address)
  }

  // The address's host: a loopback IP.
  get host() {
    return this.address.slice(0, this.address.lastIndexOf(':')).replace(/^\[|\]$/g, '')
  }

  // The address's port.
  get port() {
    return Number(this.address.slice(this.address.lastIndexOf(':') + 1))
  }

  // Stop forwarding. Closing twice does nothing.
  close() {
    if (this._raw === null) return
    forwarders.unregister(this._token)
    this._lib.virtx_port_forwarder_free(this._raw)
    this._raw = null
  }
}

// `using`, where the runtime has it.
if (Symbol.dispose) PortForwarder.prototype[Symbol.dispose] = PortForwarder.prototype.close

module.exports = {
  Client,
  LIBRARY_ENV,
  PortForwarder,
  Recipe,
  VirtxError,
  imageName,
  libraryPath,
  load,
}
