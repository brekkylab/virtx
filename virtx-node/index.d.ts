// A virtx client over the C ABI of `libvirtx_core`. Field names are the wire's, camelCased.

/** The variable naming the library to open when `load` was not called first. */
export const LIBRARY_ENV: 'VIRTX_CORE_LIB'

/** A request failed: the server's error, the transport's, or the library's. */
export class VirtxError extends Error {
  /**
   * The HTTP status the server refused the request with, if it did: `404` for what does not
   * exist, `409` for what is in use or has ended, and the like.
   */
  status: number | null
  /** The server's response body, where it was not a JSON error. */
  body: string | null
}

/** Open the library at `file` for this process's clients; another than the one open is refused. */
export function load(file: string): void

/** The path the open library was opened by; `null` before the first client or `load`. */
export function libraryPath(): string | null

/**
 * Fetch `virtx-stdio`, the server of the library's version, into this host's cache if it is not
 * there, and give its path: what `Client.spawn()` without a program starts. The fetch runs on
 * a worker thread, off the event loop.
 *
 * The server is kept one directory per version, so a library that is upgraded fetches its
 * own. `$VIRTX_STDIO` names a server to use as it is; `$VIRTX_STDIO_VERSION` another version;
 * `$VIRTX_DIST_URL` where releases are fetched from; `$VIRTX_HOME` the cache.
 */
export function ensureVirtx(): Promise<string>

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

export type ImageJobKind = 'pull' | 'push' | 'build'

