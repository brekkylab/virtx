"""Builds ``libvirtx_core`` once and opens it, unless ``$VIRTX_CORE_LIB`` names one."""

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
    name = {"win32": "virtx_core.dll", "darwin": "libvirtx_core.dylib"}.get(
        sys.platform, "libvirtx_core.so"
    )
    virtx.load(os.environ.get(virtx.LIBRARY_ENV) or TARGET / name)
