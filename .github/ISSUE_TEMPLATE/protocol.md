---
name: Protocol change
about: A change to what a console client and the console server say to each other
title: ''
labels: protocol
---

<!-- Both ends of the protocol live in `src/protocol/`, but the console server that speaks it is a prebuilt binary a maintainer releases. A change to the wire is planned here first, so the client change and the server release can go out together. CONTRIBUTING.md has the rule. -->

**What the client cannot say today** — the gap, as a user of the crate or a binding meets it, and what they do instead.

**The change on the wire** — the call, field or response to add or change, with its shape. `src/protocol/PROTOCOL.md` is the vocabulary to spell it in.

**What the server does with it** — the behaviour inside the VM the message asks for. This half is the maintainers' to build.

**Compatibility** — whether a current client against the new server, and the new client against a current server, still work, and if not, what fails and whether `PROTOCOL_VERSION` moves.

**What else moves** — the console builder or result it reaches, the bindings, `PROTOCOL.md`.

**Who does the client half** — say if you intend to send the pull request for `src/protocol/` and the console, or are reporting the need only.
