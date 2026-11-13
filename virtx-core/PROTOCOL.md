# VirtX Protocol

VirtX protocol is for running virtual machines, either locally or remotely.

A server offers several endpoints, and each speaks its own protocol:

| Endpoint | Protocol | For |
| --- | --- | --- |
| `/images/oci/v2/` | [OCI Distribution](#images) | Images: manifests, blobs and tags |
| `/images/jobs/` | [HTTP](#image-jobs) | Image jobs: read, cancel |
| `/images/jobs/pull/` | [HTTP](#image-jobs) | Starting an image pull from another registry |
| `/images/jobs/push/` | [HTTP](#image-jobs) | Starting an image push to another registry |
| `/images/jobs/build/` | [HTTP](#image-jobs) | Starting an image build |
| `/images/jobs/commit/` | [HTTP](#image-jobs) | Starting an image made of a sandbox |
| `/images/jobs/{id}/events` | [Event stream](#events) | An image job's log and end |
| `/images/prune` | [HTTP](#image-jobs) | Deleting the images nothing uses |
| `/volumes/` | [HTTP](#volumes) | Volume CRUD |
| `/volumes/{name}/mounts/` | [HTTP](#mounts) | Volume mount CRUD |
| `/volumes/{name}/files/` | [WebDAV](#files) | The files in a volume |
| `/sandboxes/` | [HTTP](#sandboxes) | Sandbox CRUD |
| `/sandboxes/{id}/files/` | [WebDAV](#files) | The files in a sandbox, as its processes see them |
| `/sandboxes/{id}/start`, `/sandboxes/{id}/stop` | [HTTP](#sandboxes) | Starting and stopping a sandbox |
| `/sandboxes/{id}/ports/{port}` | [HTTP](#ports) | Opening a port in a sandbox |
| `/sandboxes/{id}/jobs/` | [HTTP](#sandbox-jobs) | Sandbox jobs: read, signal, cancel |
| `/sandboxes/{id}/jobs/exec/` | [HTTP](#sandbox-jobs) | Starting a process in a sandbox |
| `/sandboxes/{id}/jobs/{job}/stdin` | [HTTP](#sandbox-jobs) | Writing to a process's stdin |
| `/sandboxes/{id}/jobs/{job}/signal` | [HTTP](#sandbox-jobs) | Sending a signal to a process |
| `/sandboxes/{id}/jobs/{job}/events` | [Event stream](#events) | A process's output and end |

Everything is made, read and removed with plain HTTP, each request standing alone.
What runs for a while, such as a build or a process, is a [job](#jobs): started with a `POST`, read with a `GET`, cancelled with a `DELETE`, and its log read as an event stream by any client allowed to, from where it left off.
What a thing holds is moved with an established protocol, so none of these carries bulk data.

Both [transports](#transport) carry HTTP, and every endpoint is reached the same way over either.

## Transport

### TCP

The server listens on TCP and is reached over HTTP at a base URL, such as `https://host/`.
Endpoints are paths under it.
The server SHOULD offer HTTP/2, and MAY also offer HTTP/1.1 and HTTP/3.
Over HTTP/1.1 every event stream read at once holds a connection of its own, so a client reading many SHOULD reach the server over HTTP/2.

The server keeps nothing between requests for the transport's sake.

A server on the local machine SHOULD bind to `127.0.0.1`, not to every interface.

#### Origin

A web page could reach a server through DNS rebinding, so the server MUST check the `Origin` header of every request, the opening of a [port's](#ports) WebSocket included.
It MUST answer `403 Forbidden` to a request whose `Origin` names an origin it does not allow.
A request with no `Origin`, from any non-browser client, is not refused for that.

### Stdio

The client starts the server as a child process, and the server's stdin and stdout together carry one HTTP/2 connection (RFC 9113), with the client as its client.

- The connection starts with prior knowledge (RFC 9113, section 3.3): the client sends the connection preface first, with no TLS and no upgrade. Requests use the `http` scheme. The authority is not checked.
- Neither side writes anything else there: the client MUST NOT to stdin, and the server MUST NOT to stdout.
- The server MAY write UTF-8 text to stderr for a person to read. The client MAY capture, forward or ignore it.
- The connection has one client. The server takes no [credentials](#authentication) and checks no [`Origin`](#origin).
- The client ends it by closing the server's stdin, which the OS also does when the client's process ends. The server SHOULD then stop every sandbox, as [`stop`](#sandboxes) does -- deleting those that are ephemeral -- cancel every job, and exit, without waiting for requests in flight to finish. A client MAY kill a server that has not exited within a time it chooses.

## Authentication

Over [TCP](#tcp), a server reachable from other machines SHOULD require credentials on every request, to every endpoint.
A server on the local machine MAY take none, and a server over [stdio](#stdio) takes none.
The credentials SHOULD be an OAuth 2.1 access token, sent as `Authorization: Bearer <token>`.
How the client obtains the token is outside this protocol.

A server answers `401 Unauthorized` to a request without valid credentials, and `403 Forbidden` to one whose credentials do not allow it.

A server that keeps organizations apart, each with its own images, volumes and sandboxes, takes the organization a request is for in the `virtx-org` header.
A credential that is an organization's own, such as an API key, names its organization already, and the header MAY be left out.
A server answers `400 Bad Request` to a request that names no organization when it needs one, and `403 Forbidden` to one naming an organization the credentials do not reach.

## Resources

Everything but images and files is made, read and removed with the requests below.
A request body is JSON, sent with `Content-Type: application/json`, and so is every response body but an [event stream](#events).
`{name}`, `{id}` and `{job}` are each one path segment, percent-encoded.

A `POST` or `PUT` that makes something answers `201 Created` with it, and with its path in `Location`.
A `DELETE` answers `204 No Content` once what it names is gone.

A request that fails is answered with an HTTP error status and a body:

```text
{"error": {"code": "in_use", "message": "volume `data` is mounted by a sandbox"}}
```

- `code` is a string, from the table below, or another the server gives for its own failures.
- `message` is a string, for a person to read. A client SHOULD NOT tell errors apart by it.

| Status | `code` | When |
| --- | --- | --- |
| `400 Bad Request` | `invalid` | The body is not JSON, or not what the request takes, such as a missing or unknown member. |
| `401 Unauthorized`, `403 Forbidden` | `unauthorized`, `forbidden` | See [Authentication](#authentication) and [Origin](#origin). |
| `404 Not Found` | `not_found` | What the path names does not exist. |
| `405 Method Not Allowed` | `method_not_allowed` | The path takes no such method. |
| `409 Conflict` | `exists`, `in_use`, `ended`, `stopped` | A name already in use, something held by a sandbox, a job or a sandbox's VM that has ended, or what is asked of a sandbox while it runs and it is stopped. |
| `413 Content Too Large` | `too_large` | The body is over the server's limit. |
| `422 Unprocessable Content` | `not_started` | A job that cannot be started, such as a command not found. |
| `500 Internal Server Error` | `internal` | The server failed on its side. |

### Jobs

A job is something the server does for a while: an [image job](#image-jobs), such as a build, or a [sandbox job](#sandbox-jobs), a process.
Each is a resource of its own, under an id the server gives it, and every job is read as the others are:

- As JSON it is an object with at least `id`, `kind`, such as `"build"` or `"exec"`, and `running`, whether it runs still. What it ended with is beside them once it has ended.
- A `DELETE` of one that runs cancels it, as its kind says, and then removes it. A `DELETE` of one that has ended removes it.
- Its log is its [event stream](#events), at `{job}/events` under its path, and kept as long as the job is.

A client that needs what a job did reads it from the job, or from its last event, not from the lines of its log.

### Events

A job's log is read with `GET`, answered `200` with `Content-Type: text/event-stream`: a stream of [server-sent events](https://html.spec.whatwg.org/multipage/server-sent-events.html), one entry of the log to each event.

```text
id: 1
data: {"stdout": "hello"}

id: 2
data: {"end": {"code": 0}}
```

- `id` is the entry's number in the log: `1` for the first, and one more for each after it.
- `data` is the entry, one JSON object with one member, written on one line. What its member is named and holds is the job's kind's.
- The server sets no other field, and MAY send comments, lines starting with `:`, to keep the connection open. A client ignores any other field, and any event with no `data`.

Every reader gets the whole log, from its first entry, as the job writes it.
A client that sends `Last-Event-ID: n` gets the entries after the `n`th only, so a client whose stream dropped picks up where it left off.

The last entry is `{"end": {...}}`, what the job ended with, and the stream ends after it.
A job that has ended answers its whole log at once, and ends there.
A job removed while a client reads its log ends that stream where it is.

A log is kept whole up to the server's limit, and no further: an entry past it is neither kept nor sent, and the first one past it is replaced by `{"truncated": {}}`, once.
The `end` entry is kept and sent anyway.

A slow reader slows only its own stream: the server does not buffer for it beyond the log it keeps.

### Image jobs

An image job copies an image between the server's [images](#images) and another registry, or makes one into them: built from a recipe, or of a sandbox.

| Request | Body | Answer |
| --- | --- | --- |
| `GET /images/jobs/` | | `200`, array of image jobs |
| `POST /images/jobs/pull/` | `{reference, auth?}` | `201`, the job |
| `POST /images/jobs/push/` | `{reference, auth?}` | `201`, the job |
| `POST /images/jobs/build/` | `{recipe, reference}` | `201`, the job |
| `POST /images/jobs/commit/` | `{sandbox, reference}` | `201`, the job |
| `GET /images/jobs/{id}` | | `200`, the job |
| `DELETE /images/jobs/{id}` | | `204` |
| `GET /images/jobs/{id}/events` | | `200`, its [log](#events) |
| `POST /images/prune` | `{}` | `200`, `{images, reclaimed}` |

An image job is `{id, kind, reference, running, digest?, error?}`:

- `kind` is `"pull"`, `"push"`, `"build"` or `"commit"`, and `reference` the one it was started with.
- `digest`, once it has succeeded, is the digest of the manifest it copied or built; for an image index, the index's own.
- `error`, once it has failed, is why, for a person to read.

Its log's entries are `{"log": "..."}`, a line of it, and `{"truncated": {}}`, then `{"end": {"digest": "..."}}` or `{"end": {"error": "..."}}`, as the job has them.

Pull and push copy an image between the server's images and another registry, the one `reference` names, as `docker pull` and `docker push` do: `ghcr.io/owner/name:tag` is `owner/name:tag` at `ghcr.io`, and a reference that names no registry, such as `alpine:3.20`, is at Docker Hub.
Each keeps the reference as it is on the server's side, so what a pull pulls is tagged `reference` here, and a push pushes the image `reference` tags here.
Each skips a blob the other side already has, and moves the tag on the receiving side to the image.

`auth` is the credentials for the other registry, as `{"type": "basic", "username", "password"}`, the password possibly an access token the registry takes in its place, or `{"type": "bearer", "token"}`.
Without it the server goes to the registry anonymously.
The server uses `auth` for that job alone: it MUST NOT keep it beyond the job, show it in the job or its log, write it to a log of its own, or send it to any other host.

A build builds an image from `recipe` into the server's images and tags it `reference`, as a push of it would.

A commit makes an image of [sandbox](#sandboxes) `sandbox` and tags it `reference`: the image it stands on, with what it has written to its root filesystem as one more layer, and that image's config.
It is how a sandbox is kept to start others from: any number boot the image, each with a disk of its own.

- Of a running sandbox, it is what its processes had written by the time the job started, as one moment: as a disk is when its machine stops, with everything written flushed first. The server MAY pause the sandbox while it copies, and its processes then go on as they were.
- Of a stopped one, it is its disk as it was left.
- It holds files, not processes, and the root filesystem alone: what the sandbox's volumes hold stays in them.
- The sandbox goes on as it was: a commit does not use it, and does not start a stopped one.

Cancelling one stops it:

- A build leaves no tag changed. Layers already built MAY be kept for reuse.
- A commit leaves no tag changed.
- A pull leaves no tag changed. Blobs already fetched MAY be kept for reuse.
- A push puts no manifest to the other registry. Blobs already pushed there stay.

The server MAY remove an image job that has ended, once a time it chooses has passed.

`POST /images/prune` deletes each image no reference points at and no [sandbox](#sandboxes), running or stopped, stands on, and what only such images held, such as their layers.
`images` is the digest of each image it deleted, and `reclaimed` how many bytes that freed.
It MAY leave what is too new to tell from what a running image job will point a reference at.

### Volumes

A volume is storage kept under a name, apart from any sandbox, and mounted into sandboxes.
Its files are reached at [`/volumes/{name}/files/`](#files).

| Request | Body | Answer |
| --- | --- | --- |
| `GET /volumes/` | | `200`, array of `{name, size}` |
| `POST /volumes/` | `{name?}` | `201`, `{name, size}` |
| `GET /volumes/{name}` | | `200`, `{name, size}` |
| `DELETE /volumes/{name}` | | `204` |

Without `name` the server picks one, and a name in use is refused with `409`, `exists`.
`size` is the bytes the volume's files take on the server.
`DELETE` deletes everything in it, and refuses a volume a sandbox mounts with `409`, `in_use`.

#### Mounts

Remote sources, such as Notion or Google Drive, can be mounted at a path inside a volume.
`{path}` is that path, each segment percent-encoded, as in [Files](#files).

| Request | Body | Answer |
| --- | --- | --- |
| `GET /volumes/{name}/mounts/` | | `200`, array of `{path, mount_type}` |
| `PUT /volumes/{name}/mounts/{path}` | `{source}` | `201`, `{path, mount_type}` |
| `DELETE /volumes/{name}/mounts/{path}` | | `204` |

`source` is what is mounted, and the credentials to reach it:

```text
{"type": "notion", "api_key": "..."}
{"type": "gdrive", "client_id": "...", "client_secret": "...", "refresh_token": "..."}
{"type": "onedrive", "client_id": "...", "client_secret": "...", "refresh_token": "..."}
```

`mount_type` is its `type`; its credentials are never answered.
The mount is a directory it creates, not the root and not inside another mount, so its parent must exist and it must not.
`PUT` refuses a path that already has a mount with `409`, `exists`.
`DELETE` takes the mount and its directory away, and leaves the source as it is.

### Sandboxes

A sandbox is a VM booted from an image, with [volumes](#volumes) mounted into it, and a disk of its own: what its processes write to its root filesystem, over its image.
Its disk outlives its VM. A sandbox stops, when it is not used or is told to, and starts again as it was left, under the same id, until it is deleted.

| Request | Body | Answer |
| --- | --- | --- |
| `GET /sandboxes/` | | `200`, array of sandboxes |
| `POST /sandboxes/` | `{image, name?, ephemeral?, auto_stop_minutes?, auto_delete_days?, disk_gib?, network?, ports?, vcpus?, memory_mib?, gpu?, gpu_memory_mib?, volumes?}` | `201`, the sandbox, with `ports` |
| `GET /sandboxes/{id}` | | `200`, the sandbox |
| `POST /sandboxes/{id}/start` | `{}` | `200`, the sandbox, with `ports` |
| `POST /sandboxes/{id}/stop` | `{}` | `200`, the sandbox |
| `DELETE /sandboxes/{id}` | | `204` |
| `PUT /sandboxes/{id}/ports/{port}` | `{label?, protocol?, inbound?}` | `201`, the port; `200` for one open already |
| `DELETE /sandboxes/{id}/ports/{port}` | | `204` |

A sandbox is `{id, name?, image, state, ephemeral, auto_stop_minutes, auto_delete_days, used_at}`, and in the answer to its `POST` and to `start`, `ports` too:

- `image` in the body is `{"type": "tag", "tag": "name:tag"}` or `{"type": "digest", "digest": "algorithm:hex"}`, an image in the server's [images](#images); in the answer, the digest of the image it booted.
- `name`, if given, names it among its organization's sandboxes -- the server's, on one that keeps no organizations apart -- so a client finds it again with `GET /sandboxes/?name={name}` rather than keeping its id. A name another sandbox has is refused with `409`, `exists`; once that one is deleted, the name is free.
- `state` is `"running"`, its VM up, or `"stopped"`, its VM down and its disk kept.
- `ephemeral`, `false` without it, is whether it is deleted when it stops, rather than kept.
- `auto_stop_minutes` is how long it runs [unused](#use) before it stops. Without it the server picks it; the server MAY refuse a longer one than it allows.
- `auto_delete_days` is how long it is kept stopped and unused before it is deleted. `null` keeps it until its `DELETE`. Without it the server picks it.
- `used_at` is when it was last [used](#use), as RFC 3339.
- `disk_gib` is how much it may write to its root filesystem over its image, in GiB. Without it the server picks it, and `0` is refused.
- `network` is whether its processes reach a network at all, and is `true` without it.
- `vcpus`, `memory_mib`, `gpu` and `gpu_memory_mib` each leave it to the server without one. The server gives exactly what is asked or refuses; `0` for a number is refused, and so is a `memory_mib` under the least a machine boots in, which the refusal names. `gpu` `false` forbids a GPU on a server that would attach one, and `gpu_memory_mib` is refused without one.
- `volumes` are mounted into it in order, so a later path may be inside an earlier one, each as `{volume, path, readonly?}`: the volume by name, where it appears to processes, absolute, and whether a write in it fails. A path is read with repeated and trailing `/` dropped, so `//data/` is `/data`; one that is not absolute, has a `.` or `..` segment, or is `/`, and two at one path so read, are refused with `400`.
Its files are reached at [`/sandboxes/{id}/files/`](#files) while it runs.

A sandbox stands on the image it booted, by digest, for as long as it exists: started again, it boots that image, whatever its tag points at by then.
So the server keeps that image while the sandbox exists, as [Images](#images) says.

#### Use

A sandbox is used while a [job](#sandbox-jobs) runs in it, a connection through one of its [ports](#ports) is open, or a request to its [files](#a-sandboxs-files) is answered, and when a job is started in it.
`used_at` is the last of these; a `GET`, a listing, and a request it refuses because it is stopped do not use it.

#### Stopping

A sandbox stops after `auto_stop_minutes` unused, or at its `POST /sandboxes/{id}/stop`:

- Every job in it ends as a cancellation does, and every connection through its ports is closed.
- What its processes had written is put on its disk, and its VM is stopped. Its disk is kept with all of it.
- An ephemeral sandbox is deleted then, as its `DELETE` does.

`stop` answers once it has stopped, and answers a stopped one as it is.

#### Starting

A stopped sandbox starts at its `POST /sandboxes/{id}/start`, and when a job is started in it, which then runs once it has:

- Its VM boots again, from the image it stands on, over its disk as it was left.
- Its files are as they were when it stopped. Its processes are not: nothing that ran in it before runs, until something starts it anew.
- Its volumes are mounted again, and its ports opened again: at the same `url`s where the server can, and otherwise at others -- a server that runs it somewhere else this time, or one on the client's machine at other loopback addresses -- which `start` answers.

`start` answers once it runs, and answers a running one as it is.
A `GET`, a listing, and a connection to one of its ports do not start it.

#### Deleting

A sandbox is deleted by its `DELETE`, when it stops if it is ephemeral, and after `auto_delete_days` stopped and unused.
The server deletes one in no other way.
Deleting one stops it first if it runs, and then removes its disk; its volumes stay, and every request naming it is answered `404`, `not_found`.

#### Ports

`ports` in the body are ports in the sandbox to open, each a number, or `{port, label?, protocol?, inbound?}` to say what it is for, how it is reached and whom it lets in: `[5900, {"port": 5173, "label": "dev server", "protocol": "http"}]`.

- `label` is what it is for, free text of up to 128 characters, answered with it.
- `protocol` is `tcp`, the default: TCP, reached as below; or `http`: HTTP served in the sandbox, reached at its `url` as HTTP by a browser as by any client, WebSockets included -- over TLS, from a server reachable from other machines, the sandbox serving plain HTTP. A server that cannot reach a port over HTTP refuses `http` with `400`, `invalid`.
- `inbound` is the port's inbound rules, `[{source, description?}]`, each `source` an IPv4 or IPv6 CIDR or one address: virtx cloud's, beside the protocol, which answers them at `/sandboxes/{id}/ports/{port}/inbound`. A server that keeps none refuses `inbound` with `400`, `invalid`, rather than open a port to whoever it was meant to keep out.

They are reserved when the sandbox is made, and kept through every stop and start until it is deleted. `0` and a port given twice are refused.
A connection to a port of a stopped sandbox is closed, as one nothing listens on is: it does not start the sandbox.

A port is opened in a sandbox already made, running or stopped, with `PUT /sandboxes/{id}/ports/{port}`, its body `{label?, protocol?, inbound?}` as above -- `{}`, or none, for TCP named nothing -- and kept from then on as those it was made with are. Opened again, it is answered `200` with where it is now: how a client whose sandbox started without its `start`, for a job, learns a port's `url` if it changed; a `label` given then is its label from then on, and a `protocol` other than the one it was opened with is refused with `409`, `exists`. A program in the sandbox already listening on the port when it is opened MAY not be reached until it listens again.

A port is closed with `DELETE /sandboxes/{id}/ports/{port}`: its URL reaches it no more from then on, and it is not opened again as the sandbox starts; one not open is answered `404`, `not_found`. A server MAY refuse to close a port it opens in every sandbox, with `409`, `in_use`. To reach a port another way -- `http` rather than `tcp` -- it is closed and opened again, at another URL.

The `POST` answers each with where it is reached from, as `{port, url, label?, protocol?, address?}` -- `protocol` left out for `tcp`, `address` for a port that has none -- in the order given, and only there, in `start`, and in the `PUT` that opens one: a `GET` does not, since the URLs carry credentials.

- A server on the client's machine answers a loopback address, such as `http://127.0.0.1:54123`, on a port it picks. Only that machine reaches it.
- A server reachable from other machines answers a URL that carries the credentials it needs, and MUST NOT open a port that is reached without them but at its `address`. The URL is as secret as those credentials, and the client MUST keep it so.
- A server reachable from other machines MAY answer a `tcp` port with an `address` too, `host:port`, such as `tcp.example.com:20001`, where any TCP client reaches it directly, with no WebSocket and no credentials. It is not secret, as the URL is: what listens on the port answers whoever connects there, and authenticates them itself. The server MAY give one to some `tcp` ports and not others; an `http` port has none, and nor does any on a server on the client's machine.
- The URL is the server's to form: a client MUST NOT build one or take it apart.
- A connection reaches whatever in the sandbox listens on that port, whichever address it listens on, and is closed when nothing does. Over a remote URL, a `tcp` port's TCP is carried in a WebSocket to that URL, opened with the subprotocol `virtx.tcp`; an `http` port's URL is an `https` one that a browser opens.
- Ports are the way in, and do not need `network`, which is the way out.

### Sandbox jobs

A sandbox job is a process in a [sandbox](#sandboxes), run beside whatever else runs there.

| Request | Body | Answer |
| --- | --- | --- |
| `GET /sandboxes/{id}/jobs/` | | `200`, array of sandbox jobs |
| `POST /sandboxes/{id}/jobs/exec/` | `{cmd, stdin?, timeout_ms?}` | `201`, the job |
| `GET /sandboxes/{id}/jobs/{job}` | | `200`, the job |
| `DELETE /sandboxes/{id}/jobs/{job}` | | `204` |
| `POST /sandboxes/{id}/jobs/{job}/signal` | `{signal}` | `204` |
| `POST /sandboxes/{id}/jobs/{job}/stdin` | `{data?, eof?}` | `204` |
| `GET /sandboxes/{id}/jobs/{job}/events` | | `200`, its [log](#events) |

A sandbox job is `{id, kind, pid, cmd, running, code?}`:

- `kind` is `"exec"`.
- `pid` is its process's pid in the sandbox. The job is named by `id`, not by it, so a pid the sandbox gives again never names another job.
- `cmd` is the argv it was started with, with no shell: `["sh", "-c", "..."]` for one. An empty one is refused.
- `code`, once it has ended, is its exit status, or `128 + signal` for one a signal ended.

A job started in a stopped sandbox [starts](#starting) it first.
A command that cannot be started, such as one not found, is refused with `422`, `not_started`, and makes no job.

Its log's entries are each an object with one member:

- `{"stdout": "..."}` or `{"stderr": "..."}`: a line the process wrote there, without its line ending, decoded as UTF-8 with each invalid sequence replaced by U+FFFD. A line longer than the server's limit is sent in parts, an entry each.
- `{"truncated": {}}`, as [Events](#events) says.
- `{"end": {"code": n}}`, last.

Lines of one stream are in the order the process wrote them; lines of the two, in the order the server read them.

The process ends when it exits, when its `timeout_ms` runs out, or when its job is cancelled: each of the last two kills it and every process it started, as `SIGKILL` does.
The job, and its log, are kept until its `DELETE` or its sandbox's deletion: a sandbox that stops ends its jobs and keeps them.

`signal` sends `signal` to the process and every process it started, and answers once it is sent: what the process does with it, its log says.
`signal` is a name as Linux gives it, such as `"SIGTERM"` or `"SIGKILL"`, and a name the server does not know is refused.
A job that has ended refuses it with `409`, `ended`.

Without `stdin`, or with it `false`, the process's stdin is empty.
With `stdin` `true`, the client writes to it with `POST {job}/stdin`:

- `data` is a string, written to the process's stdin as UTF-8, as it is.
- `eof` `true` closes the process's stdin, after any `data` in the same request.
- The server answers once it has written `data`, so a process that does not read holds the answer back.
- A client sends a job's next `stdin` only once the one before it is answered, so they are written in the order sent.
- A job started without `stdin`, one whose stdin is closed, and one that has ended refuse it with `409`, `ended`.

## Images

The server keeps images as an OCI registry, and `/images/oci/v2/` is its API: the [OCI Distribution Specification](https://github.com/opencontainers/distribution-spec/blob/v1.1.1/spec.md) v1.1, with every path the specification roots at `/v2/` rooted at `/images/oci/v2/` instead.
A client gives `/images/oci` as the registry's base path, since tools that assume `/v2/` at the root do not reach it.

The server MUST support the specification's Pull, Push, Content Discovery and Content Management categories.
It also answers `GET /images/oci/v2/_catalog`, which the specification does not define, with the repositories it holds as `{"repositories": [...]}`, paged by `n` and `last` as tag listing is.

- Repository: the name of an image, an OCI repository name such as `alpine` or `team/base`.
- Tag: a name within a repository, such as `3.20`. It points at one manifest, and can be moved to another.
- Reference: a repository and a tag, as `name:tag` (such as `alpine:3.20`).
- Digest: the hash of a manifest, as `algorithm:hex` (such as `sha256:0123abcd...`). It identifies one image's content and never changes. A tag may point at an image index, and a sandbox then boots the manifest in it for the server's platform.

Images are listed, tagged and removed with the specification's requests:

| To | Request |
| --- | --- |
| List images | `GET /images/oci/v2/_catalog`, then `GET /images/oci/v2/{name}/tags/list` |
| Tag an image | `PUT /images/oci/v2/{name}/manifests/{tag}` with its manifest |
| Untag a reference | `DELETE /images/oci/v2/{name}/manifests/{tag}` |
| Remove an image | `DELETE /images/oci/v2/{name}/manifests/{digest}`, which also removes every tag pointing at it; refused with `409 Conflict` and the code `DENIED` for one a sandbox stands on |
| Export an image | `GET` its manifest, and each blob it names |
| Import an image | Push each blob, then `PUT` its manifest by tag |

A manifest no tag points at, and a blob no manifest names, MAY be deleted by the server at any time -- but not one a [sandbox](#sandboxes) stands on, running or stopped, which is kept while the sandbox is.
A client pushing an image therefore puts its manifest by tag, not by digest alone.

The server copies images to and from other registries with [pull and push jobs](#image-jobs), which take that registry's credentials.
A client that would rather keep them can copy an image itself instead: it fetches the image from that registry and pushes it here, skipping each blob a `HEAD` shows the server already has, or the other way round.

A request that fails is answered with the specification's error codes, in its `{"errors": [...]}` body.

## Files

The files in a volume are reached at `/volumes/{name}/files/`, and those in a sandbox at `/sandboxes/{id}/files/`, with [WebDAV](https://www.rfc-editor.org/rfc/rfc4918) (RFC 4918).
The path after the root is the file's path in the volume or sandbox, each segment percent-encoded.
A client resolves a path's `.` and `..` segments before it sends it, as a file's path is read: empty segments are dropped, and each `..` takes the segment before it, so `a/../b` and `a//../b` are both sent as `b`; a path that goes above the root is not sent.
The server refuses with `400 Bad Request` a path, or the `Destination` of a `MOVE` or `COPY` up to its `?` or `#`, with a `.` or `..` segment, plain or percent-encoded, `/` and `\` both separating segments; and one that is not in the root (of a URL, its path alone, with consecutive slashes taken as one). It does so before anything in the volume or sandbox is touched.

The server MUST support WebDAV class 1, and MAY support class 2 (locking).

| To | Request |
| --- | --- |
| Read metadata | `PROPFIND`, `Depth: 0` |
| List a directory | `PROPFIND`, `Depth: 1` |
| Read a file | `GET`, with `Range` for a part of it |
| Write a file | `PUT` |
| Create a directory | `MKCOL` |
| Remove | `DELETE` |
| Rename | `MOVE`, with `Overwrite` |
| Copy | `COPY`, with `Overwrite` |

- `PROPFIND` answers at least `resourcetype`, `getcontentlength`, `getlastmodified` and `getetag`. The server MAY refuse `Depth: infinity`, as RFC 4918 lets it.
- `PUT` replaces the whole file, and creates it if it does not exist. A `PUT` with `Content-Range` is refused with `400 Bad Request`: a file is written whole, and lands whole. Until all of the body has arrived, what was at the path is what is read there, and a `PUT` broken off leaves it. A `PUT` with `If-Match` is refused with `412 Precondition Failed` unless the file's `getetag` is among the tags given, as RFC 9110 says: how a client writes only over the file it read.
- `PUT` and `MKCOL` under a directory that does not exist are refused with `409 Conflict`, as RFC 4918 says. `MKCOL` creates one directory, not its parents.
- `DELETE` of a directory removes everything in it. A `DELETE`, `COPY` or `MOVE` of a directory that fails in part answers `207 Multi-Status`, with the status of each path that failed, as RFC 4918 says. `DELETE` or `MOVE` of the root, and `MOVE` or `COPY` onto it, are refused with `403 Forbidden` before anything in it is touched: a volume or sandbox is removed through [Resources](#resources).
- `MOVE` and `COPY` take a `Destination` in the same volume or sandbox, and refuse any other with `400 Bad Request`.
- A request naming a volume or sandbox that does not exist is answered `404 Not Found`.

### A sandbox's files

A sandbox's files are its root filesystem as its processes see it, the volumes mounted into it among them, while it runs: what a process writes is read here at once, and what is written here a process reads.

- The server MAY leave out what is not a file to read or write, such as `/proc`, `/sys` and `/dev`, and what is neither a file nor a directory, such as a link or a device: it is not listed, and a request naming it is answered as if nothing were there.
- A file a `PUT` makes is owned as one a process in the sandbox makes is; one it replaces keeps its mode and owner.
- A write to a volume mounted `readonly` is refused with `403 Forbidden`.
- A request to a sandbox that is stopped is refused with `409 Conflict`, `stopped`, and does not start it: its files are read and written while it runs.

In a volume, a remote source [mounted](#mounts) at a path is reached at that path, and a request under it goes to the source. It fails, with the status the server chooses, where the source cannot do it, such as a `MOVE` the source does not support.
