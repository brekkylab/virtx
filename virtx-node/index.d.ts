/// <reference types="node" />
// A virtx client over the C ABI of `libvirtx_core`. Field names are the wire's, camelCased.

/** The variable naming the library to open when `load` was not called first. */
export const LIBRARY_ENV: 'VIRTX_CORE_LIB'

/** A request failed: the server's error, the transport's, or the library's. */
export class VirtxError extends Error {
  /**
   * Where it failed: the server answered with an error, the job it started failed, the
   * connection failed, or this side did. Tell errors apart by it and `status`, not the message.
   */
  source: 'server' | 'job' | 'transport' | 'client'
  /**
   * The HTTP status the server refused the request with, if it did: `404` for what does not
   * exist, `409` for what is in use or has ended, and the like.
   */
  status: number | null
  /**
   * The protocol's code the server refused the request with, where it answered one:
   * `not_found`, `in_use` and the like -- what to branch on, not the message.
   */
  code: string | null
  /** The server's response body, where it was not a JSON error. */
  body: string | null
  /**
   * How many bytes of a write to a job's stdin went in before its process closed the stdin under
   * it: that `409`, `ended`, alone has it.
   */
  written: number | null
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
  /**
   * Its log, from its start or after the entry numbered `after`, ending after its `end` entry,
   * or throwing the `VirtxError` it failed with in that entry's place.
   */
  log(after?: number): AsyncGenerator<ImageJobEntry>
  /** Follow it to its end, each line of its log printed if `print`: the digest it ended with. Rejects with why it failed. */
  complete(options?: CompleteOptions): Promise<string>
  /** Cancel it if it runs, and forget it. */
  remove(): Promise<void>
}

/** How an image job that succeeded ended. One that failed throws its error in this entry's place. */
export type ImageJobEnd = { digest: string }

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
export type Auth =
  | { type: 'bearer'; token: string }
  | { type: 'command'; program: string; args?: string[] }

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

/** A user's role in a project: an owner manages its members and removes it; a member uses what is in it. */
export type Role = 'owner' | 'member'

/** What the server answers about a project. */
export interface ProjectInfo {
  /** What names it on any server, given it once. */
  id: string
  /** What its owner names it, among their projects. */
  name: string
  /** The user whose project it is: absent for one of no user's, such as one made over stdio. */
  owner?: string
  /** The client's role in it. */
  role: Role
}

/** What the server answers about a member of a project. */
export interface ProjectMemberInfo {
  /** The user, as the server names them, such as an email address. */
  user: string
  role: Role
}

/**
 * A project the client has a role in, as making it or asking for it answered, and the client
 * that reaches it: its members, and its removal. What is in it, a client reaches with the
 * project set (`setProject`). Dropping it leaves the project; `remove` removes it.
 */
