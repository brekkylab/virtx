"""Builds ``libvirtx_core`` and the test server (``virtx-rs/examples/test_server.rs``) once, and
opens the library, unless ``$VIRTX_CORE_LIB`` names one."""

import os
import subprocess
import sys
from pathlib import Path

import pytest

import virtx

ROOT = Path(__file__).resolve().parents[2]
TARGET = ROOT / "target" / "debug"


def pytest_configure(config: pytest.Config) -> None:
    subprocess.run(
        ["cargo", "build", "-p", "virtx-core", "--lib", "-p", "virtx-rs", "--example", "test_server"],
        cwd=ROOT,
        check=True,
    )
    name = {"win32": "virtx_core.dll", "darwin": "libvirtx_core.dylib"}.get(
        sys.platform, "libvirtx_core.so"
    )
    virtx.load(os.environ.get(virtx.LIBRARY_ENV) or TARGET / name)


@pytest.fixture
def server() -> str:
    return str(TARGET / "examples" / ("test_server.exe" if sys.platform == "win32" else "test_server"))


@pytest.fixture
def client(server: str):
    with virtx.Client.spawn(server) as client:
        yield client
