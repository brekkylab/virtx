"""The Python package as a user installs it, on the platform this runs on: the library it opens
is the one its wheel carries, and a call into it answers."""

import os
import sys

import virtx

failures = []


def check(ok, what):
    print(("PASS " if ok else "FAIL ") + what, flush=True)
    if not ok:
        failures.append(what)


package = os.path.dirname(virtx.__file__)
check(hasattr(virtx, "Client"), f"the package loads from {package}")

# Opening the library is the first call that needs it; a server that cannot start is the one
# answer that needs no server.
try:
    virtx.Client.spawn(os.path.join(os.getcwd(), "no-such-virtx-server"))
    check(False, "spawning a missing server raises")
except virtx.VirtxError as e:
    check("starting the server" in str(e), f"spawning a missing server raises: {e}")
opened = virtx.library_path()
check(
    opened is not None and os.path.dirname(opened) == package,
    f"the library is the wheel's: {opened}",
)

digest = "sha256:" + "0" * 64
check(virtx.image_name(digest) == virtx.ImageDigest(digest), "an image name reads as a digest")

if failures:
    print(f"\n{len(failures)} failed")
    sys.exit(1)
