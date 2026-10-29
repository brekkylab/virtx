// A virtx client over the C ABI of `libvirtx_core`. Field names are the wire's, camelCased.

/** The variable naming the library to open when `load` was not called first. */
export const LIBRARY_ENV: 'VIRTX_CORE_LIB'

/** A request failed: the server's error, the transport's, or the library's. */
export class VirtxError extends Error {}

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

export interface ImageBuildOutput {
  /** The digest of the image's manifest, as `algorithm:hex`. */
  digest: string
}

/** Credentials for a remote image registry. A `basic` password may be an access token the registry takes in its place. */
export type RegistryAuth =
  | { type: 'basic'; username: string; password: string }
  | { type: 'bearer'; token: string }

/** The credentials a server wants with every request: a token, sent as `Authorization: Bearer <token>`. */
export type Auth = { type: 'bearer'; token: string }

export interface ImagePullOutput {
  /** The digest of the image's manifest, as `algorithm:hex`. */
  digest: string
}

export interface ImagePushOutput {
  /** The digest of the manifest pushed, as `algorithm:hex`. */
  digest: string
}

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

export interface SnapshotListItem {
  name: string
  size: number
  /** Whether a sandbox holds it now, from its `initSandbox` until its `closeSandbox`. */
  inUse: boolean
}

export interface VolumeListOutputItem {
  name: string
  /** The bytes the volume's files take on the server. */
  size: number
}

export interface VolumeCreateOutput {
  /** The one asked for, or the one the server picked. */
  name: string
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

/** Whose files a request names: a volume's, or a snapshot's. A snapshot in use, held by a sandbox, refuses every request. */
export type FileRoot = { volume: string } | { snapshot: string }

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

export interface SandboxListItem {
  id: string
  /** The digest of the image it was made from. */
  image: string
  snapshot?: string
  /** Whether its VM is up now. */
  running: boolean
  ports?: SandboxPort[]
}

/** A TCP port opened in a sandbox, and where it is reached from. */
export interface SandboxPort {
  port: number
  url: string
}

/** Whatever is left out the server picks. */
export interface SandboxInitOptions {
  snapshot?: string
  network?: boolean
  /** TCP ports in it to open, reserved as it is made. */
  ports?: number[]
  vcpus?: number
  memoryMib?: number
  gpu?: boolean
  gpuMemoryMib?: number
  volumes?: { volume: string; path: string; readonly?: boolean }[]
}

export interface SandboxInitOutput {
  id: string
  ports?: SandboxPort[]
}

/**
 * A client of one server, over stdio or Streamable HTTP, made by `libvirtx_core`. Holds of one
 * session are counted: `clone` takes another, `close` lets this one go, and the session ends with
 * the last.
 */
export class Client {
  /** Start `program` with `args` as the server, and talk to it over its stdin and stdout. */
  static spawn(program: string, args?: string[]): Client
  /**
   * Reach the server at `url`, its base `http` or `https` URL (`/rpc` is under it), sending
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

  buildImage(recipe: Recipe | ReturnType<Recipe['toJSON']>, reference: string): AsyncGenerator<Event<ImageBuildOutput, string>>
  /** Pull `reference` from its registry, and point it at what it pulls; without `auth` the server pulls anonymously. */
  pullImage(reference: string, auth?: RegistryAuth): AsyncGenerator<Event<ImagePullOutput, string>>
  /** Push the image `reference` points at to the registry `reference` names; without `auth` the server pushes anonymously. */
  pushImage(reference: string, auth?: RegistryAuth): AsyncGenerator<Event<ImagePushOutput, string>>
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
  /** Delete each image no tag points at and no sandbox uses, and what only such images held. */
  pruneImage(): Promise<ImagePruneOutput>
  listSnapshot(): Promise<SnapshotListItem[]>
  createSnapshot(name?: string, diskGib?: number): Promise<{ name: string }>
  removeSnapshot(name: string): Promise<void>
  listVolume(): Promise<VolumeListOutputItem[]>
  createVolume(name?: string): Promise<VolumeCreateOutput>
  removeVolume(name: string): Promise<void>
  listVolumeMount(volume: string): Promise<VolumeMountListOutputItem[]>
  createVolumeMount(volume: string, path: string, source: MountSource): Promise<void>
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
  writeFile(root: FileRoot, path: string, content: Buffer | Uint8Array | string, ifMatch?: string): Promise<void>
  writeText(root: FileRoot, path: string, text: string, ifMatch?: string): Promise<void>
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
  listSandbox(): Promise<SandboxListItem[]>
  initSandbox(image: string | ImageName, options?: SandboxInitOptions): Promise<SandboxInitOutput>
  /** Killed after `timeoutMs` if one is given. */
  execSandbox(sandbox: string, cmd: string[], timeoutMs?: number): AsyncGenerator<Event<{ code: number }, string>>
  closeSandbox(sandbox: string): Promise<void>
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
