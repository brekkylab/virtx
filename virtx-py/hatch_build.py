"""Puts ``libvirtx_core`` in the package, beside its Python files, as a wheel or an editable install
is built: the one ``$VIRTX_CORE_LIB`` names, as the release's wheels take theirs, else one cargo
builds from the crate beside this directory, in the ``dev`` profile for an editable install and
``release`` for a wheel. An editable install opens it from ``src/virtx/``, so it is copied there;
a wheel carries it, and is tagged for this platform."""

import json
import os
import shutil
import subprocess
import sysconfig
from pathlib import Path
from typing import Any

from hatchling.builders.hooks.plugin.interface import BuildHookInterface

NAMES = ("libvirtx_core.so", "libvirtx_core.dylib", "virtx_core.dll")
"""The library's file name on each platform."""


class LibraryHook(BuildHookInterface):
    PLUGIN_NAME = "custom"

    def initialize(self, version: str, build_data: dict[str, Any]) -> None:
        if self.target_name != "wheel":
            return
        editable = version == "editable"
        given = os.environ.get("VIRTX_CORE_LIB")
        library = Path(given) if given else build(Path(self.root).parent, not editable)
        if editable:
            package = Path(self.root) / "src" / "virtx"
            for name in NAMES:
                (package / name).unlink(missing_ok=True)
            shutil.copy2(library, package / library.name)
            return
        build_data["force_include"][str(library)] = f"virtx/{library.name}"
        build_data["pure_python"] = False
        platform = sysconfig.get_platform().replace("-", "_").replace(".", "_")
        build_data["tag"] = f"py3-none-{platform}"


def build(repository: Path, release: bool) -> Path:
    """``libvirtx_core`` built by cargo from ``repository``'s ``virtx-core``, and where it is."""
    manifest = repository / "virtx-core" / "Cargo.toml"
    if not manifest.is_file():
        raise RuntimeError(f"no {manifest} to build libvirtx_core from, and no $VIRTX_CORE_LIB")
    cargo = ["cargo", "rustc", "--manifest-path", str(manifest), "--lib", "--crate-type", "cdylib"]
    cargo += ["--message-format=json-render-diagnostics"] + (["--release"] if release else [])
    out = subprocess.run(cargo, stdout=subprocess.PIPE, text=True, check=True).stdout
    for line in out.splitlines():
        message = json.loads(line)
        if message.get("reason") != "compiler-artifact":
            continue
        for file in message["filenames"]:
            if Path(file).name in NAMES:
                return Path(file)
    raise RuntimeError("cargo built no libvirtx_core")
