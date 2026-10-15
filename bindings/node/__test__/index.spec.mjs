import assert from 'node:assert/strict'
import { randomUUID } from 'node:crypto'
import { mkdtempSync, mkdirSync, readFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'
import { setTimeout as sleep } from 'node:timers/promises'
import { createRequire } from 'node:module'

const virtx = createRequire(import.meta.url)('../index.js')
const { ConsoleClient, Directory, ImageClient, ImageSource, Recipe, Step } = virtx

const tempDir = () => mkdtempSync(join(tmpdir(), 'virtx-'))

test('a Recipe is a value', () => {
  const base = new Recipe('python:3.12-slim')
  const extended = base.step('pip install duckdb').step(Step.env('TZ', 'UTC'))

  assert.match(extended.toString(), /python:3\.12-slim/)
  assert.match(extended.toString(), /duckdb/)
  assert.doesNotMatch(base.toString(), /duckdb/)
})

test('a Recipe from a Dockerfile', () => {
  const image = Recipe.fromDockerfile('FROM alpine:3.20\nRUN apk add jq\n')
  assert.match(image.toString(), /alpine:3\.20/)
})

test('ImageSource', () => {
  const recipe = new Recipe('alpine:3.20').step('apk add jq')
  assert.ok(ImageSource.recipe(recipe).equals(ImageSource.recipe(new Recipe('alpine:3.20', ['apk add jq']))))
  assert.ok(!ImageSource.reference('myimg:latest').equals(ImageSource.digest('myimg:latest')))
  assert.equal(ImageSource.digest('sha256:0123').toString(), 'ImageSource.digest("sha256:0123")')
  assert.equal(recipe.base, 'alpine:3.20')
})

test('a port is spelled the way docker spells it', () => {
  const builder = ConsoleClient.builder().network(true).ports(['8080:80', '5901:5900'])
  assert.throws(() => builder.ports(['5900']), { code: 'INVALID_ARG' })
  assert.throws(() => builder.ports(['8080:0']), { code: 'INVALID_ARG' })
  assert.throws(() => builder.ports(['http']), { code: 'INVALID_ARG' })
})

test('jobs are exported', () => {
  for (const name of ['Job', 'ExecStream']) assert.equal(typeof virtx[name], 'function', name)
})

test('a Directory refuses a file under a mount', () => {
  const directory = new Directory().withMount('project', tempDir())
  assert.throws(() => directory.addFile('project/notes.md', 'under a mount'), { code: 'InvalidInput' })
})

test('a HostMount serves the Directory', { skip: !virtx.HostMount && 'built without `mount`' }, () => {
  const mountpoint = join(tempDir(), 'mnt')
  mkdirSync(mountpoint)
  const directory = new Directory().withFile('notes/today.md', Buffer.from('ship the release'))

  const mount = new virtx.HostMount(directory, mountpoint)
  assert.equal(mount.mountpoint, mountpoint)
  assert.equal(readFileSync(join(mountpoint, 'notes', 'today.md'), 'utf8'), 'ship the release')
  // The mount owns the tree now.
  assert.throws(() => directory.addFile('more.md', ''), { code: 'INVALID_ARG' })
})

test('building without a server fails and spends the builder', async () => {
  // Point the default server lookup (virtx's cache) at an empty directory.
  const home = process.env.VIRTX_HOME
  process.env.VIRTX_HOME = tempDir()
  try {
    const builder = ConsoleClient.builder()
    await assert.rejects(builder.build(), { code: 'VIRTX_ERROR' })
    assert.throws(() => builder.vcpus(2), { code: 'INVALID_ARG' })
  } finally {
    if (home === undefined) delete process.env.VIRTX_HOME
    else process.env.VIRTX_HOME = home
  }
})

test('building against a missing binary fails', async () => {
  await assert.rejects(
    ConsoleClient.builder().cmd(['virtx-no-such-console-server']).build(),
    { code: 'VIRTX_ERROR' },
  )
})

test('an image client against a missing binary fails', async () => {
  await assert.rejects(ImageClient.tryFromCmd(['virtx-no-such-console-server']), { code: 'CONSOLE_BROKEN' })
})

// Against a real console server, named by `$VIRTX_CONSOLE` (`virtx-uvm`, say).
const SERVER = process.env.VIRTX_CONSOLE

test('exec, read and write', { skip: !SERVER && 'set $VIRTX_CONSOLE' }, async () => {
  const console_ = await ConsoleClient.builder()
    .cmd([SERVER])
    .image(new Recipe('python:3.12-slim-trixie'))
    .mount(tempDir(), '/work')
    .network(false)
    .build()
  try {
    assert.deepEqual(console_.mounts, ['/work'])

    assert.equal(await console_.write('/work/hello.txt', 'hi'), 2)
    const result = await console_.exec(['cat', '/work/hello.txt'])
    assert.equal(result.code, 0)
    assert.equal(result.stdout.toString(), 'hi')

    const read = await console_.read('/work/hello.txt')
    assert.equal(read.data.toString(), 'hi')
    assert.equal(read.size, 2)
  } finally {
    await console_.close()
  }
})

test('build, list and remove', { skip: !SERVER && 'set $VIRTX_CONSOLE' }, async () => {
  const images = await ImageClient.tryFromCmd([SERVER])
  try {
    assert.ok(await images.version())

    const built = await images.build(new Recipe('alpine:3.20'), 'virtx-node-test:latest')
    assert.equal(built.reference, 'virtx-node-test:latest')
    assert.ok((await images.list()).some((entry) => entry.digest === built.digest))

    await images.remove(ImageSource.reference(built.reference))
    assert.ok((await images.list()).every((entry) => !entry.refs.includes(built.reference)))
  } finally {
    await images.close()
  }
})

/** A BSON document of string members: enough for a notification frame. */
const stringsDoc = (members) => {
  const body = Buffer.concat(
    Object.entries(members).map(([k, v]) => {
      const len = Buffer.alloc(4)
      len.writeInt32LE(Buffer.byteLength(v) + 1)
      return Buffer.concat([Buffer.from([2]), Buffer.from(k + '\0'), len, Buffer.from(v + '\0')])
    }),
  )
  const size = Buffer.alloc(4)
  size.writeInt32LE(body.length + 5)
  return Buffer.concat([size, body, Buffer.from([0])])
}

test('_relay answers a frame on the console it relays to', { skip: !SERVER && 'set $VIRTX_CONSOLE' }, async () => {
  const console_ = await ConsoleClient.builder().cmd([SERVER]).image(new Recipe('alpine:3.20')).build()

  // `quit` is the owner's to say: relayed, it is dropped, and the session goes on.
  assert.equal((await console_._relay(stringsDoc({ jsonrpc: '2.0', method: 'quit' }))).length, 0)
  assert.equal((await console_.exec(['true'])).code, 0)

  await assert.rejects(console_._relay(Buffer.from('not a message')), { code: 'CONSOLE_BROKEN' })

  await console_.close()
  await assert.rejects(console_._relay(Buffer.alloc(0)), { message: /closed/ })
})

/** A booted console for the job tests. */
const jobConsole = async () => {
  const console_ = await ConsoleClient.builder().cmd([SERVER]).image(new Recipe('alpine:3.20')).build()
  // Booted first, so the boot is not counted against what a test times.
  await console_.exec(['true'])
  return console_
}

test('a command printing once a second is seen once a second', { skip: !SERVER && 'set $VIRTX_CONSOLE' }, async () => {
  const console_ = await jobConsole()
  try {
    const ticks = []
    const codes = []
    for await (const chunk of console_.execStream(['sh', '-c', 'for i in 1 2 3; do echo tick $i; sleep 1; done'])) {
      const now = performance.now()
      for (const line of chunk.stdout.toString().split('\n').filter(Boolean)) ticks.push([line, now])
      codes.push(chunk.code)
    }

    assert.deepEqual(ticks.map(([line]) => line), ['tick 1', 'tick 2', 'tick 3'])
    for (let i = 1; i < ticks.length; i++) assert.ok(ticks[i][1] - ticks[i - 1][1] > 500)
    // Only the last chunk carries the code.
    assert.equal(codes.at(-1), 0)
    assert.ok(codes.slice(0, -1).every((code) => code === undefined))
  } finally {
    await console_.close()
  }
})

test('a timeout keeps the output up to the kill', { skip: !SERVER && 'set $VIRTX_CONSOLE' }, async () => {
  const cmd = ['sh', '-c', 'echo started; echo err >&2; sleep 5']
  const console_ = await jobConsole()
  try {
    await assert.rejects(console_.exec(cmd, 1000), (err) => {
      assert.equal(err.code, 'TIMED_OUT')
      assert.equal(err.stdout.toString(), 'started\n')
      assert.equal(err.stderr.toString(), 'err\n')
      assert.equal(err.truncated, false)
      return true
    })

    // A stream ends in the same refusal, with what the chunks before it did not carry.
    let stdout = ''
    let stderr = ''
    await assert.rejects(
      (async () => {
        for await (const chunk of console_.execStream(cmd, 1000)) {
          stdout += chunk.stdout
          stderr += chunk.stderr
        }
      })(),
      (err) => {
        assert.equal(err.code, 'TIMED_OUT')
        assert.equal(stdout + err.stdout, 'started\n')
        assert.equal(stderr + err.stderr, 'err\n')
        return true
      },
    )
  } finally {
    await console_.close()
  }
})

test('a console answers while a job runs', { skip: !SERVER && 'set $VIRTX_CONSOLE' }, async () => {
  const log = `/tmp/virtx-${randomUUID()}.log`
  const console_ = await jobConsole()
  try {
    const job = await console_.spawn(['sh', '-c', `echo partial | tee ${log}; sleep 3; echo done`])
    const started = performance.now()
    const first = await job.poll(2000)
    assert.equal(first.stdout.toString(), 'partial\n')
    assert.equal(first.code, undefined)

    // The log the job is writing, read while it runs.
    assert.equal((await console_.read(log)).data.toString(), 'partial\n')
    assert.equal((await console_.exec(['echo', 'meanwhile'])).stdout.toString(), 'meanwhile\n')
    assert.ok(performance.now() - started < 3000)

    let stdout = ''
    let chunk
    do {
      chunk = await job.poll(1000)
      stdout += chunk.stdout
    } while (chunk.code === undefined)
    assert.equal(stdout, 'done\n')
    assert.equal(chunk.code, 0)

    // Its end was reported, so the server holds it no longer.
    await assert.rejects(job.poll(), { code: 'UNKNOWN_JOB' })
  } finally {
    await console_.close()
  }
})

test('leaving a stream early kills its command', { skip: !SERVER && 'set $VIRTX_CONSOLE' }, async () => {
  const console_ = await jobConsole()
  const size = async (path) => (await console_.read(path)).size
  try {
    const beat = `/tmp/virtx-${randomUUID()}`
    // `break` calls the stream's `return()`, which kills the command before it settles.
    for await (const chunk of console_.execStream(['sh', '-c', `while :; do echo beat; echo x >> ${beat}; sleep 0.2; done`])) {
      if (chunk.stdout.includes('beat')) break
    }
    const before = await size(beat)
    await sleep(1000)
    assert.equal(await size(beat), before)
  } finally {
    await console_.close()
  }
})

test('a job is discarded when its await using block is left', { skip: (!SERVER && 'set $VIRTX_CONSOLE') || (!Symbol.asyncDispose && 'this Node has no Symbol.asyncDispose') }, async () => {
  const console_ = await jobConsole()
  try {
    const job = await console_.spawn(['sleep', '60'])
    assert.ok((await job.poll()).code == null)
    // What `await using` calls at the end of its block; spelled out, since older Node cannot
    // parse the declaration.
    await job[Symbol.asyncDispose]()
    await assert.rejects(job.poll(), { code: 'UNKNOWN_JOB' })
  } finally {
    await console_.close()
  }
})

test('a job let go of is reached again by its id', { skip: !SERVER && 'set $VIRTX_CONSOLE' }, async () => {
  const console_ = await jobConsole()
  try {
    const { id } = await console_.spawn(['sh', '-c', 'echo hi; sleep 60'])
    const job = console_.job(id)
    assert.equal(job.id, id)
    assert.equal((await job.poll(5000)).stdout.toString(), 'hi\n')
    await job.discard()
    await assert.rejects(console_.job(id).poll(), { code: 'UNKNOWN_JOB' })
    // A job already gone leaves nothing to do.
    await job.discard()
  } finally {
    await console_.close()
  }
})
