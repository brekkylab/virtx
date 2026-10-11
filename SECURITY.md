# Security Policy

## Reporting a vulnerability

Report privately through [GitHub's advisory form](https://github.com/brekkylab/virtx/security/advisories/new), or by email to contact@brekkylab.com. Please do not open a public issue for a suspected vulnerability, and do not include a working exploit or a real credential in any report.

Include the language you used virtx from, the package version, the host OS and its FUSE provider, what you ran, what you expected, and what happened. A reproduction that needs no remote service, such as one on an in-memory `Directory` or a `Recipe` on `alpine:latest`, is ideal.

We aim to acknowledge a report within three working days and to describe the fix or the disagreement within fourteen. Fixes ship in a normal release, credited to the reporter unless they ask otherwise.

Only the latest release on crates.io, PyPI and npm is supported. The console server binary that `ensure_virtx()` fetches is this project's too: a report about it comes here as well.

## What counts

virtx draws the line between a task and the host it runs on, so the interesting failures are the ones that break the boundaries it claims to enforce:

- **The VM reaching the host beyond what was mounted.** A command in a session that reads or writes a host path outside the session's mounts, writes under a mount declared read-only, or reaches the network from a session built with `network(false)`.
- **A mount exposing more than its tree.** A path that escapes a mounted `Directory` or host directory, a read-only mount that accepts a write, or a mount that stays reachable after its guard is dropped or its process is gone.
- **A credential reaching somewhere it was not given to.** A token for S3, Notion, Google Drive or OneDrive, or anything else from the host environment, that ends up inside the VM, in a command's output, or in a log.
- **Code execution from data.** A filesystem entry, an image recipe, or a message from the console server that runs on the host rather than being handed to the VM.

## What does not

- **What a command does inside its VM.** The session is disposable, and what it installs, deletes or runs there is the caller's business. The boundary is the host.
- **Binding and exposure are the operator's call.** The console server speaks to the process that started it over stdio; putting a session behind a public interface is a deployment decision.
- **The fixtures in the tests are not secrets.** The in-memory stores and the object-store endpoints the tests build on hold nothing real.
- **A FUSE provider's own defects.** A failure inside FUSE-T, Dokany or the kernel's FUSE belongs to that project; report it here if you are unsure and we will pass it on.