export class Project implements ProjectInfo {
  private constructor()
  /** The client its requests go through. */
  readonly client: Client
  id: string
  name: string
  owner?: string
  role: Role
  /** Its members, with their roles. */
  members(): Promise<ProjectMemberInfo[]>
  /** Give `user` `role` in it, adding them if they are no member. */
  setMember(user: string, role: Role): Promise<ProjectMemberInfo>
  /** Take `user`'s role in it away: theirs to do, leaving it, or an owner's. */
  removeMember(user: string): Promise<void>
  /** Delete it and everything in it. */
  remove(): Promise<void>
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
 * The files of a `Volume` or a `Sandbox`. A path is relative to its root, as
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

/** What the server answers about a sandbox. */
export interface SandboxInfo {
  id: string
  /** The name it was made with, which `findSandbox` finds it by. */
  name?: string
  /** The digest of the image it was made from. */
  image: string
  /** Whether it takes processes: `false` once its VM has exited on its own. */
  running: boolean
  /** Its opened ports, if it was given some: each with its `url` only as `createSandbox` answers it. */
  ports?: SandboxPort[]
  /** virtx's own programs it has at `/abin`, if it was made with some. */
  abins?: string[]
  /** Why it takes nothing but `remove`, for one its server could not know again after a restart. */
  error?: string
}

/**
 * A sandbox the server holds, as making it or asking for it answered, and the client that reaches
 * it, to run processes in it and read and write its files, its root as its processes see it, as
 * `etc/hostname`. Dropping it leaves the sandbox; `remove` removes it.
 */
export class Sandbox extends Files implements SandboxInfo {
  private constructor()
  id: string
  name?: string
  image: string
  running: boolean
  ports?: SandboxPort[]
  abins?: string[]
  error?: string
  /** Start `cmd` in it: the job, and its output as the job's `log`. */
  exec(cmd: string[], options?: SandboxExecOptions): StartedJob<SandboxJob, SandboxJobEntry>
  /** Every job in it. */
  jobs(): Promise<SandboxJob[]>
  /** The job `id` in it. */
  job(id: string): Promise<SandboxJob>
  /**
   * Resize it: its vCPUs and memory from its VM's next boot on, and its disk grown, never shrunk,
   * as its VM next boots. A VM up and unused is stopped for its vCPUs or memory to change, and
   * one in use is refused `409`; its disk alone stops nothing. What is left out stays as it is.
   */
  resize(options?: SandboxResizeOptions): Promise<SandboxResources>
  /** Stop it, end its jobs and forget it, with everything it wrote outside its volumes. */
  remove(): Promise<void>
}

/** What `Sandbox.resize` changes; whatever is left out stays as it is. */
export interface SandboxResizeOptions {
  vcpus?: number
  memoryMib?: number
  /** How much it may write over its image, in GiB, at most 1024: grown, never shrunk. */
  diskGib?: number
}

/** A sandbox's size as `Sandbox.resize` answers it: what its VM boots with from its next boot on. */
export interface SandboxResources {
  vcpus: number
  memoryMib: number
  diskGib: number
  /** Whether its VM is up now, the size it booted with until it boots again. */
  up: boolean
}

/** How a sandbox's port is reached: `'tcp'` through a `PortForwarder`, `'http'` at its `url` by a browser as by any client. */
export type PortProtocol = 'tcp' | 'http'

/** A port opened in a sandbox, and where it is reached from. */
export interface SandboxPort {
  port: number
  /** Answered by `createSandbox` alone, as secret as the credentials it carries: absent from a sandbox `getSandbox`, `findSandbox` or `listSandbox` answers, which a `PortForwarder` refuses. */
  url?: string
  /** What it is for, as it was opened with. */
  label?: string
  /** Absent for `'tcp'`. Any other string is a protocol this version does not know, as the server said it. */
  protocol?: PortProtocol | (string & {})
  /** A `host:port` any TCP client reaches a TCP port at directly, from a server that gives it one. Not secret, as `url` is: what listens there answers whoever connects. */
  address?: string
  /** Why its inbound rules were not set, from a server that made the port all the same: it lets no one in, and is reached at no `address`, until they are. */
  inboundError?: InboundError
}

/** Why a port's inbound rules were not set, in the shape of a refusal. */
export interface InboundError {
  /** As a refusal's, such as `'quota'` or `'unavailable'`. */
  code: string
  /** For a person to read. */
  message: string
  /** What a `'quota'` is past, as a refusal's `reason`. */
  reason?: string
}

/**
 * A port to open as a sandbox is made: a number for TCP that says nothing more, or one that says
 * what it is for, up to 128 characters, how it is reached, and whom it lets in. `inbound` left out
 * leaves that to the server, and `[]` lets in no one; a server that keeps no rules refuses them.
 */
export type PortSpec =
  | number
  | { port: number; label?: string; protocol?: PortProtocol; inbound?: InboundRule[] }

/** One of a port's inbound rules: the addresses it lets in, and whose they are. */
export interface InboundRule {
  /** An IPv4 or IPv6 CIDR, as `203.0.113.0/24`, or one address, as `203.0.113.7`. */
  source: string
  /** What it is for: whose they are. */
  description?: string
}

/** Whatever is left out the server picks. */
export interface SandboxCreateOptions {
  /** A name to find it again by, with `findSandbox`, rather than keeping its id: letters, digits, `.`, `_` and `-`, not starting with `.` or `-`, at most 128, or it is refused with `400`; one another sandbox has is refused with `409`. */
  name?: string
  network?: boolean
  /** Ports in it to open, reserved as it is made: `[8080, { port: 5173, label: 'dev', protocol: 'http' }]`. */
  ports?: PortSpec[]
  vcpus?: number
  memoryMib?: number
  /** How much it may write over its image, in GiB, at most 1024, which goes with it. */
  diskGib?: number
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

/**
 * Why a process did not end by itself: the server, or the kernel, killed it, as `SIGKILL` does,
 * its code `137`. `timeout`, its `timeoutMs` ran out; `cancelled`, its job was removed while it
 * ran, its sandbox still there; `sandbox_deleted`, its sandbox was removed under it; `vm_exited`,
 * its sandbox's VM exited on its own under it; `oom`, its memory ran out; `restarted`, its server
 * restarted while it ran, keeping its sandbox. Any other string is a reason this version does not
 * know, as the server said it.
 */
export type SandboxJobEndReason =
  | 'timeout'
  | 'cancelled'
  | 'sandbox_deleted'
  | 'vm_exited'
  | 'oom'
  | 'restarted'
  | (string & {})

/** What the server answers about a process started in a sandbox. */
export interface SandboxJobInfo {
  id: string
  kind: 'exec'
  pid: number
  cmd: string[]
  running: boolean
  /** Its exit code, once it has ended. */
  code?: number
  /** Why it ended, once it has, if it did not end by itself. */
  reason?: SandboxJobEndReason
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
  reason?: SandboxJobEndReason
  /** It as the server has it now. */
  get(): Promise<SandboxJob>
  /** Send `signal`, a name such as `'SIGTERM'`, to its process and every one it started. */
  signal(signal: string): Promise<void>
  /** Write `data` to its stdin, then close it if `eof`; resolves once it is written. */
  writeStdin(data?: string, eof?: boolean): Promise<void>
  /** Its log, from its start or after the entry numbered `after`, ending after its `end` entry. */
  log(after?: number): AsyncGenerator<SandboxJobEntry>
  /**
   * Follow it to its end, what its process writes to stdout and stderr printed there if
   * `print`: its exit code, `128 + signal` for one a signal ended.
   */
  complete(options?: CompleteOptions): Promise<number>
  /** Cancel it if it runs, killing its process, and forget it. */
  remove(): Promise<void>
}

/** An entry of a sandbox job's log, numbered by `id`; `end` is the last. */
export type SandboxJobEntry = { id: number } & (
  | { stdout: string }
  | { stderr: string }
  | { truncated: Record<string, never> }
  | { end: SandboxJobEnd }
)

/** How a process ended, last in its log: its exit code, and why, if it did not end by itself. */
export interface SandboxJobEnd {
  code: number
  reason?: SandboxJobEndReason
}

/** How a job's `complete` follows it. */
export interface CompleteOptions {
  /** Print its log as it is read, to stdout, and a process's stderr to stderr. */
  print?: boolean
}

/**
 * A job as it starts: a promise of the job `J`, an async iterable of its log's entries `E`,
 * read until its `end` entry, and `complete`, the job's own once started. The job starts at the
 * call, as a promise's work does, and once however the value is used: awaited, iterated,
 * completed, or each. Each loop reads the log from its start; leaving one leaves the job
 * running, which the job's `remove` cancels. A sandbox's `exec` iterated before the code awaits
 * anything else -- `for await (const entry of sandbox.exec(cmd))` -- starts the job and reads its
 * log as one request, where the server answers so; once anything is awaited between the call and
 * the loop, the job has started on its own and its log is a second request.
 */
export class StartedJob<J, E> extends Promise<J> implements AsyncIterable<E> {
  /** Start the job and follow it to its end, as its `complete` does. */
  complete<T>(
    this: StartedJob<{ complete(options?: CompleteOptions): Promise<T> }, E>,
    options?: CompleteOptions,
  ): Promise<T>
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
   * this host has none, blocking for the fetch; `ensureVirtx` does that fetch alone, on the
   * event loop.
   */
  static spawn(program?: string | null, args?: string[]): Client
  /**
   * Reach the server at `url`, its base `http` or `https` URL, sending `auth`, the credentials
   * it wants, `project`, the project every request is in (the user's own without one),
   * and `headers`, whatever else it asks of each request, with every request. `Authorization`
   * is `auth`'s, and `virtx-project` names `project`, over one given as a header. Refused when a
   * token would go in the clear: over `http` to any host but this machine.
   */
  static connect(
    url: string,
    auth?: Auth | null,
    project?: string | null,
    headers?: Record<string, string>,
  ): Client
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
  /**
   * Name `project` in every request from now on, over either transport, in place of a
   * `virtx-project` among the headers given. With null, that header names it where there is one,
   * and otherwise none is named, for the user's own project. A name no header carries throws,
   * the project unchanged.
   */
  setProject(project: string | null): void
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
  buildImage(
    recipe: Recipe | ReturnType<Recipe['toJSON']>,
    reference: string,
  ): StartedJob<ImageJob, ImageJobEntry>
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
  /** Remove the tag `reference` (`name:tag`), leaving the image it points at. The last tag of an image a sandbox or a build uses is refused `409`, and kept. */
  untagImage(reference: string): Promise<void>
  /** Remove the image `reference` names (`name:tag` or `name@algorithm:hex`), and every tag on it in that name. An image a sandbox or a build uses is refused `409`, with nothing removed. */
  removeImage(reference: string): Promise<void>
  /** Write the image `reference` names to `path`, as an OCI image layout in a tar archive, replacing what is there once all of it is written. Its events are a line of log a blob, then what it wrote. */
  downloadImage(reference: string, path: string): AsyncGenerator<Event<ImageDownloadOutput, string>>
  /** Upload the image in the OCI image layout archive at `path`, and point the tag `reference` (`name:tag`) at it: the archive's only image, or the one it names by that tag. Its events are a line of log a blob, then what it uploaded. */
  uploadImage(path: string, reference: string): AsyncGenerator<Event<ImageUploadOutput, string>>
  /** Delete each image no tag points at and no sandbox uses, and what only such images held. */
  pruneImage(): Promise<ImagePruneOutput>
  /** Every project the client has a role in. */
  listProject(): Promise<Project[]>
  /** Make the project `name`, the client its owner. */
  createProject(name: string): Promise<Project>
  /** The project `name` names -- a project of the user's by name, or any by its id -- with the client's role in it. */
  getProject(name: string): Promise<Project>
  listVolume(): Promise<Volume[]>
  /** The server picks a name if none is given. */
  createVolume(name?: string): Promise<Volume>
  getVolume(name: string): Promise<Volume>
  /** Every sandbox the server holds, each port without its `url`. */
  listSandbox(): Promise<Sandbox[]>
  /** `image` has to be on the server already, pulled or built: this does not pull it, and a tag the server has no image by is refused with `404`. */
  createSandbox(image: string | ImageName, options?: SandboxCreateOptions): Promise<Sandbox>
  /** The sandbox `id`, each port without its `url`. */
  getSandbox(id: string): Promise<Sandbox>
  /** The sandbox named `name`, each port without its `url`, or `undefined` if none is. */
  findSandbox(name: string): Promise<Sandbox | undefined>
}

/**
 * A sandbox's port at an address on this machine's loopback, until it is closed. It runs on its
 * client's session, so the client's last close stops it. A connection whose WebSocket is
 * refused, or closed before the port sent a byte, is closed, and what was seen is `lastError`,
 * and emitted as a process warning of the type `VirtxWarning`.
 */
export class PortForwarder {
  /** Forward `port`, as `createSandbox` answers one in `ports`, or the `url` it answered for it, over `client`'s session. Given the port, what a connection that fails says names its number. Returns once its local address is open, which takes no request of the server. A port without its `url`, as `getSandbox` answers one, is refused with a `VirtxError` naming it. */
  static open(client: Client, port: SandboxPort | string): PortForwarder
  /** Where the port is reached, as `host:port`. */
  readonly address: string
  /** Why the last connection to the forwarder that failed did, its message naming the port, with the server's reason for closing it if it gave one; `null` while none has, and again once a later connection's port sends a byte. It is the last failure, not whether the port is reached now. Its `status` is the server's, where it refused the connection's WebSocket. A server on this machine is reached at its own address, not through the forwarder, so it has none. */
  readonly lastError: VirtxError | null
  /** The address's host: a loopback IP. */
  readonly host: string
  /** The address's port. */
  readonly port: number
  /** Stop forwarding. Closing twice does nothing. */
  close(): void
  [Symbol.dispose](): void
}