/** What the server answers about a build, pull or push it runs. */
export interface ImageJobInfo {
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

/**
 * A build, pull or push the server runs, as starting it or asking for it answered, and the
 * client that reaches it: what reads its log. Dropping it leaves the job; `remove` removes it.
 */
export class ImageJob implements ImageJobInfo {
  private constructor()
  /** The client its requests go through. */
  readonly client: Client
  id: string
  kind: ImageJobKind
  reference: string
  running: boolean
  digest?: string
  error?: string
  /** It as the server has it now. */
  get(): Promise<ImageJob>
  /** Its log, from its start or after the entry numbered `after`, ending after its `end` entry. */
  log(after?: number): AsyncGenerator<ImageJobEntry>
  /** Cancel it if it runs, and forget it. */
  remove(): Promise<void>
}

/** How an image job ended. */
export type ImageJobEnd = { digest: string } | { error: string }

/** An entry of an image job's log, numbered by `id`; `end` is the last. */
export type ImageJobEntry = { id: number } & (
  | { log: string }
  | { truncated: Record<string, never> }
  | { end: ImageJobEnd }
)

/** Credentials for a remote image registry. A `basic` password may be an access token the registry takes in its place. */
export type RegistryAuth =
  | { type: 'basic'; username: string; password: string }
  | { type: 'bearer'; token: string }

/** The credentials a server wants with every request: a token, sent as `Authorization: Bearer <token>`. */
export type Auth = { type: 'bearer'; token: string }

export interface ImageListOutputItem {
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

/** What the server answers about a snapshot. */
export interface SnapshotInfo {
  name: string
  size: number
  /** Whether a sandbox held it when it was answered, from the `createSandbox` that made a sandbox on it until that sandbox's `remove`. */
  inUse: boolean
}

/** What the server answers about a volume. */
export interface VolumeInfo {
  name: string
  /** The bytes the volume's files took on the server when it was answered. */
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
 * The files of a `Volume`, a `Snapshot` or a `Sandbox`. A path is relative to its root, as
 * `dir/file.txt`; the empty path is the root, and the server refuses one that leads outside.
 */
export class Files {
  protected constructor()
  /** The client its requests go through. */
  readonly client: Client
  statFile(path: string): Promise<Metadata>
  exists(path: string): Promise<boolean>
  isFile(path: string): Promise<boolean>
  isDir(path: string): Promise<boolean>
  readDir(path: string): Promise<DirEntry[]>
  /** From `offset`, `length` bytes of the file or all that is left. */
  readFile(path: string, offset?: number, length?: number): Promise<Buffer>
  /** The whole file as text, which it has to be: UTF-8. */
  readText(path: string): Promise<string>
  /**
   * The whole file, created if it does not exist; its parent directory must exist. With `ifMatch`, the `etag`
   * a stat gave, the write goes only while the file still has it, and is refused with a `412` otherwise.
   */
  writeFile(path: string, content: Buffer | Uint8Array | string, ifMatch?: string): Promise<void>
  /** The parent must exist. */
  createDir(path: string): Promise<void>
  /** As `mkdir -p`: parents created as needed, one there already left as it is; `405` when a file is there. */
  createDirAll(path: string): Promise<void>
  /** A directory goes with everything in it. With `missingOk`, nothing being there is fine, as `rm -f`. */
  removeFile(path: string, missingOk?: boolean): Promise<void>
  /** What is at `to` is replaced if `overwrite`, and the move refused if not and something is there. */
  renameFile(from: string, to: string, overwrite?: boolean): Promise<void>
  /** A directory with everything in it; `overwrite` as in `renameFile`. */
  copyFile(from: string, to: string, overwrite?: boolean): Promise<void>
}

/**
 * A volume the server holds, as making it or asking for it answered, and the client that reaches
 * it: its files and the mounts in it. Dropping it leaves the volume; `remove` removes it.
 */
export class Volume extends Files implements VolumeInfo {
  private constructor()
  name: string
  size: number
  /** The mounts in it, without their credentials. */
  mounts(): Promise<VolumeMountListOutputItem[]>
  /** Mount `source` at `path` in it, a directory the mount creates. */
  mount(path: string, source: MountSource): Promise<VolumeMountListOutputItem>
  /** Take away the mount at `path`, leaving its source as it is. */
  unmount(path: string): Promise<void>
  /** Delete it and everything in it. */
  remove(): Promise<void>
}

/**
 * A snapshot the server holds, as making it or asking for it answered, and the client that
 * reaches it: its files, refused with a `423` while a sandbox holds it. Dropping it leaves the
 * snapshot; `remove` removes it.
 */
export class Snapshot extends Files implements SnapshotInfo {
  private constructor()
  name: string
  size: number
  inUse: boolean
  /** Delete it and its upper layer; refused with a `409` while a sandbox holds it. */
  remove(): Promise<void>
}

/** What the server answers about a sandbox. */
export interface SandboxInfo {
  id: string
  /** The digest of the image it was made from. */
  image: string
  snapshot?: string
  /** Whether it takes processes: `false` once its VM has exited on its own. */
  running: boolean
  /** Only as `createSandbox` answers it, if it was given some. */
  ports?: SandboxPort[]
  /** virtx's own programs it has at `/abin`, if it was made with some. */
  abins?: string[]
}

/**
 * A sandbox the server holds, as making it or asking for it answered, and the client that reaches
 * it, to run processes in it and read and write its files, its root as its processes see it, as
 * `etc/hostname`. Dropping it leaves the sandbox; `remove` removes it.
 */
export class Sandbox extends Files implements SandboxInfo {
  private constructor()
  id: string
  image: string
  snapshot?: string
  running: boolean
  ports?: SandboxPort[]
  abins?: string[]
  /** Start `cmd` in it: the job, and its output as the job's `log`. */
  exec(cmd: string[], options?: SandboxExecOptions): StartedJob<SandboxJob, SandboxJobEntry>
  /** Every job in it. */
  jobs(): Promise<SandboxJob[]>
  /** The job `id` in it. */
  job(id: string): Promise<SandboxJob>
  /** Stop it, end its jobs and forget it, keeping its snapshot and volumes. */
  remove(): Promise<void>
}

/** A TCP port opened in a sandbox, and where it is reached from. */
export interface SandboxPort {
  port: number
  url: string
}

/** Whatever is left out the server picks. */
export interface SandboxCreateOptions {
  /** The snapshot it stands on, or its name. */
  snapshot?: string | Snapshot
  network?: boolean
  /** TCP ports in it to open, reserved as it is made. */
  ports?: number[]
  vcpus?: number
  memoryMib?: number
  gpu?: boolean
  gpuMemoryMib?: number
  /** Each volume, or its name, and where it is mounted. */
  volumes?: { volume: string | Volume; path: string; readonly?: boolean }[]
  /** virtx's own programs it gets at `/abin`, by name, each with its `SKILL.md` at `/abin/skills/{name}/`. */
  abins?: string[]
}

export interface SandboxExecOptions {
  /** Keep its stdin open for the job's `writeStdin`. */
  stdin?: boolean
  /** Kill it after this long. */
  timeoutMs?: number
}

/** What the server answers about a process started in a sandbox. */
export interface SandboxJobInfo {
  id: string
  kind: 'exec'
  pid: number
  cmd: string[]
  running: boolean
  /** Its exit code, once it has ended. */
  code?: number
}

/**
 * A process started in a sandbox, as starting it or asking for it answered, and the client and
 * sandbox that reach it: what signals it, writes to its stdin and reads its log. Dropping it
 * leaves the job; `remove` removes it.
 */
export class SandboxJob implements SandboxJobInfo {
  private constructor()
  /** The client its requests go through. */
  readonly client: Client
  /** The id of the sandbox it runs in. */
  readonly sandboxId: string
  id: string
  kind: 'exec'
  pid: number
  cmd: string[]
  running: boolean
  code?: number
  /** It as the server has it now. */
  get(): Promise<SandboxJob>
  /** Send `signal`, a name such as `'SIGTERM'`, to its process and every one it started. */
  signal(signal: string): Promise<void>
  /** Write `data` to its stdin, then close it if `eof`; resolves once it is written. */
  writeStdin(data?: string, eof?: boolean): Promise<void>
  /** Its log, from its start or after the entry numbered `after`, ending after its `end` entry. */
  log(after?: number): AsyncGenerator<SandboxJobEntry>
  /** Cancel it if it runs, killing its process, and forget it. */
  remove(): Promise<void>
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
 * leaving one leaves the job running, which the job's `remove` cancels.
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
  /**
   * Start `program` with `args` as the server, and talk to it over its stdin and stdout.
   * Without `program`, the server is `virtx-stdio` of this library's version, fetched first if
   * this host has none, blocking for the fetch; `ensureVirtx` does that fetch alone, off the
   * event loop.
   */
  static spawn(program?: string | null, args?: string[]): Client
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

