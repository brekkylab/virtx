"""Builds ``libvirtx_core`` once and opens it, unless ``$VIRTX_CORE_LIB`` names one, and
builds ``virtx-core``'s ``test_server`` example, the in-memory server the tests spawn."""

import os
import subprocess
import sys
from pathlib import Path

import pytest

import virtx

ROOT = Path(__file__).resolve().parents[2]
TARGET = ROOT / "target" / "debug"


def pytest_configure(config: pytest.Config) -> None:
    subprocess.run(["cargo", "build", "-p", "virtx-core", "--lib"], cwd=ROOT, check=True)
    subprocess.run(
        ["cargo", "build", "-p", "virtx-core", "--features", "testing", "--example", "test_server"],
        cwd=ROOT,
        check=True,
    )
    name = {"win32": "virtx_core.dll", "darwin": "libvirtx_core.dylib"}.get(
        sys.platform, "libvirtx_core.so"
    )
    virtx.load(os.environ.get(virtx.LIBRARY_ENV) or TARGET / name)


@pytest.fixture
def test_server() -> str:
    """The path of the ``test_server`` example."""
    suffix = ".exe" if sys.platform == "win32" else ""
    return str(TARGET / "examples" / f"test_server{suffix}")
