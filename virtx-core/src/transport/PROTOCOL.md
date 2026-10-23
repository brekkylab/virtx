# virtx protocol — transports (draft)

How a virtx client and server exchange messages: what a message is, and the two transports
that carry them, **stdio** and **Streamable HTTP**. The methods themselves are not here.

This is a draft. The words MUST, SHOULD and MAY are used as in RFC 2119, and the
[open questions](#open-questions) are not settled by anything below.

The transports follow MCP's (specification 2025-06-18, "Transports") where MCP's reasons
hold here too, and say so where they part from it.

## Messages

- A message is a JSON-RPC 2.0 request, notification or response, encoded as UTF-8 JSON.
- A message is one JSON object. Batches (JSON arrays) are not used: a client MUST NOT send
  one, and a server MAY refuse one.
- Only the client sends requests. The server answers them, and MAY send notifications
  about a request while it runs (see [Notifications](#notifications)). The server never
  sends a request.

## stdio

The client starts the server as a child process.

- The server reads messages from its stdin and writes messages to its stdout, one message
  per line: each is followed by `\n` and contains no newline itself.
- The server MUST NOT write anything to stdout that is not a message. It MAY write UTF-8
  text to stderr for a person to read; the client MAY capture, forward or ignore it.
- A client SHOULD skip a stdout line that does not start with `{`, rather than end the
  session over it.
- The client MUST NOT write anything to the server's stdin that is not a message.
- The client ends the session by closing the server's stdin, and the server then exits:
  see [Ending a session](#ending-a-session). A client MAY kill a server that has not exited
  within a time it chooses.
- Requests may be in flight concurrently; responses are matched to requests by `id`, not by
  order.

## Streamable HTTP

The server offers one HTTP endpoint (for example `https://host/rpc`), to which every
message is sent.

### Sending a message

1. Every message from the client is a new `POST` to the endpoint, with
   `Content-Type: application/json` and the message as the body.
2. The client MUST send an `Accept` header listing `application/json`, and SHOULD also list
   `text/event-stream`. A client that does not list `text/event-stream` receives no
   [notifications](#notifications) over HTTP.
3. If the message is a notification, the server answers `202 Accepted` with no body, or an
   HTTP error status if it does not accept it.
4. If the message is a request, the server answers `200` with either
   - `Content-Type: application/json` and the response as the body, or
   - `Content-Type: text/event-stream`, an SSE stream (only to a client that listed it).
5. On an SSE stream, each event's `data` is one message. The server sends notifications
   about the request first, then the response, and then closes the stream. A server MUST
   NOT close the stream before the response unless the session has ended.
6. A dropped connection is not a cancellation: the server MAY run the request to its end,
   or until the session ends.

The server chooses per request. It SHOULD answer with JSON when it has nothing to send
before the response, so a request with no notifications costs one plain HTTP exchange.

### Sessions

1. A `POST` without a `Virtx-Session-Id` header opens a session: the server's answer carries
   `Virtx-Session-Id` with the new session's id.
   - An id is visible ASCII (0x21–0x7E), and SHOULD be unguessable (for example a random
     UUID).
2. The client MUST send that header on every later request of the session.
   - Until the client has the id, it has no session to put a second request in: it MUST
     NOT send another message until the first is answered.
3. To a request naming a session it does not have (never opened, ended, or expired), the
   server answers `404 Not Found`. The client MUST NOT reuse that id.
4. The client ends a session with a `DELETE` to the endpoint carrying its
   `Virtx-Session-Id`. The server answers `204 No Content`, or `404` if there was no such
   session.
5. A server MAY also end a session the client has left idle: see
   [Ending a session](#ending-a-session).

Unlike MCP, opening a session is not tied to an `initialize` method: any first request
opens one, since this protocol has no handshake method.

### Other methods

A `GET` to the endpoint is answered `405 Method Not Allowed`. MCP uses `GET` for a stream
the server sends unprompted messages on, and for resuming a stream; this protocol has
neither yet.

### Security

- A server MUST answer `403 Forbidden` to a request whose `Origin` header names an origin
  it does not allow, so a web page cannot reach a server on the user's machine through the
  user's browser. A request with no `Origin` (any non-browser client) is not refused for
  that.
- A server on the local machine SHOULD bind to `127.0.0.1`, not to every interface.

## Notifications

While a request runs, the server MAY send its progress, such as a build's output, as
`notifications/progress` notifications:

```json
{"jsonrpc": "2.0", "method": "notifications/progress", "params": {"requestId": 7, "message": "pulling alpine:3.20"}}
```

- `requestId` is the `id` of the request the progress is about, exactly as the client sent
  it: a client tells concurrent requests' progress apart by it, as it does their responses.
- `message` is a string: one line of the request's log, for a person to read.
- A request's progress comes before its response. A client MUST be ready for none, and MAY
  ignore what it does not want.
- A notification from the client has no `id`, so the server sends no progress about it.
- The server sends no other notification.
- Over Streamable HTTP, progress goes on that request's SSE stream, before its response.
- Over stdio, it is written to stdout between responses, as any message is.

## Ending a session

A session ends when the client ends it, or when the client is gone without ending it: it
exited, crashed, or was killed. Either way nothing will read what the session's requests
answer, so when a session ends the server SHOULD cancel the requests still running in it and
release what the session holds.

How the server learns that a session has ended depends on the transport:

| | The client ends it | The client is gone |
|---|---|---|
| stdio | It closes the server's stdin. | The OS closes the server's stdin as the client's process ends. |
| Streamable HTTP | It sends `DELETE`. | Nothing tells the server. |

- **stdio.** End of input on stdin is the end of the session, whichever way it came. The
  server SHOULD then cancel what is in flight and exit, without waiting for those requests
  to finish.
- **Streamable HTTP.** A dropped connection is not a sign that the client is gone (it may
  reconnect), so a server learns of a client that left without a `DELETE` only by its
  silence. A server SHOULD end a session that has been idle (no request in flight and none
  received) for longer than a time it chooses, and from then on answer that session's id
  with `404`. A client MUST be ready for a session to end that way.

## Open questions

- **Resuming a stream.** MCP lets a server put an `id` on each SSE event, and a client that
  lost the stream `GET` the endpoint with `Last-Event-ID` to have the rest of it replayed.
  Until this protocol defines that, a server MUST NOT rely on a client resuming, and the
  `id` field of an event carries no meaning.
- **Cancellation.** Ending the session cancels its requests, but a client cannot yet cancel
  one request and keep the session. MCP has a `notifications/cancelled`, which names the
  request by its `requestId` as progress does here.
- **Asking for no progress.** A server sends a request's progress whether or not the client
  wants it. MCP sends it only to a request carrying a `progressToken`.
- **Version.** Nothing yet tells a server which version of this protocol a client speaks.
  MCP sends `MCP-Protocol-Version` on every HTTP request.
- **What a session holds**, and so what ending one releases.
- **How long an idle session lives.** The server chooses for now, and a client cannot ask
  for longer or learn the time; a client that idles past it loses the session. A fixed
  minimum, a header that states the time, or a method that keeps a session alive would
  each settle it.
