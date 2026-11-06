"""The Python package against ``virtx-uvm``: the server ``Client.spawn()`` starts when it
names none, which ``$VIRTX_UVM`` points at the one built here."""

import subprocess
import sys

import pytest

from virtx import Client, ImageJobEnd, SandboxJobEnd, Stderr, Stdout, VirtxError

IMAGE = "alpine:3.20"


async def test_a_pulled_image_boots_a_sandbox_that_runs_a_command() -> None:
    with Client.spawn() as client:
        end = [entry.entry async for entry in client.pull_image(IMAGE)][-1]
        assert isinstance(end, ImageJobEnd), end
        assert end.digest and end.digest.startswith("sha256:"), end.error
        assert any(image.digest == end.digest for image in await client.list_image())

        sandbox = await client.create_sandbox(IMAGE)
        try:
            assert sandbox.running and sandbox.image == end.digest
            log = [
                entry.entry
                async for entry in client.exec_sandbox(
                    sandbox.id, ["sh", "-c", "echo one; echo two >&2; exit 3"]
                )
            ]
            # stdout and stderr are read apart, so one's lines may come before the other's.
            assert sorted(log[:-1], key=str) == [Stderr("two"), Stdout("one")]
            assert log[-1] == SandboxJobEnd(3)
        finally:
            await client.remove_sandbox(sandbox.id)
        with pytest.raises(VirtxError, match="404"):
            await client.get_sandbox(sandbox.id)


def test_the_command_pulls_through_the_server_it_starts() -> None:
    pulled = subprocess.run(
        [sys.executable, "-c", "from virtx._cli import main; main()", "pull", "-q", IMAGE],
        check=False,
        capture_output=True,
        text=True,
    )
    assert pulled.returncode == 0, pulled.stderr
    reference, digest = pulled.stdout.split()
    assert (reference, digest[:7]) == (IMAGE, "sha256:")
