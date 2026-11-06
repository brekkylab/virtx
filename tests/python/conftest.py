"""Opens the ``libvirtx_core`` that ``$VIRTX_CORE_LIB`` names; ``cargo xtask e2e`` sets it,
with ``$VIRTX_UVM`` and ``$VIRTX_HOME``."""

import os

import pytest

import virtx

NEEDED = ("VIRTX_CORE_LIB", "VIRTX_UVM", "VIRTX_HOME")


def pytest_configure(config: pytest.Config) -> None:
    missing = [name for name in NEEDED if not os.environ.get(name)]
    if missing:
        pytest.exit(f"{', '.join(missing)} unset: run these with `cargo xtask e2e`", 2)
    virtx.load(os.environ[virtx.LIBRARY_ENV])
