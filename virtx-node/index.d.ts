// A virtx client over the C ABI of `libvirtx_core`. Field names are the wire's, camelCased.

/** The variable naming the library to open when `load` was not called first. */
export const LIBRARY_ENV: 'VIRTX_CORE_LIB'

/** A request failed: the server's error, the transport's, or the library's. */
export class VirtxError extends Error {}

/** Open the library at `file` for this process's clients; another than the one open is refused. */
export function load(file: string): void

/** The path the open library was opened by; `null` before the first client or `load`. */
export function libraryPath(): string | null

/** An image, as the wire names one. */
export type ImageName = { type: 'reference'; reference: string } | { type: 'digest'; digest: string }

/** A full `sha256:` or `sha512:` digest is a digest, any other string a reference. */
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

/** A line of a request's log, then what it answered. */
export type Event<T> = { progress: string } | { output: T }

export interface ImageListItem {
  reference: string
  digest: string
  size: number
}

export interface ImagePullOutput {
  reference: string
  digest: string
}

export interface ImageBuildOutput {
  reference: string
  digest: string
}

export interface SnapshotListItem {
  name: string
  size: number
}

export interface VolumeListItem {
  name: string
  size: number
}

export interface Metadata {
  type: 'file' | 'dir'
  size: number
  /** Nanoseconds since the Unix epoch. */
  modified: number
}

export interface DirEntry {
  name: string
  metadata: Metadata
}

export type MountType = 'notion' | 'gdrive' | 'onedrive'

export type MountSource =
  | { type: 'notion'; apiKey: string }
  | { type: 'gdrive'; clientId: string; clientSecret: string; refreshToken: string }
  | { type: 'onedrive'; clientId: string; clientSecret?: string; refreshToken: string }

export interface VolumeMountListItem {
  path: string
  type: MountType
}

export interface SandboxListItem {
  id: string
  /** The digest of the image it was made from. */
  image: string
  snapshot?: string
}

/** Whatever is left out the server picks. */
export interface SandboxInitOptions {
  snapshot?: string
  network?: boolean
  ports?: { host: number; guest: number }[]
  vcpus?: number
  memoryMib?: number
  gpu?: boolean
  gpuMemoryMib?: number
  volumes?: { volume: string; path: string; readonly?: boolean }[]
}

export interface SandboxInitOutput {
  id: string
  cwd?: string
}

/**
 * A client of one server, over stdio or Streamable HTTP, made by `libvirtx_core`. Holds of one
 * session are counted: `clone` takes another, `close` lets this one go, and the session ends with
 * the last.
 */
export class Client {
  /** Start `program` with `args` as the server, and talk to it over its stdin and stdout. */
  static spawn(program: string, args?: string[]): Client
  /** Reach the server at `url`, an `http` or `https` URL. */
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

  listImage(): Promise<ImageListItem[]>
  pullImage(reference: string): AsyncGenerator<Event<ImagePullOutput>>
  buildImage(recipe: Recipe | ReturnType<Recipe['toJSON']>, reference?: string): AsyncGenerator<Event<ImageBuildOutput>>
  removeImage(image: string | ImageName): Promise<void>
  listSnapshot(): Promise<SnapshotListItem[]>
  createSnapshot(name?: string, diskGib?: number): Promise<{ name: string }>
  removeSnapshot(name: string): Promise<void>
  listVolume(): Promise<VolumeListItem[]>
  createVolume(name?: string): Promise<{ name: string }>
  removeVolume(name: string): Promise<void>
  statVolumeFile(volume: string, path: string): Promise<Metadata>
  readVolumeDir(volume: string, path: string): Promise<DirEntry[]>
  /** Up to `length` bytes from `offset`, or to the end without a `length`. */
  readVolumeFile(volume: string, path: string, offset?: number, length?: number): Promise<Uint8Array>
  /** At `offset`, or the whole file without one. */
  writeVolumeFile(volume: string, path: string, data: Uint8Array, offset?: number): Promise<void>
  createVolumeDir(volume: string, path: string, parents?: boolean): Promise<void>
  removeVolumeFile(volume: string, path: string, recursive?: boolean): Promise<void>
  renameVolumeFile(volume: string, from: string, to: string, overwrite?: boolean): Promise<void>
  copyVolumeFile(volume: string, from: string, to: string, overwrite?: boolean): Promise<void>
  listVolumeMount(volume: string): Promise<VolumeMountListItem[]>
  createVolumeMount(volume: string, path: string, source: MountSource): Promise<void>
  removeVolumeMount(volume: string, path: string): Promise<void>
  listSandbox(): Promise<SandboxListItem[]>
  initSandbox(image: string | ImageName, options?: SandboxInitOptions): Promise<SandboxInitOutput>
  /** Killed after `timeoutMs` if one is given. */
  execSandbox(sandbox: string, cmd: string[], timeoutMs?: number): AsyncGenerator<Event<{ code: number }>>
  closeSandbox(sandbox: string): Promise<void>
}
