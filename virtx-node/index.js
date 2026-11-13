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
//
// What the library warns of beside an answer, such as a forwarded connection that reached nothing
// in its sandbox, is emitted as a process warning of the type `VirtxWarning`, queued onto the main
// thread as a wake is.

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
  constructor(message, status = null, body = null, code = null) {
    super(message)
    this.name = 'VirtxError'
    // The HTTP status the server refused the request with, if it did.
    this.status = status
    // The protocol's code it refused with, where it answered one: `not_found`, `stopped`,
    // `in_use` and the like -- what to branch on, not the message.
    this.code = code
    // The server's response body, where it was not a JSON error.
    this.body = body
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
const Warn = koffi.proto('void VirtxWarn(const char *message)')

const PLAIN = ['uintptr_t', ['uintptr_t']]
const WITH_ARGS = ['uintptr_t', ['uintptr_t', 'uint8_t *', 'size_t']]
// A request with a body of bytes beside its args.
const WITH_DATA = ['uintptr_t', ['uintptr_t', 'uint8_t *', 'size_t', 'uint8_t *', 'size_t']]
const SIGNATURES = {
  virtx_ensure_virtx: ['int', [Out]],
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
  virtx_client_close_sandbox_port: WITH_ARGS,
  virtx_client_get_sandbox_port_inbound: WITH_ARGS,
  virtx_client_set_sandbox_port_inbound: WITH_ARGS,
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
  virtx_port_forwarder_last_error: ['int', ['uintptr_t', Out]],
  virtx_port_forwarder_free: ['void', ['uintptr_t']],
  virtx_set_warn: ['void', [koffi.pointer(Warn)]],
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
const warn = koffi.register(
  (message) => process.emitWarning(message, 'VirtxWarning'),
  koffi.pointer(Warn),
)

class Library {
  constructor(file) {
    this.path = file
    const lib = koffi.load(file)
    this.lib = lib
    for (const [name, [result, args]] of Object.entries(SIGNATURES)) {
      this[name] = lib.func(name, result, args)
    }
    this.virtx_set_warn(warn)
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
      if (!data.length) throw new VirtxError('the call failed')
      const failure = JSON.parse(data.toString('utf8'))
      throw new VirtxError(
        failure.message,
        failure.status ?? null,
        failure.body ?? null,
        failure.code ?? null,
      )
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
    // Read as one with its log, a start that fails is the loop's to throw: the job's own
    // promise, rejected with it, is not left unheard -- which would end the process.
    Promise.prototype.then.call(this, undefined, () => {})
    yield* items
  }
}

// Each hold not let go of, released when its client is collected.
const holds = new FinalizationRegistry(({ lib, raw }) => {
  lib.virtx_client_release.async(raw, () => {})
})

// Fetch `virtx-stdio`, the server of the library's version, into this host's cache if it is not
// there, and give its path: what `Client.spawn()` without a program starts. The fetch runs on a
// worker thread, off the event loop.
//
// The server is kept one directory per version, so a library that is upgraded fetches its own.
// `$VIRTX_STDIO` names a server to use as it is; `$VIRTX_STDIO_VERSION` another version;
// `$VIRTX_DIST_URL` where releases are fetched from; `$VIRTX_HOME` the cache.
function ensureVirtx() {
  const lib = library()
  const out = {}
  return new Promise((resolve, reject) => {
    lib.virtx_ensure_virtx.async(out, (error, status) => {
      if (error) return reject(error)
      try {
        resolve(lib.take(status, out))
      } catch (e) {
        reject(e)
      }
    })
  })
}

// `auth` as `virtx_client_connect` takes it: JSON, or nothing for none.
function authBytes(auth) {
  return auth ? Buffer.from(JSON.stringify(auth)) : Buffer.alloc(0)
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
  // Without `program`, the server is `virtx-stdio` of this library's version, fetched first if
  // this host has none -- blocking for the fetch; `ensureVirtx` does that fetch alone, off the
  // event loop.
  static spawn(program = null, args = []) {
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
      () => this._call(name, args).then((info) => new ImageJob(this, info)),
      (job) => job.log(),
    )
  }

  // Starts the build: the job, and its log as the job's `log`.
  buildImage(recipe, reference) {
    const declared = recipe instanceof Recipe ? recipe.toJSON() : recipe
    return this._imageJob('virtx_client_build_image', { recipe: declared, reference })
  }

  pullImage(reference, auth) {
    return this._imageJob('virtx_client_pull_image', { reference, auth })
  }

  // Starts making an image of `sandbox`, a handle or its id: the job, and its log as the job's
  // `log`.
  commitImage(sandbox, reference) {
    const id = sandbox instanceof Sandbox ? sandbox.id : sandbox
    return this._imageJob('virtx_client_commit_image', { sandbox: id, reference })
  }

  pushImage(reference, auth) {
    return this._imageJob('virtx_client_push_image', { reference, auth })
  }

  async listImageJob() {
    const infos = await this._call('virtx_client_list_image_job')
    return infos.map((info) => new ImageJob(this, info))
  }

  async getImageJob(id) {
    return new ImageJob(this, await this._call('virtx_client_get_image_job', { id }))
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

  async listVolume() {
    const infos = await this._call('virtx_client_list_volume')
    return infos.map((info) => new Volume(this, info))
  }

  async createVolume(name) {
    return new Volume(this, await this._call('virtx_client_create_volume', { name }))
  }

  async getVolume(name) {
    return new Volume(this, await this._call('virtx_client_get_volume', { name }))
  }

  async listSandbox() {
    const infos = await this._call('virtx_client_list_sandbox')
    return infos.map((info) => new Sandbox(this, info))
  }

  // Each of `volumes`' `volume` is a handle or its name. `autoDeleteDays: 'never'` keeps it until
  // it is deleted.
  async createSandbox(image, options = {}) {
    const { autoDeleteDays, volumes } = options
    const info = await this._call('virtx_client_create_sandbox', {
      ...options,
      image: imageName(image),
      autoDeleteDays: autoDeleteDays === 'never' ? NULL : autoDeleteDays,
      volumes: volumes?.map((v) =>
        v.volume instanceof Volume ? { ...v, volume: v.volume.name } : v,
      ),
    })
    return new Sandbox(this, info)
  }

  async getSandbox(id) {
    return new Sandbox(this, await this._call('virtx_client_get_sandbox', { id }))
  }

  // The sandbox named `name`, or `undefined` if none is.
  async findSandbox(name) {
    const info = await this._call('virtx_client_find_sandbox', { name })
    return info ? new Sandbox(this, info) : undefined
  }

  // Starts it if it is stopped; answered once it runs, with where its ports are now.
  async startSandbox(id) {
    return new Sandbox(this, await this._call('virtx_client_start_sandbox', { id }))
  }

  // Stops it if it runs, its disk kept -- or, for an ephemeral one, deleted with it.
  async stopSandbox(id) {
    return new Sandbox(this, await this._call('virtx_client_stop_sandbox', { id }))
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
// forwarder is carried to the port in a WebSocket. A connection that reaches nothing in the
// sandbox -- its WebSocket refused, or closed before the port sent a byte, as it is when nothing
// listens there -- is closed, and why is `lastError`, and emitted as a `VirtxWarning`.
class PortForwarder {
  constructor(raw, lib, address) {
    this._raw = raw
    this._lib = lib
    // Where the port is reached, as `host:port`.
    this.address = address
    this._token = {}
    forwarders.register(this, { lib, raw }, this._token)
  }

  // Forward `port`, as `createSandbox` answers one in `ports`, or the `url` it answered for it,
  // over `client`'s session. Given the port, what a connection that fails says names its number.
  // Returns once its local address is open, which takes no request of the server.
  static open(client, port) {
    const lib = client._lib
    const bytes = encode(
      typeof port === 'string' ? { url: port } : { url: port.url, port: port.port },
    )
    const raw = [0]
    const out = {}
    const status = lib.virtx_port_forwarder_open(client._held(), bytes, bytes.length, raw, out)
    const { address } = lib.take(status, out)
    return new PortForwarder(raw[0], lib, address)
  }

  // Why the last connection to the forwarder that reached nothing in the sandbox failed, its
  // message naming the port, as a `VirtxError`; `null` while none has. Its `status` and `code`
  // are the server's, where it refused the connection's WebSocket.
  get lastError() {
    if (this._raw === null) throw new VirtxError('the forwarder is closed')
    const out = {}
    const status = this._lib.virtx_port_forwarder_last_error(this._raw, out)
    try {
      this._lib.take(status, out)
    } catch (error) {
      if (error instanceof VirtxError) return error
      throw error
    }
    return null
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

// What the server answered about something, and the client that reaches it. The client is not
// enumerable, so it reads, logs and compares as what the server answered.
function held(handle, client, info) {
  Object.defineProperty(handle, 'client', { value: client })
  Object.assign(handle, info)
}

// The files of a volume or a sandbox, at its `_root`, the `{ volume }` or `{ sandbox }` each
// request names. A path is relative to the root, as `dir/file.txt`.
class Files {
  _call(name, args, raw = false) {
    return this.client._call(name, { ...this._root, ...args }, raw)
  }

  // What is at `at`.
  statFile(at) {
    return this._call('virtx_client_stat_file', { path: at })
  }

  // Whether anything, a file, or a directory is at `at`.
  exists(at) {
    return this._call('virtx_client_file_exists', { path: at })
  }

  isFile(at) {
    return this._call('virtx_client_is_file', { path: at })
  }

  isDir(at) {
    return this._call('virtx_client_is_dir', { path: at })
  }

  readDir(at) {
    return this._call('virtx_client_read_dir', { path: at })
  }

  // The file at `at`, from `offset`, `length` bytes of it or all that is left, as a `Buffer`.
  readFile(at, offset, length) {
    return this._call('virtx_client_read_file', { path: at, offset, length }, true)
  }

  // The whole file as text, which it has to be: UTF-8.
  readText(at) {
    return this._call('virtx_client_read_text', { path: at })
  }

  // Writes `content` (a `Buffer`, a `Uint8Array` or a string, as UTF-8) as the whole file at
  // `at`, creating it if it does not exist. Its parent directory must exist. With `ifMatch`, the
  // `etag` a stat gave, the write goes only while the file still has it, and is refused with a
  // `412` otherwise.
  async writeFile(at, content, ifMatch) {
    const { _lib: lib } = this.client
    const bytes = encode({ ...this._root, path: at, ifMatch })
    const data = Buffer.isBuffer(content) ? content : Buffer.from(content)
    await lib.answer(
      lib.virtx_client_write_file(this.client._held(), bytes, bytes.length, data, data.length),
    )
  }

  async createDir(at) {
    await this._call('virtx_client_create_dir', { path: at })
  }

  async createDirAll(at) {
    await this._call('virtx_client_create_dir_all', { path: at })
  }

  // Removes what is at `at`: a directory with everything in it. With `missingOk`, nothing being
  // there is fine, as `rm -f`; without it, a `404`.
  async removeFile(at, missingOk = false) {
    await this._call('virtx_client_remove_file', { path: at, missingOk: missingOk || undefined })
  }

  // Moves what is at `from` to `to`, replacing what is at `to` if `overwrite`, and refused if not
  // and something is there.
  async renameFile(from, to, overwrite = false) {
    await this._call('virtx_client_rename_file', { from, to, overwrite })
  }

  // Copies what is at `from` to `to`, a directory with everything in it, as `renameFile`
  // replaces what is at `to`.
  async copyFile(from, to, overwrite = false) {
    await this._call('virtx_client_copy_file', { from, to, overwrite })
  }
}

// A build, pull or push the server runs (`id`, `kind`, `reference`, `running`, `digest`,
// `error`), and the client that reaches it. Dropping it leaves the job; `remove` removes it.
class ImageJob {
  constructor(client, info) {
    held(this, client, info)
  }

  // It as the server has it now.
  async get() {
    return new ImageJob(
      this.client,
      await this.client._call('virtx_client_get_image_job', { id: this.id }),
    )
  }

  // Its log entries, from its start or after the entry numbered `after`, ending after its `end`
  // entry.
  log(after) {
    return this.client._items('virtx_client_image_job_log', { id: this.id, after })
  }

  // Cancels it if it runs, and forgets it.
  async remove() {
    await this.client._call('virtx_client_remove_image_job', { id: this.id })
  }
}

// A volume the server holds (`name`, `size`), and the client that reaches it: its files and the
// mounts in it. Dropping it leaves the volume; `remove` removes it.
class Volume extends Files {
  constructor(client, info) {
    super()
    held(this, client, info)
  }

  get _root() {
    return { volume: this.name }
  }

  // The mounts in it, without their credentials.
  mounts() {
    return this.client._call('virtx_client_list_volume_mount', { volume: this.name })
  }

  // Mounts `source` at `at` in it, a directory the mount creates.
  mount(at, source) {
    return this.client._call('virtx_client_create_volume_mount', {
      volume: this.name,
      path: at,
      source,
    })
  }

  // Takes away the mount at `at`, leaving its source as it is.
  async unmount(at) {
    await this.client._call('virtx_client_remove_volume_mount', { volume: this.name, path: at })
  }

  // Deletes it and everything in it.
  async remove() {
    await this.client._call('virtx_client_remove_volume', { name: this.name })
  }
}

// A sandbox the server holds, as making it or asking for it answered (`id`, `name`, `image`,
// `state`, `ephemeral`, `autoStopMinutes`, `autoDeleteDays`, `usedAt`, `ports`), and the client
// that reaches it, to run processes in it and read and write its files, its root as its
// processes see it. Its disk -- what it writes over its image -- outlives its VM: it stops when
// unused or told to, and starts again as it was left, under the same id, until it is deleted.
// Dropping it leaves the sandbox; `remove` removes it.
class Sandbox extends Files {
  constructor(client, info) {
    super()
    held(this, client, info)
  }

  get _root() {
    return { sandbox: this.id }
  }

  // Starts `cmd` in it, its stdin open for `writeStdin` if `stdin`, killed after `timeoutMs` if
  // given: the job, and its output as its `log`. Iterated at once, the job is started and its
  // log read as one request.
  exec(cmd, options = {}) {
    const { client, id } = this
    const args = { ...options, sandbox: id, cmd }
    const handle = (info) => new SandboxJob(client, id, info)
    return StartedJob.of(
      () => client._call('virtx_client_exec_sandbox', args).then(handle),
      (job) => job.log(),
      async function* (settle) {
        let answered = false
        try {
          for await (const item of client._items('virtx_client_exec_sandbox_log', args)) {
            if (item.job) {
              answered = true
              settle.resolve(handle(item.job))
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

  // Every job in it.
  async jobs() {
    const infos = await this.client._call('virtx_client_list_sandbox_job', { sandbox: this.id })
    return infos.map((info) => new SandboxJob(this.client, this.id, info))
  }

  // The job `id` in it.
  async job(id) {
    const info = await this.client._call('virtx_client_get_sandbox_job', {
      sandbox: this.id,
      job: id,
    })
    return new SandboxJob(this.client, this.id, info)
  }

  // Starts it if it is stopped; answered once it runs, with where its ports are now.
  start() {
    return this.client.startSandbox(this.id)
  }

  // Stops it if it runs, its disk kept -- or, for an ephemeral one, deleted with it.
  stop() {
    return this.client.stopSandbox(this.id)
  }

  // Opens the port `port` in it, as those it was made with are, TCP unless `protocol` says
  // `'http'`, with `inbound` its inbound rules: where it is reached from, kept in `ports` in
  // place of one for the same port. Opened already, it is answered as it is, `label` renaming it.
  async openPort(port, { label, protocol, inbound } = {}) {
    const opened = await this.client._call('virtx_client_open_sandbox_port', {
      id: this.id,
      port,
      label,
      protocol,
      inbound,
    })
    this.ports = (this.ports ?? []).filter((known) => known.port !== port).concat([opened])
    return opened
  }

  // Closes the port `port` in it: its URL reaches it no more, and it is not opened again as it
  // starts; dropped from `ports`.
  async closePort(port) {
    await this.client._call('virtx_client_close_sandbox_port', { id: this.id, port })
    if (this.ports) this.ports = this.ports.filter((known) => known.port !== port)
  }

  // The inbound rules of its port `port`, and its public address if it has one: virtx cloud's.
  inbound(port) {
    return this.client._call('virtx_client_get_sandbox_port_inbound', { id: this.id, port })
  }

  // Makes the inbound rules of its port `port` `rules`, all of them -- `[]` lets in no one --
  // answered as the server keeps them: its public `address` kept on the port in `ports` too.
  async setInbound(port, rules) {
    const inbound = await this.client._call('virtx_client_set_sandbox_port_inbound', {
      id: this.id,
      port,
      rules,
    })
    if (this.ports) {
      this.ports = this.ports.map((known) => {
        if (known.port !== port) return known
        const { address: _, ...rest } = known
        return inbound.address == null ? rest : { ...rest, address: inbound.address }
      })
    }
    return inbound
  }

  // Starts making an image of it, to point `reference` at: the job, and its log as the job's
  // `log`.
  commit(reference) {
    return this.client.commitImage(this.id, reference)
  }

  // Deletes it: stopped first if it runs, and its disk removed. Its volumes stay.
  async remove() {
    await this.client._call('virtx_client_remove_sandbox', { id: this.id })
  }
}

// `await using sandbox = await client.createSandbox(...)`, where the runtime has it: deletes it
// on leaving, as `remove` does, and leaves one deleted already so.
if (Symbol.asyncDispose) {
  Sandbox.prototype[Symbol.asyncDispose] = async function () {
    try {
      await this.remove()
    } catch (e) {
      if (e.status !== 404) throw e
    }
  }
}

// A process run in a sandbox (`id`, `kind`, `pid`, `cmd`, `running`, `code`), and the client and
// sandbox, `sandboxId`, that reach it. Dropping it leaves the job; `remove` removes it.
class SandboxJob {
  constructor(client, sandbox, info) {
    Object.defineProperty(this, 'sandboxId', { value: sandbox })
    held(this, client, info)
  }

  _call(name, args) {
    return this.client._call(name, { sandbox: this.sandboxId, job: this.id, ...args })
  }

  // It as the server has it now.
  async get() {
    return new SandboxJob(
      this.client,
      this.sandboxId,
      await this._call('virtx_client_get_sandbox_job'),
    )
  }

  // Sends `signal`, a name such as `'SIGTERM'`, to its process and every one it started.
  async signal(signal) {
    await this._call('virtx_client_signal_sandbox_job', { signal })
  }

  // Writes `data`, text, to its stdin, then closes it if `eof`; resolves once written.
  async writeStdin(data, eof = false) {
    await this._call('virtx_client_write_sandbox_job_stdin', { data, eof: eof || undefined })
  }

  // Its log entries, from its start or after the entry numbered `after`, ending after its `end`
  // entry.
  log(after) {
    return this.client._items('virtx_client_sandbox_job_log', {
      sandbox: this.sandboxId,
      job: this.id,
      after,
    })
  }

  // Cancels it if it runs, killing its process, and forgets it.
  async remove() {
    await this._call('virtx_client_remove_sandbox_job')
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
  Files,
  ImageJob,
  LIBRARY_ENV,
  PortForwarder,
  Recipe,
  Sandbox,
  SandboxJob,
  StartedJob,
  VirtxError,
  Volume,
  ensureVirtx,
  imageName,
  libraryPath,
  load,
  // For `cli.js`, which runs the library's `virtx` command; not part of the client's API.
  _library: library,
}
