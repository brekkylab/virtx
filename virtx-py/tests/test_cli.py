"""The ``virtx`` command the package installs, run as a process of its own against
``virtx-core``'s ``test_server`` example."""

import os
import subprocess
import sys

import virtx


def virtx_command(*args: str) -> subprocess.CompletedProcess[str]:
    env = {**os.environ, virtx.LIBRARY_ENV: str(virtx.library_path())}
    return subprocess.run(
        [sys.executable, "-c", "from virtx._cli import main; main()", *args],
        check=False,
        capture_output=True,
        text=True,
        env=env,
    )


def test_the_command_runs_against_a_server(test_server: str) -> None:
    pulled = virtx_command("--server", test_server, "pull", "-q", "alpine:3.20")
    assert (pulled.returncode, pulled.stdout) == (0, "alpine:3.20 sha256:0\n"), pulled.stderr


def test_a_bad_argument_exits_as_clap_does() -> None:
    refused = virtx_command("images", "--bogus")
    assert refused.returncode == 2
    assert "unexpected argument '--bogus'" in refused.stderr
