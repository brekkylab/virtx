// A virtx client over the C ABI of `libvirtx_core`. Field names are the wire's, camelCased.

/** The variable naming the library to open when `load` was not called first. */
export const LIBRARY_ENV: 'VIRTX_CORE_LIB'

/** A request failed: the server's error, the transport's, or the library's. */
export class VirtxError extends Error {}

/** Where virtx cloud is, by default: `run.<DOMAIN>`, every cell's name, and `<cell>.api.<DOMAIN>` for one. */
export const DOMAIN: string

/** Open the library at `file` for this process's clients; another than the one open is refused. */
export function load(file: string): void

/** The path the open library was opened by; `null` before the first client or `load`. */
export function libraryPath(): string | null

/** An image by a tag on it, as `name:tag`. */
export interface ImageTag {
  type: 'tag'
  tag: string
}

/** An image by the digest of its manifest, as `algorithm:hex`. */
export interface ImageDigest {
  type: 'digest'
  digest: string
}

/** An image, by a tag on it or by its digest. */
export type ImageName = ImageTag | ImageDigest

/** A full `sha256:` or `sha512:` digest is a digest, any other string a tag. */
export function imageName(image: string | ImageName): ImageName

export type Step = { run: string } | { env: { key: string; value: string } } | { workdir: string }

/** A base and the steps over it. Each step returns the recipe, so they chain. */
export class Recipe {
  constructor(base: string, steps?: Step[])
  base: string
  steps: Step[]
  /** A command, run through `sh -c` with every earlier `env`. */
  run(command: string): this
  /** A variable for every later `run` and the built image's config. */
  env(key: string, value: string): this
  /** Where later steps run, and the built image's working directory. */
  workdir(directory: string): this
  toJSON(): { v: 1; base: string; steps: Step[] }
}

/** A notification about a request: how far it has got, what that will reach when known, and its `content`, if it sent one, such as a line of its log. */
export interface Progress<C> {
  progress: number
  total?: number
  content?: C
}

/** Each of a request's notifications, then what it answered. */
export type Event<T, C> = { progress: Progress<C> } | { output: T }

export type ImageJobKind = 'pull' | 'push' | 'build' | 'commit'

/** A build, pull or push the server runs. */
export interface ImageJob {
  id: string
  kind: ImageJobKind
  /** The tag it points, or the image it pushes. */
  reference: string
  running: boolean
  /** The digest of the manifest, as `algorithm:hex`, once it has succeeded. */
  digest?: string
  /** Why it failed, once it has. */
  error?: string
}

/** How an image job ended. */
export type ImageJobEnd = { digest: string } | { error: string }

/** An entry of an image job's log, numbered by `id`; `end` is the last. */
export type ImageJobEntry = { id: number } & (
  { log: string } | { truncated: Record<string, never> } | { end: ImageJobEnd }
)

/** Credentials for a remote image registry. A `basic` password may be an access token the registry takes in its place. */
export type RegistryAuth =
  { type: 'basic'; username: string; password: string } | { type: 'bearer'; token: string }

/** The credentials a server wants with every request: a token, sent as `Authorization: Bearer <token>`. */
export type Auth =
  { type: 'bearer'; token: string } | { type: 'command'; program: string; args?: string[] }

export interface ImageListItem {
  /** The tag, as `name:tag`. */
  reference: string
  /** The digest of the manifest it points at, as `algorithm:hex`. */
  digest: string
}

export interface ImageDownloadOutput {
  /** The digest of the manifest written, as `algorithm:hex`. */
  digest: string
}

export interface ImageUploadOutput {
  /** The digest of the manifest uploaded, as `algorithm:hex`. */
  digest: string
}

export interface ImagePruneOutput {
  /** The digest of each image deleted, as `algorithm:hex`. */
  images: string[]
  /** How many bytes the server freed. */
  reclaimed: number
}

export interface VolumeListOutputItem {
  name: string
  /** The bytes the volume's files take on the server. */
  size: number
}

/** A mount source's type, without its credentials. */
export type MountType = 'notion' | 'gdrive' | 'onedrive'

/** The pages a Notion integration can reach. */
export interface NotionSource {
  type: 'notion'
  apiKey: string
}

/** A Google Drive. */
export interface GdriveSource {
  type: 'gdrive'
  clientId: string
  clientSecret: string
  refreshToken: string
}

/** A OneDrive. `clientSecret` is for a confidential app; a public one, as a personal account's usually is, has none. */
export interface OnedriveSource {
  type: 'onedrive'
  clientId: string
  clientSecret?: string
  refreshToken: string
}

/** What a mount shows, and the credentials to reach it. */
export type MountSource = NotionSource | GdriveSource | OnedriveSource

