"""The ``virtx`` command: the one ``libvirtx_core`` carries, run with this process's arguments."""

from __future__ import annotations

import ctypes
import os
import signal
import sys

from ._library import VirtxError, library


def main() -> None:
    try:
        dll = library()._dll
    except VirtxError as e:
        sys.exit(f"virtx: {e}")
    run = dll.virtx_cli_main
    run.argtypes = [ctypes.POINTER(ctypes.c_char_p), ctypes.c_size_t]
    run.restype = ctypes.c_int
    argv = [b"virtx", *(os.fsencode(arg) for arg in sys.argv[1:])]
    # Ctrl-C is the command's to take while it runs: it cancels the job it interrupts and exits
    # 130, where Python's own handler would raise KeyboardInterrupt once the call returns.
    signal.signal(signal.SIGINT, signal.SIG_DFL)
    sys.exit(run((ctypes.c_char_p * len(argv))(*argv), len(argv)))
