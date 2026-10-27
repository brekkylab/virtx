import socket

import pytest

import virtx
from virtx import Client, PortForwarder, VirtxError


def test_spawning_nothing_raises_a_message() -> None:
    with pytest.raises(VirtxError, match="starting the server"):
        Client.spawn("/nonexistent/virtx-server")


def test_a_second_library_is_refused() -> None:
    with pytest.raises(VirtxError, match="is open already"):
        virtx.load("/nonexistent/libvirtx_core")
    assert virtx.library_path() is not None


def test_an_image_is_named_by_its_spelling() -> None:
    digest = "sha256:" + "0" * 64
    assert virtx.image_name(digest) == virtx.ImageDigest(digest)
    assert virtx.image_name("sha256:0") == virtx.ImageTag("sha256:0")


def test_a_mount_source_keeps_its_credentials_out_of_its_repr() -> None:
    source = virtx.GdriveSource("id", "secret", "token")
    assert source.to_dict() == {
        "type": "gdrive",
        "client_id": "id",
        "client_secret": "secret",
        "refresh_token": "token",
    }
    assert "secret" not in repr(source) and "token" not in repr(source)
    assert "client_secret" not in virtx.OnedriveSource("id", "token").to_dict()


def test_a_streamed_event_reads_back() -> None:
    from virtx.client import _event

    progress = {"progress": {"progress": 1, "content": "fetch alpine:3.20"}}
    assert _event(progress, dict) == virtx.Progress(1, None, "fetch alpine:3.20")
    assert _event({"progress": {"progress": 2, "total": 3}}, dict) == virtx.Progress(2, 3)
    assert _event({"output": {"code": 0}}, dict) == virtx.Output({"code": 0})


def test_a_forwarder_is_on_the_loopback_until_it_is_closed() -> None:
    # Nothing listens there: each connection to the forwarder is closed.
    with (
        Client.connect("http://127.0.0.1:9/") as client,
        PortForwarder.open(client, "ports/abc") as forwarder,
    ):
        assert forwarder.host == "127.0.0.1"
        with socket.create_connection((forwarder.host, forwarder.port)) as connection:
            assert connection.recv(1) == b""
    with pytest.raises(ConnectionRefusedError):
        socket.create_connection((forwarder.host, forwarder.port))