/** One mount in a volume. Its credentials are not answered. */
export interface VolumeMountListOutputItem {
  /** Where it is mounted. */
  path: string
  mountType: MountType
}

/** Whose files a request names: a volume's. */
export type FileRoot = { volume: string }

/** What a file or directory is. */
export interface Metadata {
  isDir: boolean
  /** Its size in bytes; `0` for a directory. */
  size: number
  /** When it was last modified, in seconds since the Unix epoch, if the server says. */
  modified?: number
  /** Its entity tag as an `ETag` header spells it, if the server gives one: what `writeFile` takes as `ifMatch`. */
  etag?: string
}

/** One entry of a directory. */
export interface DirEntry {
  /** Its name in the directory: one path segment. */
  name: string
  metadata: Metadata
}

/**
 * A sandbox, running or stopped. Its disk -- what it writes over its image -- outlives its VM: it
 * stops when unused or told to, and starts again as it was left, under the same id, until it is
 * deleted.
 */
export interface Sandbox {
  id: string
  /** The name it was given, which `findSandbox` finds it by. */
  name?: string
  /** The digest of the image it stands on: what it boots every time it starts. */
  image: string
  /** `'stopped'`: its VM down and its disk kept; a job started in it, or `startSandbox`, starts it. */
  state: 'running' | 'stopped'
  /** Whether it is deleted when it stops, rather than kept. */
  ephemeral: boolean
  /** How long it runs unused before it stops. */
  autoStopMinutes: number
  /** How long it is kept stopped and unused before it is deleted; absent, until it is deleted. */
  autoDeleteDays?: number
  /** When it was last used, as RFC 3339. */
  usedAt: string
  /** Only as `createSandbox` and `startSandbox` answer it, if it has some. */
  ports?: SandboxPort[]
}

/** A TCP port opened in a sandbox, and where it is reached from. */
export interface SandboxPort {
  port: number
  url: string
}

/** Whatever is left out the server picks. */
export interface SandboxCreateOptions {
  /** A name to find it again by, with `findSandbox`. */
  name?: string
  /** Delete it when it stops, rather than keep it. */
  ephemeral?: boolean
  autoStopMinutes?: number
  /** `'never'` keeps it until it is deleted. */
  autoDeleteDays?: number | 'never'
  /** How much it may write over its image, in GiB. */
  diskGib?: number
  network?: boolean
  /** TCP ports in it to open, reserved as it is made. */
  ports?: number[]
  vcpus?: number
  memoryMib?: number
  gpu?: boolean
  gpuMemoryMib?: number
  volumes?: { volume: string; path: string; readonly?: boolean }[]
}

export interface SandboxExecOptions {
  /** Keep its stdin open for `writeSandboxJobStdin`. */
  stdin?: boolean
  /** Kill it after this long. */
  timeoutMs?: number
}

/** A process started in a sandbox. */
export interface SandboxJob {
  id: string
  kind: 'exec'
  pid: number
  cmd: string[]
  running: boolean
  /** Its exit code, once it has ended. */
  code?: number
}

/** An entry of a sandbox job's log, numbered by `id`; `end` is the last. */
export type SandboxJobEntry = { id: number } & (
  | { stdout: string }
  | { stderr: string }
  | { truncated: Record<string, never> }
  | { end: { code: number } }
)

/**
 * A job as it starts: a promise of the job `J`, and an async iterable of its log's entries `E`,
 * read until its `end` entry. The job starts at the call, as a promise's work does, and once
 * however the value is used: awaited, iterated, or both. Each loop reads the log from its start;
 * leaving one leaves the job running, which `removeImageJob` or `removeSandboxJob` cancels.
 */
export class StartedJob<J, E> extends Promise<J> implements AsyncIterable<E> {
  [Symbol.asyncIterator](): AsyncGenerator<E>
}

/**
 * A client of one server, over stdio or HTTP, made by `libvirtx_core`. Holds of one
 * session are counted: `clone` takes another, `close` lets this one go, and the session ends with
 * the last.
 */
