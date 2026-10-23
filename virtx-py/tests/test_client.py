import pytest

import virtx
from virtx import Client, VirtxError


def test_spawning_nothing_raises_a_message() -> None:
    with pytest.raises(VirtxError, match="starting the server"):
        Client.spawn("/nonexistent/virtx-server")


def test_a_second_library_is_refused() -> None:
    with pytest.raises(VirtxError, match="is open already"):
        virtx.load("/nonexistent/libvirtx_core")
    assert virtx.library_path() is not None


def test_an_image_is_named_by_its_spelling() -> None:
    digest = "sha256:" + "0" * 64
    assert virtx.image_name(digest) == {"type": "digest", "digest": digest}
    assert virtx.image_name("sha256:0") == {"type": "reference", "reference": "sha256:0"}
