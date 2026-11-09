---
name: Protocol change
about: A change to what a client and the server say to each other
title: ''
labels: protocol
---

<!-- Both ends of the protocol live in this repository: the types and transports in `virtx-core/`, the servers in `virtx-server-stdio/` and `virtx-server-tcp/`. A change to the wire is agreed here first, then made to both ends in one pull request. CONTRIBUTING.md has the rule. -->

**What the client cannot say today** — the gap, as a user of one of the packages meets it, and what they do instead.

**The change on the wire** — the call, field or response to add or change, with its shape. `virtx-core/PROTOCOL.md` is the vocabulary to spell it in.

**What the server does with it** — the behaviour inside the VM the message asks for.

**Compatibility** — whether a current client against the new server, and the new client against a current server, still work, and if not, what fails.

**What else moves** — the packages it reaches, the servers, `PROTOCOL.md`.

**Who does it** — say if you intend to send the pull request, or are reporting the need only.