export class Client {
  /** Start `program` with `args` as the server, and talk to it over its stdin and stdout. */
  static spawn(program: string, args?: string[]): Client
  /**
   * Reach the server `VIRTX_URL` names, with `VIRTX_API_KEY` and `VIRTX_ORG` if set: with
   * `VIRTX_ORG` and no key, as the person signed in with `virtx login`, whose token
   * `virtx token --json` prints, again as it runs out. Without `VIRTX_URL`, virtx cloud under `VIRTX_DOMAIN` (`DOMAIN` by default): the cell `VIRTX_REGION`
   * names (`apne2`), or else `run.<domain>`, every cell's name, which DNS answers with the cell
   * nearest this machine.
   */
  static fromEnv(): Client
  /**
   * Reach the server at `url`, its base `http` or `https` URL, sending
   * `auth`, the credentials it wants, and `headers`, whatever else it asks of each request, with
   * every request. `Authorization` is `auth`'s, over one given as a header. Refused when a token
   * would go in the clear: over `http` to any host but this machine.
   */
  static connect(url: string, auth?: Auth | null, headers?: Record<string, string>): Client
  /**
   * Send `auth` with every request from now on, in place of what was given at `connect`: a new
   * token, when the server's are short-lived. A forwarder already open sends it with its next
   * connection. Over stdio, where none is sent, nothing changes. Refused as at `connect`, the
   * auth unchanged.
   */
  setAuth(auth: Auth | null): void
  /**
   * Send `headers` with every request from now on, in place of those given at `connect`. A
   * forwarder already open sends them with its next connection. Over stdio, where none are
   * sent, nothing changes. A header that cannot be sent throws, the headers unchanged.
   */
  setHeaders(headers: Record<string, string>): void
  /** Take over one hold of the `VirtxClient*` `raw`, made by the library this process opened. */
  static fromRaw(raw: number): Client
  /** The `VirtxClient*` this holds, still held by it. */
  asRaw(): number
  /** Give up this hold, for whoever takes the pointer to release. This client is closed after. */
  intoRaw(): number
  /** Another hold of the same session. */
  clone(): Client
  /** Let go of this hold; the last ends the session, which the promise waits for. */
  close(): Promise<void>
  [Symbol.asyncDispose](): Promise<void>