  /** Start building `recipe`, to point `reference` (`name:tag`) at it. Its log is the job's `log`. */
  buildImage(recipe: Recipe | ReturnType<Recipe['toJSON']>, reference: string): StartedJob<ImageJob, ImageJobEntry>
  /** Start pulling `reference` from its registry, to point it at what it pulls; without `auth` the server pulls anonymously. */
  pullImage(reference: string, auth?: RegistryAuth): StartedJob<ImageJob, ImageJobEntry>
  /** Start pushing the image `reference` points at to the registry `reference` names; without `auth` the server pushes anonymously. */
  pushImage(reference: string, auth?: RegistryAuth): StartedJob<ImageJob, ImageJobEntry>
  /** Every image job the server holds. */
  listImageJob(): Promise<ImageJob[]>
  /** The image job `id`. */
  getImageJob(id: string): Promise<ImageJob>
  /** Every tag on the server's images, as `name:tag`, with the digest it points at. */
  listImage(): Promise<ImageListOutputItem[]>
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
  /** Delete each image no tag points at and no sandbox uses, and what only such images held. */
  pruneImage(): Promise<ImagePruneOutput>
  listSnapshot(): Promise<Snapshot[]>
  /** The server picks a name or size not given. */
  createSnapshot(name?: string, diskGib?: number): Promise<Snapshot>
  getSnapshot(name: string): Promise<Snapshot>
  listVolume(): Promise<Volume[]>
  /** The server picks a name if none is given. */
  createVolume(name?: string): Promise<Volume>
  getVolume(name: string): Promise<Volume>
  listSandbox(): Promise<Sandbox[]>
  createSandbox(image: string | ImageName, options?: SandboxCreateOptions): Promise<Sandbox>
  getSandbox(id: string): Promise<Sandbox>
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
