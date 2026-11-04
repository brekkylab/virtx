'use strict'
// A virtx client for Node, over the C ABI of `libvirtx_core` (`virtx-core/include/virtx.h`),
// reached with koffi: the package has no addon of its own, and the library for its platform
// comes in that platform's package, `@brekkylab/virtx-<platform>`, an optional dependency.
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

// The package carrying the library for this platform, which npm installs only here.
const PLATFORM_PACKAGE = {
  'darwin-arm64': '@brekkylab/virtx-darwin-arm64',
  'linux-x64': '@brekkylab/virtx-linux-x64-gnu',
  'linux-arm64': '@brekkylab/virtx-linux-arm64-gnu',
  'win32-x64': '@brekkylab/virtx-win32-x64-msvc',
}[`${process.platform}-${process.arch}`]

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
// A request with a body of bytes beside its args.
const WITH_DATA = ['uintptr_t', ['uintptr_t', 'uint8_t *', 'size_t', 'uint8_t *', 'size_t']]
const SIGNATURES = {
  virtx_client_spawn: [
    'int',
    ['const char *', 'const char **', 'size_t', koffi.out(koffi.pointer('uintptr_t')), Out],
  ],
  virtx_client_connect: [
    'int',
    [
      'const char *',
      'uint8_t *',
      'size_t',
      'const char **',
      'size_t',
      koffi.out(koffi.pointer('uintptr_t')),
      Out,
    ],
  ],
  virtx_client_set_auth: ['int', ['uintptr_t', 'uint8_t *', 'size_t', Out]],
  virtx_client_set_headers: ['int', ['uintptr_t', 'const char **', 'size_t', Out]],
  virtx_client_retain: ['void', ['uintptr_t']],
  virtx_client_release: ['void', ['uintptr_t']],
  virtx_client_build_image: WITH_ARGS,
  virtx_client_pull_image: WITH_ARGS,
  virtx_client_push_image: WITH_ARGS,
  virtx_client_list_image_job: PLAIN,
  virtx_client_get_image_job: WITH_ARGS,
  virtx_client_remove_image_job: WITH_ARGS,
  virtx_client_image_job_log: WITH_ARGS,
  virtx_client_list_image: PLAIN,
  virtx_client_tag_image: WITH_ARGS,
  virtx_client_untag_image: WITH_ARGS,
  virtx_client_remove_image: WITH_ARGS,
  virtx_client_download_image: WITH_ARGS,
  virtx_client_upload_image: WITH_ARGS,
  virtx_client_prune_image: PLAIN,
  virtx_client_commit_image: WITH_ARGS,
  virtx_client_list_volume: PLAIN,
  virtx_client_create_volume: WITH_ARGS,
  virtx_client_get_volume: WITH_ARGS,
  virtx_client_remove_volume: WITH_ARGS,
  virtx_client_list_volume_mount: WITH_ARGS,
  virtx_client_create_volume_mount: WITH_ARGS,
  virtx_client_remove_volume_mount: WITH_ARGS,
  virtx_client_stat_file: WITH_ARGS,
  virtx_client_read_dir: WITH_ARGS,
  virtx_client_file_exists: WITH_ARGS,
  virtx_client_is_file: WITH_ARGS,
  virtx_client_is_dir: WITH_ARGS,
  virtx_client_read_file: WITH_ARGS,
  virtx_client_read_text: WITH_ARGS,
  virtx_client_write_file: WITH_DATA,
  virtx_client_create_dir: WITH_ARGS,
  virtx_client_create_dir_all: WITH_ARGS,
  virtx_client_remove_file: WITH_ARGS,
  virtx_client_rename_file: WITH_ARGS,
  virtx_client_copy_file: WITH_ARGS,
  virtx_client_list_sandbox: PLAIN,
  virtx_client_create_sandbox: WITH_ARGS,
  virtx_client_get_sandbox: WITH_ARGS,
  virtx_client_find_sandbox: WITH_ARGS,
  virtx_client_start_sandbox: WITH_ARGS,
  virtx_client_stop_sandbox: WITH_ARGS,
  virtx_client_open_sandbox_port: WITH_ARGS,
  virtx_client_remove_sandbox: WITH_ARGS,
  virtx_client_list_sandbox_job: WITH_ARGS,
  virtx_client_exec_sandbox: WITH_ARGS,
  virtx_client_get_sandbox_job: WITH_ARGS,
  virtx_client_remove_sandbox_job: WITH_ARGS,
  virtx_client_signal_sandbox_job: WITH_ARGS,
  virtx_client_write_sandbox_job_stdin: WITH_ARGS,
  virtx_client_sandbox_job_log: WITH_ARGS,
  virtx_client_exec_sandbox_log: WITH_ARGS,
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

  // What the library wrote for `status`, read and freed: the output, as JSON unless `raw`, or
  // thrown.
  take(status, out, raw = false) {
    let data = Buffer.alloc(0)
    if (out.data !== null) {
      data = Buffer.from(new Uint8Array(koffi.view(out.data, out.len)))
      this.virtx_bytes_free(out)
    }
    if (status === OK) return raw ? data : JSON.parse(data.toString('utf8'))
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

  // Await the future `handle`, freeing it after. Its answer is JSON, or a `Buffer` as it is
  // if `raw`.
  async answer(handle, raw = false) {
    try {
      for (;;) {
        const { status, out, woken } = this.poll(this.virtx_future_poll, handle)
        if (status !== PENDING) return this.take(status, out, raw)
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

// The library in this platform's package, or `null` where it is not installed.
function platformLibrary() {
  try {
    return path.join(path.dirname(require.resolve(`${PLATFORM_PACKAGE}/package.json`)), FILE_NAME)
  } catch {
    return null
  }
}

// The open library, opening it first if none is: `$VIRTX_CORE_LIB`, else one beside this file,
// else the one in this platform's package, else the platform's file name for the system's
// loader to look up.
function library() {
  if (opened === null) {
    const beside = path.join(__dirname, FILE_NAME)
    load(
      process.env[LIBRARY_ENV] ||
        (fs.existsSync(beside) ? beside : PLATFORM_PACKAGE && platformLibrary()) ||
        FILE_NAME,
    )
  }
  return opened
}

// What is sent as `null`, where leaving a member out would mean something else.
const NULL = Symbol('null')

// camelCase keys to the wire's snake_case, and back, through plain objects and arrays.
function rekey(value, key) {
  if (Array.isArray(value)) return value.map((item) => rekey(item, key))
  if (value === null || typeof value !== 'object' || ArrayBuffer.isView(value)) return value
  const rekeyed = {}
  for (const [name, item] of Object.entries(value)) {
    if (item === NULL) rekeyed[key(name)] = null
    else if (item !== undefined && item !== null) rekeyed[key(name)] = rekey(item, key)
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
  return DIGEST.test(image) ? { type: 'digest', digest: image } : { type: 'tag', tag: image }
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

// A job as it starts: a promise of the job, and an async iterable of its log's entries, read once
// the job has started, until its `end` entry. The job starts when it is made, once however it is
// used; leaving the loop leaves the job running.
class StartedJob extends Promise {
  // `then` and the rest answer plain promises.
  static get [Symbol.species]() {
    return Promise
  }

  constructor(executor, log) {
    super(executor)
    this._log = log
  }

  // `start()`, the request that answers the job, sent as the call returns, as a promise's work
  // is; `log`, its log by the job. Given `run()`, one iterated at once -- before that -- starts
  // the job and reads its log as one: `run` yields `{ job }` first, then the log's entries.
  static of(start, log, run = null) {
    let started = null
    let settle = null
    const job = new StartedJob((resolve, reject) => {
      settle = { resolve, reject }
    }, log)
    job._start = () => {
      if (!started) {
        started = start()
        started.then(settle.resolve, settle.reject)
      }
    }
    job._run = run && (() => (started ? null : ((started = true), run(settle))))
    queueMicrotask(job._start)
    return job
  }

  then(resolved, rejected) {
    this._start?.()
    return super.then(resolved, rejected)
  }

  async *[Symbol.asyncIterator]() {
    const items = this._run?.()
    if (!items) {
      yield* this._log(await this)
      return
    }
    yield* items
  }
}

// Each hold not let go of, released when its client is collected.
const holds = new FinalizationRegistry(({ lib, raw }) => {
  lib.virtx_client_release.async(raw, () => {})
})

// `auth` as `virtx_client_connect` takes it: JSON, or nothing for none.
function authBytes(auth) {
  return auth ? Buffer.from(JSON.stringify(auth)) : Buffer.alloc(0)
}

// How a refusal with `404` reads, as `libvirtx_core` words it.
const NOT_FOUND = 'the server answered 404'

// A sandbox as the client answered it, its members those of the wire, and that client's: `openPort`
// is sent over it, and `await using` deletes the sandbox on leaving.
class Sandbox {
  constructor(client, fields) {
    Object.defineProperty(this, '_client', { value: client })
    Object.assign(this, fields)
  }

  // Opens the port as `openSandboxPort` does, and keeps the answer in `ports`, in place of one
  // for the same port.
  async openPort(port, options = {}) {
    const opened = await this._client.openSandboxPort(this.id, port, options)
    const ports = (this.ports ??= [])
    const at = ports.findIndex((known) => known.port === port)
    if (at === -1) ports.push(opened)
    else ports[at] = opened
    return opened
  }
}

// `await using`, where the runtime has it: deletes it as `removeSandbox` does, and leaves one
// deleted already so.
if (Symbol.asyncDispose) {
  Sandbox.prototype[Symbol.asyncDispose] = async function () {
    try {
      await this._client.removeSandbox(this.id)
    } catch (e) {
      if (!String(e.message).startsWith(NOT_FOUND)) throw e
    }
  }
}

function opening(lib, status, raw, error) {
  if (status === OK) return new Client(raw[0], lib)
  lib.take(status, error)
  throw new VirtxError(`unknown status ${status}`)
}

// A client of one server, over stdio or HTTP, made by `libvirtx_core`.
//
// Each method returns a promise, or an async iterable for a request that streams: a job's log,
// or a download's or upload's progress. One that starts a job returns a `StartedJob`, both.
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

  // Reach the server at `url`, its base `http` or `https` URL, sending
  // `auth`, the credentials it wants as `{ type: 'bearer', token }` or null for none, and
  // `headers`, a name to a value, whatever else it asks of each request, with every request.
  // `Authorization` is `auth`'s, over one given as a header. Refused when a token would go in
  // the clear: over `http` to any host but this machine.
  static connect(url, auth = null, headers = {}) {
    const lib = library()
    const bytes = authBytes(auth)
    const strings = Object.entries(headers).flat()
    const raw = [0]
    const error = {}
    const status = lib.virtx_client_connect(
      url,
      bytes,
      bytes.length,
      strings,
      strings.length / 2,
      raw,
      error,
    )
    return opening(lib, status, raw, error)
  }

  // Reach the server `VIRTX_URL` names, with `VIRTX_API_KEY` and `VIRTX_ORG` if set: with
  // `VIRTX_ORG` and no key, as the person signed in with `virtx login`, whose token
  // `virtx token --json` prints, again as it runs out.
  // Without `VIRTX_URL`, virtx cloud under `VIRTX_DOMAIN` (`DOMAIN` by default): the cell
  // `VIRTX_REGION` names (`apne2`), or else `run.<domain>`, every cell's name, which DNS answers
  // with the cell nearest this machine. A sandbox of another cell is reached through it as well.
  static fromEnv() {
    const key = process.env.VIRTX_API_KEY || null
    const org = process.env.VIRTX_ORG || null
    const domain = process.env.VIRTX_DOMAIN || null
    const region = process.env.VIRTX_REGION || null
    let url = process.env.VIRTX_URL || null
    if (!url && region) url = `https://${region}.api.${domain || DOMAIN}`
    if (!url && (key || org)) url = `https://run.${domain || DOMAIN}`
    if (!url) throw new VirtxError('VIRTX_URL is not set, and no cell of virtx cloud was found')
    const auth = key ? { type: 'bearer', token: key } : org ? SIGNED_IN : null
    return Client.connect(url, auth, org ? { [ORG_HEADER]: org } : {})
  }

  // Send `auth` with every request from now on, in place of what was given at `connect`: a
  // new token, when the server's are short-lived. A forwarder already open sends it with its
  // next connection. Over stdio, where none is sent, nothing changes. Refused as at `connect`,
  // the auth unchanged.
  setAuth(auth) {
    const bytes = authBytes(auth)
    const error = {}
    const status = this._lib.virtx_client_set_auth(this._held(), bytes, bytes.length, error)
    if (status !== OK) this._lib.take(status, error)
  }

  // Send `headers`, a name to a value, with every request from now on, in place of those given
  // at `connect`. A forwarder already open sends them with its next connection. Over stdio,
  // where none are sent, nothing changes. A header that cannot be sent throws, the headers
  // unchanged.
  setHeaders(headers) {
    const strings = Object.entries(headers).flat()
    const error = {}
    const status = this._lib.virtx_client_set_headers(
      this._held(),
      strings,
      strings.length / 2,
      error,
    )
    if (status !== OK) this._lib.take(status, error)
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

  async _call(name, args, raw = false) {
    const start = this._lib[name]
    const held = this._held()
    let handle
    if (args === undefined) {
      handle = start(held)
    } else {
      const bytes = encode(args)
      handle = start(held, bytes, bytes.length)
    }
    const answer = await this._lib.answer(handle, raw)
    return raw ? answer : rekey(answer, camel)
  }

  async *_items(name, args) {
    const bytes = encode(args)
    const handle = this._lib[name](this._held(), bytes, bytes.length)
    for await (const item of this._lib.items(handle)) yield rekey(item, camel)
  }

  _imageJob(name, args) {
    return StartedJob.of(
      () => this._call(name, args),
      (job) => this.imageJobLog(job.id),
    )
  }

  // Starts the build: the job, and its log as `imageJobLog`'s.
  buildImage(recipe, reference) {
    const declared = recipe instanceof Recipe ? recipe.toJSON() : recipe
    return this._imageJob('virtx_client_build_image', { recipe: declared, reference })
  }

  pullImage(reference, auth) {
    return this._imageJob('virtx_client_pull_image', { reference, auth })
  }

  // Starts making an image of the sandbox: the job, and its log as `imageJobLog`'s.
  commitImage(sandbox, reference) {
    return this._imageJob('virtx_client_commit_image', { sandbox, reference })
  }

  pushImage(reference, auth) {
    return this._imageJob('virtx_client_push_image', { reference, auth })
  }

  listImageJob() {
    return this._call('virtx_client_list_image_job')
  }

  getImageJob(id) {
    return this._call('virtx_client_get_image_job', { id })
  }

  // Cancels the job if it runs, and forgets it.
  async removeImageJob(id) {
    await this._call('virtx_client_remove_image_job', { id })
  }

  // The job's log entries, from its start or after the entry numbered `after`, ending after
  // its `end` entry.
  imageJobLog(id, after) {
    return this._items('virtx_client_image_job_log', { id, after })
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
    return this._items('virtx_client_download_image', { reference, path })
  }

  uploadImage(path, reference) {
    return this._items('virtx_client_upload_image', { path, reference })
  }

  pruneImage() {
    return this._call('virtx_client_prune_image')
  }

  listVolume() {
    return this._call('virtx_client_list_volume')
  }

  createVolume(name) {
    return this._call('virtx_client_create_volume', { name })
  }

  getVolume(name) {
    return this._call('virtx_client_get_volume', { name })
  }

  async removeVolume(name) {
    await this._call('virtx_client_remove_volume', { name })
  }

  listVolumeMount(volume) {
    return this._call('virtx_client_list_volume_mount', { volume })
  }

  createVolumeMount(volume, at, source) {
    return this._call('virtx_client_create_volume_mount', { volume, path: at, source })
  }

  async removeVolumeMount(volume, at) {
    await this._call('virtx_client_remove_volume_mount', { volume, path: at })
  }

  // What is at `path` in `root`, `{ volume }`, the path relative to its root
  // as `dir/file.txt`.
  statFile(root, at) {
    return this._call('virtx_client_stat_file', { ...root, path: at })
  }

  readDir(root, at) {
    return this._call('virtx_client_read_dir', { ...root, path: at })
  }

  // The file at `path`, from `offset`, `length` bytes of it or all that is left, as a `Buffer`.
  readFile(root, at, offset, length) {
    return this._call('virtx_client_read_file', { ...root, path: at, offset, length }, true)
  }

  // Writes `content` (a `Buffer`, a `Uint8Array` or a string, as UTF-8) as the whole file at
  // `path`, creating it if it does not exist. Its parent directory must exist.
  // Whether anything, a file, or a directory is at `at`.
  async exists(root, at) {
    return await this._call('virtx_client_file_exists', { ...root, path: at })
  }

  async isFile(root, at) {
    return await this._call('virtx_client_is_file', { ...root, path: at })
  }

  async isDir(root, at) {
    return await this._call('virtx_client_is_dir', { ...root, path: at })
  }

  // The whole file as text, which it has to be: UTF-8.
  async readText(root, at) {
    return await this._call('virtx_client_read_text', { ...root, path: at })
  }

  // With `ifMatch`, the `etag` a stat gave, the write goes only while the file still has it,
  // and is refused with a `412` otherwise.
  async writeFile(root, at, content, ifMatch) {
    const bytes = encode({ ...root, path: at, ifMatch })
    const data = Buffer.isBuffer(content) ? content : Buffer.from(content)
    const handle = this._lib.virtx_client_write_file(
      this._held(),
      bytes,
      bytes.length,
      data,
      data.length,
    )
    await this._lib.answer(handle)
  }

  async createDir(root, at) {
    await this._call('virtx_client_create_dir', { ...root, path: at })
  }

  async createDirAll(root, at) {
    await this._call('virtx_client_create_dir_all', { ...root, path: at })
  }

  // Removes what is at `path`: a directory with everything in it.
  // With `missingOk`, nothing being there is fine, as `rm -f`; without it, a `404`.
  async removeFile(root, at, missingOk = false) {
    await this._call('virtx_client_remove_file', {
      ...root,
      path: at,
      missingOk: missingOk || undefined,
    })
  }

  // Moves what is at `from` to `to` in the same root, replacing what is at `to` if `overwrite`,
  // and refused if not and something is there.
  async renameFile(root, from, to, overwrite = false) {
    await this._call('virtx_client_rename_file', { ...root, from, to, overwrite })
  }

  // Copies what is at `from` to `to`, a directory with everything in it, as `renameFile`
  // replaces what is at `to`.
  async copyFile(root, from, to, overwrite = false) {
    await this._call('virtx_client_copy_file', { ...root, from, to, overwrite })
  }

  async _sandbox(name, args) {
    const answer = await this._call(name, args)
    return answer === null ? undefined : new Sandbox(this, answer)
  }

  async listSandbox() {
    const items = await this._call('virtx_client_list_sandbox')
    return items.map((item) => new Sandbox(this, item))
  }

  createSandbox(image, options = {}) {
    const autoDeleteDays = options.autoDeleteDays === 'never' ? NULL : options.autoDeleteDays
    return this._sandbox('virtx_client_create_sandbox', {
      ...options,
      autoDeleteDays,
      image: imageName(image),
    })
  }

  getSandbox(id) {
    return this._sandbox('virtx_client_get_sandbox', { id })
  }

  // The sandbox named `name`, or `undefined` if none is.
  findSandbox(name) {
    return this._sandbox('virtx_client_find_sandbox', { name })
  }

  // Starts it if it is stopped; answered once it runs, with where its ports are now.
  startSandbox(id) {
    return this._sandbox('virtx_client_start_sandbox', { id })
  }

  // Stops it if it runs, its disk kept -- or, for an ephemeral one, deleted with it.
  stopSandbox(id) {
    return this._sandbox('virtx_client_stop_sandbox', { id })
  }

  // TCP unless `protocol` says `'http'`; opened already, it is answered as it is, `label`
  // renaming it.
  openSandboxPort(id, port, { label, protocol } = {}) {
    return this._call('virtx_client_open_sandbox_port', { id, port, label, protocol })
  }

  // Deletes the sandbox: stopped first if it runs, and its disk removed.
  async removeSandbox(id) {
    await this._call('virtx_client_remove_sandbox', { id })
  }

  listSandboxJob(sandbox) {
    return this._call('virtx_client_list_sandbox_job', { sandbox })
  }

  // Starts `cmd` in the sandbox, its stdin open for `writeSandboxJobStdin` if `stdin`, killed
  // after `timeoutMs` if given: the job, and its output as `sandboxJobLog`'s.
  execSandbox(sandbox, cmd, options = {}) {
    const args = { ...options, sandbox, cmd }
    const items = this._items.bind(this)
    return StartedJob.of(
      () => this._call('virtx_client_exec_sandbox', args),
      (job) => this.sandboxJobLog(sandbox, job.id),
      async function* (settle) {
        let answered = false
        try {
          for await (const item of items('virtx_client_exec_sandbox_log', args)) {
            if (item.job) {
              answered = true
              settle.resolve(item.job)
            } else {
              yield item
            }
          }
          if (!answered) settle.reject(new VirtxError("the job's log ended before its job"))
        } catch (e) {
          if (!answered) settle.reject(e)
          throw e
        }
      },
    )
  }

  getSandboxJob(sandbox, job) {
    return this._call('virtx_client_get_sandbox_job', { sandbox, job })
  }

  // Cancels the job if it runs, killing its process, and forgets it.
  async removeSandboxJob(sandbox, job) {
    await this._call('virtx_client_remove_sandbox_job', { sandbox, job })
  }

  // Sends `signal`, a name such as `'SIGTERM'`, to the job's process and every one it started.
  async signalSandboxJob(sandbox, job, signal) {
    await this._call('virtx_client_signal_sandbox_job', { sandbox, job, signal })
  }

  // Writes `data`, text, to the job's stdin, then closes it if `eof`; resolves once written.
  async writeSandboxJobStdin(sandbox, job, data, eof = false) {
    await this._call('virtx_client_write_sandbox_job_stdin', {
      sandbox,
      job,
      data,
      eof: eof || undefined,
    })
  }

  // The job's log entries, from its start or after the entry numbered `after`, ending after
  // its `end` entry.
  sandboxJobLog(sandbox, job, after) {
    return this._items('virtx_client_sandbox_job_log', { sandbox, job, after })
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

  // Forward the port a sandbox answered `url` for, as `createSandbox` answers one in `ports`,
  // over `client`'s session. Blocks until it is open.
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

// Where virtx cloud is, by default: `run.<DOMAIN>`, every cell's name, and `<cell>.api.<DOMAIN>`
// for one.
const DOMAIN = 'virtx.sh'

// The header a request names its organization in, on a server that keeps organizations apart
// (PROTOCOL.md, Authentication).
const ORG_HEADER = 'virtx-org'

// The token of the person signed in with `virtx login`, as `virtx token --json` prints it.
const SIGNED_IN = { type: 'command', program: 'virtx', args: ['token', '--json'] }

module.exports = {
  Client,
  DOMAIN,
  LIBRARY_ENV,
  PortForwarder,
  Recipe,
  Sandbox,
  StartedJob,
  VirtxError,
  imageName,
  libraryPath,
  load,
}
