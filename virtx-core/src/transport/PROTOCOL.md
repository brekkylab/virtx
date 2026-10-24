# virtx protocol — transports (draft)

How a virtx client and server exchange messages: what a message is, and the two transports
that carry them, **stdio** and **Streamable HTTP**. The methods themselves are not here.

This is a draft. The words MUST, SHOULD and MAY are used as in RFC 2119, and the
[open questions](#open-questions) are not settled by anything below.

The transports follow MCP's (specification 2025-06-18, "Transports") where MCP's reasons
hold here too, and say so where they part from it.

## Messages

- A message is a JSON-RPC 2.0 request, notification or response, encoded as CBOR
  (RFC 8949): the object JSON-RPC defines is a map with text-string keys, and its members
  keep JSON-RPC's names and meanings, `"jsonrpc": "2.0"` included. Examples here are in CBOR
  diagnostic notation.
- A message is one CBOR data item, a map, with nothing after it. Batches (arrays) are not
  used: a client MUST NOT send one, and a server MAY refuse one.
- Bytes are a byte string, not text: CBOR carries them as they are.

### Frames

Where messages follow one another in a byte stream (stdio, and a streamed HTTP answer), each
is a frame: the self-described CBOR tag (55799) over a byte string whose bytes are the
message, with the string's length always in its four-byte form. A frame is the eight bytes

```text
d9 d9 f7 5a L L L L
```

where `L L L L` is the message's length, big-endian, followed by the message. A stream of
frames is a CBOR sequence (RFC 8742), and a reader finds the next frame by its first four
bytes.
- Only the client sends requests. The server answers them, and MAY send notifications
  about a request while it runs (see [Notifications](#notifications)). The server never
  sends a request.

## stdio

The client starts the server as a child process.

- The server reads messages from its stdin and writes messages to its stdout, each as a
  [frame](#frames).
- The server MUST NOT write anything to stdout that is not a frame. It MAY write UTF-8
  text to stderr for a person to read; the client MAY capture, forward or ignore it.
- A client SHOULD skip stdout bytes outside a frame, rather than end the session over them.
- The client MUST NOT write anything to the server's stdin that is not a message.
- The client ends the session by closing the server's stdin, and the OS closes it for a
  client whose process ends. The server SHOULD then cancel what is in flight and exit,
  without waiting for those requests to finish. A client MAY kill a server that has not
  exited within a time it chooses.
- Requests may be in flight concurrently; responses are matched to requests by `id`, not by
  order.

## Streamable HTTP

The server offers one HTTP endpoint (for example `https://host/rpc`), to which every
message is sent. Each `POST` stands alone: there are no sessions, so the server keeps
nothing between them for the transport's sake, and a client MAY have any number of requests
in flight at once. MCP, by contrast, opens a session with `initialize` and names it in an
`Mcp-Session-Id` header.

### Sending a message

1. Every message from the client is a new `POST` to the endpoint, with
   `Content-Type: application/cbor` and the message, unframed, as the body.
2. The client MUST send an `Accept` header listing `application/cbor`, and SHOULD also list
   `application/cbor-seq`. A client that does not list `application/cbor-seq` receives no
   [notifications](#notifications) over HTTP.
3. If the message is a notification, the server answers `202 Accepted` with no body, or an
   HTTP error status if it does not accept it.
4. If the message is a request, the server answers `200` with either
   - `Content-Type: application/cbor` and the response, unframed, as the body, or
   - `Content-Type: application/cbor-seq`, a stream of [frames](#frames) (only to a client
     that listed it).
5. On a stream, the server sends notifications about the request first, then the response,
   and then ends the body. A server MUST NOT end the stream before the response.
6. A dropped connection is not a cancellation: the server MAY run the request to its end.

The server chooses per request. It SHOULD answer with one message when it has nothing to
send before the response, so a request with no notifications costs one plain HTTP exchange.

### Other methods

Any other method to the endpoint is answered `405 Method Not Allowed`, with `Allow: POST`.
MCP uses `GET` for a stream the server sends unprompted messages on, and for resuming a
stream, and `DELETE` to end a session; this protocol has none of them.

### Security

- A server a browser can reach (on the user's machine or their network) and that takes no
  credentials SHOULD answer `403 Forbidden` to a request whose `Origin` header names an
  origin it does not allow, so a web page cannot reach it through DNS rebinding. A request
  with no `Origin` (any non-browser client) is not refused for that. A server that requires
  credentials needs no such check: the page has none to send.
- A server on the local machine SHOULD bind to `127.0.0.1`, not to every interface.
- A server MAY require credentials on every request, in headers such as `Authorization`.
  It answers `401 Unauthorized` to a request without them and `403 Forbidden` to one whose
  credentials may not make it.

## Notifications

While a request runs, the server MAY send its progress, such as a build's output, as
`notifications/progress` notifications:

```text
{"jsonrpc": "2.0", "method": "notifications/progress", "params": {"requestId": 7, "message": "pulling alpine:3.20"}}
```

- `requestId` is the `id` of the request the progress is about, exactly as the client sent
  it: a client tells concurrent requests' progress apart by it, as it does their responses.
- `message` is a string: one line of the request's log, for a person to read.
- A request's progress comes before its response. A client MUST be ready for none, and MAY
  ignore what it does not want.
- A notification from the client has no `id`, so the server sends no progress about it.
- The server sends no other notification.
- Over Streamable HTTP, progress goes on that request's stream, before its response.
- Over stdio, it is written to stdout between responses, as any message is.

## Open questions

- **Resuming a stream.** MCP lets a server put an `id` on each SSE event, and a client that
  lost the stream `GET` the endpoint with `Last-Event-ID` to have the rest of it replayed.
  A stream of frames has no ids, so a client that loses one loses the rest of it.
- **Cancellation.** A client cannot cancel one request: over stdio it can only end the
  session, which cancels them all, and over Streamable HTTP not even that. MCP has a
  `notifications/cancelled`, which names the request by its `requestId` as progress does
  here.
- **Asking for no progress.** A server sends a request's progress whether or not the client
  wants it. MCP sends it only to a request carrying a `progressToken`.
- **Version.** Nothing yet tells a server which version of this protocol a client speaks.
  MCP sends `MCP-Protocol-Version` on every HTTP request.