  /** Start building `recipe`, to point `reference` (`name:tag`) at it. Its log is `imageJobLog`'s. */
  buildImage(
    recipe: Recipe | ReturnType<Recipe['toJSON']>,
    reference: string,
  ): StartedJob<ImageJob, ImageJobEntry>
  /** Start pulling `reference` from its registry, to point it at what it pulls; without `auth` the server pulls anonymously. */
  pullImage(reference: string, auth?: RegistryAuth): StartedJob<ImageJob, ImageJobEntry>
  /** Start pushing the image `reference` points at to the registry `reference` names; without `auth` the server pushes anonymously. */
  pushImage(reference: string, auth?: RegistryAuth): StartedJob<ImageJob, ImageJobEntry>
  /** Start making an image of `sandbox` -- its image, with what it has written as one more layer -- to point `reference` at: how a sandbox is kept to start others from. */
  commitImage(sandbox: string, reference: string): StartedJob<ImageJob, ImageJobEntry>
  listImageJob(): Promise<ImageJob[]>
  getImageJob(id: string): Promise<ImageJob>
  /** Cancel the job if it runs, and forget it. */
  removeImageJob(id: string): Promise<void>
  /** The job's log, from its start or after the entry numbered `after`, ending after its `end` entry. */
  imageJobLog(id: string, after?: number): AsyncGenerator<ImageJobEntry>
  /** Every tag on the server's images, as `name:tag`, with the digest it points at. */
  listImage(): Promise<ImageListItem[]>
  /** Point the tag `target` (`name:tag`) at the image `source` names (`name:tag` or `name@algorithm:hex`), moving it if it is in use. */
  tagImage(source: string, target: string): Promise<void>
  /** Remove the tag `reference` (`name:tag`), leaving the image it points at. */
  untagImage(reference: string): Promise<void>
  /** Remove the image `reference` names (`name:tag` or `name@algorithm:hex`), and every tag on it in that name. */
  removeImage(reference: string): Promise<void>
  /** Write the image `reference` names to `path`, as an OCI image layout in a tar archive, replacing what is there once all of it is written. Its events are a line of log a blob, then what it wrote. */
  downloadImage(reference: string, path: string): AsyncGenerator<Event<ImageDownloadOutput, string>>
  /** Upload the image in the OCI image layout archive at `path`, and point the tag `reference` (`name:tag`) at it: the archive's only image, or the one it names by that tag. Its events are a line of log a blob, then what it uploaded. */
  uploadImage(path: string, reference: string): AsyncGenerator<Event<ImageUploadOutput, string>>
  /** Delete each image no tag points at and no sandbox stands on, and what only such images held. */
  pruneImage(): Promise<ImagePruneOutput>
  listVolume(): Promise<VolumeListOutputItem[]>
  /** The server picks a name if none is given. */
  createVolume(name?: string): Promise<VolumeListOutputItem>
  getVolume(name: string): Promise<VolumeListOutputItem>
  removeVolume(name: string): Promise<void>
  listVolumeMount(volume: string): Promise<VolumeMountListOutputItem[]>
  createVolumeMount(
    volume: string,
    path: string,
    source: MountSource,
  ): Promise<VolumeMountListOutputItem>
  removeVolumeMount(volume: string, path: string): Promise<void>
  /** A path is relative to its root, as `dir/file.txt`; the empty path is the root. */
  statFile(root: FileRoot, path: string): Promise<Metadata>
  exists(root: FileRoot, path: string): Promise<boolean>
  isFile(root: FileRoot, path: string): Promise<boolean>
  isDir(root: FileRoot, path: string): Promise<boolean>
  readDir(root: FileRoot, path: string): Promise<DirEntry[]>
  /** From `offset`, `length` bytes of the file or all that is left. */
  readFile(root: FileRoot, path: string, offset?: number, length?: number): Promise<Buffer>
  /** The whole file as text, which it has to be: UTF-8. */
  readText(root: FileRoot, path: string): Promise<string>
  /**
   * The whole file, created if it does not exist; its parent directory must exist. With `ifMatch`, the `etag`
   * a stat gave, the write goes only while the file still has it, and is refused with a `412` otherwise.
   */
  writeFile(
    root: FileRoot,
    path: string,
    content: Buffer | Uint8Array | string,
    ifMatch?: string,
  ): Promise<void>
  /** The parent must exist. */
  createDir(root: FileRoot, path: string): Promise<void>
  /** As `mkdir -p`: parents created as needed, one there already left as it is; `405` when a file is there. */
  createDirAll(root: FileRoot, path: string): Promise<void>
  /** A directory goes with everything in it. With `missingOk`, nothing being there is fine, as `rm -f`. */
  removeFile(root: FileRoot, path: string, missingOk?: boolean): Promise<void>
  /** What is at `to` is replaced if `overwrite`, and the move refused if not and something is there. */
  renameFile(root: FileRoot, from: string, to: string, overwrite?: boolean): Promise<void>
  /** A directory with everything in it; `overwrite` as in `renameFile`. */
  copyFile(root: FileRoot, from: string, to: string, overwrite?: boolean): Promise<void>
  listSandbox(): Promise<Sandbox[]>
  createSandbox(image: string | ImageName, options?: SandboxCreateOptions): Promise<Sandbox>
  getSandbox(id: string): Promise<Sandbox>
  /** The sandbox named `name`, if one is. */
  findSandbox(name: string): Promise<Sandbox | undefined>
  /** Start it if it is stopped; answered once it runs, with where its ports are now. A job started in a stopped sandbox starts it too. */
  startSandbox(id: string): Promise<Sandbox>
  /** Stop it if it runs: its jobs end, and its disk is kept -- or, for an ephemeral one, deleted with it. */
  stopSandbox(id: string): Promise<Sandbox>
  /** Open the TCP port `port` in it, as those it was made with are; where it is reached from. */
  openSandboxPort(id: string, port: number): Promise<SandboxPort>
  /** Delete the sandbox: stopped first if it runs, and its disk removed. Its volumes stay. */
  removeSandbox(id: string): Promise<void>
  listSandboxJob(sandbox: string): Promise<SandboxJob[]>
  /** Start `cmd` in the sandbox. Its output is `sandboxJobLog`'s. */
  execSandbox(
    sandbox: string,
    cmd: string[],
    options?: SandboxExecOptions,
  ): StartedJob<SandboxJob, SandboxJobEntry>
  getSandboxJob(sandbox: string, job: string): Promise<SandboxJob>
  /** Cancel the job if it runs, killing its process, and forget it. */
  removeSandboxJob(sandbox: string, job: string): Promise<void>
  /** Send `signal`, a name such as `'SIGTERM'`, to the job's process and every one it started. */
  signalSandboxJob(sandbox: string, job: string, signal: string): Promise<void>
  /** Write `data` to the job's stdin, then close it if `eof`; resolves once it is written. */
  writeSandboxJobStdin(sandbox: string, job: string, data?: string, eof?: boolean): Promise<void>
  /** The job's log, from its start or after the entry numbered `after`, ending after its `end` entry. */
  sandboxJobLog(sandbox: string, job: string, after?: number): AsyncGenerator<SandboxJobEntry>
}

/**
 * A sandbox's port at an address on this machine's loopback, until it is closed. It runs on its
 * client's session, so the client's last close stops it.
 */
export class PortForwarder {
  /** Forward the port a sandbox answered `url` for, as `SandboxPort.url`, over `client`'s session. Blocks until it is open. */
  static open(client: Client, url: string): PortForwarder
  /** Where the port is reached, as `host:port`. */
  readonly address: string
  /** The address's host: a loopback IP. */
  readonly host: string
  /** The address's port. */
  readonly port: number
  /** Stop forwarding. Closing twice does nothing. */
  close(): void
  [Symbol.dispose](): void
}
