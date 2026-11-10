"""Builds ``libvirtx_core`` once and opens it, unless ``$VIRTX_CORE_LIB`` names one, and serves
the fixture for the run: ``cargo xtask fixture --serve``, which builds the servers, serves them
over a home of the run's own, and names them, unless the environment names them already, as
``cargo xtask test`` does. They boot machines, so they need ``cargo xtask init`` and the guest
kernel first."""

import itertools
import os
import subprocess
import sys
from collections.abc import Iterator
from pathlib import Path

import pytest

import virtx
from virtx import BearerAuth, Client

ROOT = Path(__file__).resolve().parents[2]
TARGET = Path(os.environ.get("CARGO_TARGET_DIR") or ROOT / "target")

ALPINE = "mirror.gcr.io/library/alpine:3.20"
"""The image every test that boots a sandbox pulls first: Docker Hub's alpine, from Google's
mirror of its official images, as Docker Hub refuses anonymous pulls past a few an hour."""


_fixture: subprocess.Popen[str] | None = None


def pytest_configure(config: pytest.Config) -> None:
    global _fixture
    subprocess.run(["cargo", "build", "-p", "virtx-core", "--lib"], cwd=ROOT, check=True)
    name = {"win32": "virtx_core.dll", "darwin": "libvirtx_core.dylib"}.get(
        sys.platform, "libvirtx_core.so"
    )
    virtx.load(os.environ.get(virtx.LIBRARY_ENV) or TARGET / "debug" / name)

    if "VIRTX_TEST_URL" in os.environ:
        return
    # It says what it names, a `NAME=value` line each, then `ready`; it serves until its stdin
    # closes. Each `virtx-stdio` a test spawns inherits `VIRTX_HOME`, and keeps its state there.
    _fixture = subprocess.Popen(
        ["cargo", "xtask", "fixture", "--serve"],
        cwd=ROOT,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    )
    assert _fixture.stdout is not None
    for line in _fixture.stdout:
        if line.strip() == "ready":
            break
        named, _, value = line.rstrip("\n").partition("=")
        if named.startswith("VIRTX_"):
            os.environ[named] = value
    else:
        raise pytest.UsageError("`cargo xtask fixture --serve` ended before it served")


def pytest_unconfigure(config: pytest.Config) -> None:
    if _fixture is not None and _fixture.stdin is not None:
        _fixture.stdin.close()
        _fixture.wait()


def fixture(name: str) -> str:
    """What ``cargo xtask fixture`` named ``name`` in the environment."""
    value = os.environ.get(name)
    if not value:
        pytest.fail(f"{name} is not set: the fixture did not name it", pytrace=False)
    return value


@pytest.fixture(scope="session")
def server() -> str:
    """The path of ``virtx-stdio``, which ``Client.spawn`` starts for each client."""
    return fixture("VIRTX_TEST_STDIO")


@pytest.fixture
def client(server: str) -> Iterator[Client]:
    """A client of a ``virtx-stdio`` of its own."""
    with Client.spawn(server) as client:
        yield client


@pytest.fixture
def tcp_client() -> Iterator[Client]:
    """A client of the ``virtx-server-tcp`` served for the run, with its token and project."""
    auth = BearerAuth(fixture("VIRTX_TEST_TOKEN"))
    headers = {"virtx-project": fixture("VIRTX_TEST_PROJECT")}
    with Client.connect(fixture("VIRTX_TEST_URL"), auth, headers) as client:
        yield client


_names = itertools.count()


def unique(what: str) -> str:
    """A name no other test gives."""
    return f"test-{what}-{next(_names)}"


def familiar(reference: str) -> str:
    """``reference`` without the registry and namespace Docker Hub's names are filled in with,
    which a server may answer: ``docker.io/library/alpine:3.20`` is ``alpine:3.20``."""
    rest = reference.removeprefix("docker.io/")
    if rest == reference:
        return reference
    name = rest.removeprefix("library/")
    return name if "/" not in name else rest


async def tags(client: Client) -> dict[str, str]:
    """Every tag on the server's images, as the tests name it, with the digest it points at."""
    return {familiar(item.reference): item.digest for item in await client.list_image()}


async def alpine(client: Client) -> str:
    """``ALPINE`` on the server, pulled unless it is there already, and its digest. Docker Hub
    limits the pulls an address makes, so a test that only needs the image takes the one
    there."""
    digest = (await tags(client)).get(ALPINE)
    if digest is not None:
        return digest
    job = await client.pull_image(ALPINE)
    [entry async for entry in job.log()]
    digest = (await job.get()).digest
    assert digest is not None, (await job.get()).error
    return digest
