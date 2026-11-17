"""The ``virtx`` command the package installs, run as a process of its own against the
fixture's ``virtx-stdio``."""

import os
import subprocess
import sys

import virtx
from conftest import ALPINE, familiar, tags
from virtx import Client


def virtx_command(*args: str) -> subprocess.CompletedProcess[str]:
    env = {**os.environ, virtx.LIBRARY_ENV: str(virtx.library_path())}
    return subprocess.run(
        [sys.executable, "-c", "from virtx._cli import main; main()", *args],
        check=False,
        capture_output=True,
        text=True,
        env=env,
    )


async def test_the_command_runs_against_a_server(server: str) -> None:
    pulled = virtx_command("--server", server, "pull", "-q", ALPINE)
    assert pulled.returncode == 0, pulled.stderr
    with Client.spawn(server) as client:
        digest = (await tags(client))[ALPINE]
    reference, said = pulled.stdout.split()
    assert (familiar(reference), said) == (ALPINE, digest)


def test_a_bad_argument_exits_as_clap_does() -> None:
    refused = virtx_command("images", "--bogus")
    assert refused.returncode == 2
    assert "unexpected argument '--bogus'" in refused.stderr
