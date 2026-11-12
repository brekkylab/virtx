"""The server a client of this version fetches, on the platform this runs on: the wheel's
library fetches `virtx-stdio` from `$VIRTX_DIST_URL` into `$VIRTX_HOME`, starts it, and it
answers. No call here boots a machine."""

import asyncio
import os
import sys

import virtx

failures = []


def check(ok, what):
    print(("PASS " if ok else "FAIL ") + what, flush=True)
    if not ok:
        failures.append(what)


async def main():
    home = os.path.realpath(os.environ["VIRTX_HOME"])
    path = await virtx.ensure_virtx()
    check(
        os.path.isfile(path) and os.path.realpath(path).startswith(home),
        f"the server is fetched into $VIRTX_HOME: {path}",
    )

    client = virtx.Client.spawn()
    try:
        check(isinstance(await client.list_image(), list), "the server lists images")
        check(isinstance(await client.list_sandbox(), list), "the server lists sandboxes")
    finally:
        await client.aclose()


asyncio.run(main())
if failures:
    print(f"\n{len(failures)} failed")
    sys.exit(1)
