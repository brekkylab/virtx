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

export interface SandboxListItem {
  id: string
  /** The digest of the image it was made from. */
  image: string
  snapshot?: string
  /** Whether its VM is up now. */
  running: boolean
  daemon?: boolean
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
  /** Keep it until `closeSandbox`, apart from this client. */
  daemon?: boolean
  /** TCP ports in it to open; only for a daemon. */
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
  /** Reach the server at `url`, its base `http` or `https` URL (`/rpc` is under it). */
  static connect(url: string): Client
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
  listSnapshot(): Promise<SnapshotListItem[]>
  createSnapshot(name?: string, diskGib?: number): Promise<{ name: string }>
  removeSnapshot(name: string): Promise<void>
  listVolume(): Promise<VolumeListOutputItem[]>
  createVolume(name?: string): Promise<VolumeCreateOutput>
  removeVolume(name: string): Promise<void>
  listVolumeMount(volume: string): Promise<VolumeMountListOutputItem[]>
  createVolumeMount(volume: string, path: string, source: MountSource): Promise<void>
  removeVolumeMount(volume: string, path: string): Promise<void>
  listSandbox(): Promise<SandboxListItem[]>
  initSandbox(image: string | ImageName, options?: SandboxInitOptions): Promise<SandboxInitOutput>
  /** Killed after `timeoutMs` if one is given. */
  execSandbox(sandbox: string, cmd: string[], timeoutMs?: number): AsyncGenerator<Event<{ code: number }, string>>
  closeSandbox(sandbox: string): Promise<void>
}
