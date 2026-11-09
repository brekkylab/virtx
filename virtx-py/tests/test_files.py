"""The files of a volume, against ``virtx-core``'s ``test_server`` example over stdio."""

import pytest

from virtx import (
    Client,
    DirEntry,
    GdriveSource,
    Metadata,
    MountType,
    VirtxError,
)


async def test_the_files_of_a_volume_are_written_listed_moved_and_read_back(
    test_server: str,
) -> None:
    with Client.spawn(test_server) as client:
        volume = await client.get_volume("v")
        await volume.create_dir("dir")
        await volume.write_file("dir/a.txt", b"hello world")
        await volume.create_dir_all("deep/er/dir")
        await volume.create_dir_all("deep/er/dir")
        assert (await volume.stat_file("deep/er/dir")).is_dir

        metadata = await volume.stat_file("dir/a.txt")
        assert isinstance(metadata, Metadata)
        assert (metadata.is_dir, metadata.size) == (False, 11)
        entries = await volume.read_dir("dir")
        assert entries == [DirEntry("a.txt", metadata)]

        assert await volume.read_file("dir/a.txt") == b"hello world"
        assert await volume.read_file("dir/a.txt", offset=6, length=3) == b"wor"

        # Text, an etag to write over, and what is there, told without an error.
        await volume.write_file("dir/t.txt", "텍스트")
        assert await volume.read_text("dir/t.txt") == "텍스트"
        etag = (await volume.stat_file("dir/t.txt")).etag
        assert etag
        await volume.write_file("dir/t.txt", "text 2", if_match=etag)
        with pytest.raises(VirtxError) as refused:
            await volume.write_file("dir/t.txt", "text 3", if_match=etag)
        assert refused.value.status == 412
        await volume.write_file("dir/bin", b"\xff\xfe")
        with pytest.raises(VirtxError, match="UTF-8"):
            await volume.read_text("dir/bin")
        assert await volume.exists("dir/t.txt")
        assert await volume.is_file("dir/t.txt")
        assert not await volume.is_dir("dir/t.txt")
        assert await volume.is_dir("dir")
        await volume.remove_file("dir/t.txt")
        await volume.remove_file("dir/bin")
        assert not await volume.exists("dir/t.txt")
        with pytest.raises(VirtxError) as refused:
            await volume.remove_file("dir/t.txt")
        assert refused.value.status == 404
        await volume.remove_file("dir/t.txt", missing_ok=True)

        await volume.rename_file("dir/a.txt", "b.txt")
        await volume.copy_file("b.txt", "c.txt")
        with pytest.raises(VirtxError) as refused:
            await volume.copy_file("b.txt", "c.txt")
        assert refused.value.status == 412
        await volume.remove_file("dir")
        with pytest.raises(VirtxError) as refused:
            await volume.stat_file("dir")
        assert refused.value.status == 404
        assert await (await client.get_volume("v")).read_file("c.txt") == b"hello world"

        with pytest.raises(VirtxError) as refused:
            await (await client.get_snapshot("locked")).read_dir("")
        assert refused.value.status == 423


async def test_a_volume_carries_its_client_to_its_files_and_mounts(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        volume = await client.create_volume("w")
        assert (volume.name, volume.size) == ("w", 0)
        await volume.create_dir_all("in/dir")
        await volume.write_file("in/a.txt", "hello")
        await volume.rename_file("in/a.txt", "in/b.txt")
        await volume.copy_file("in/b.txt", "c.txt")
        assert await volume.read_text("c.txt") == "hello"
        assert await volume.is_dir("in/dir")
        assert sorted(entry.name for entry in await volume.read_dir("in")) == ["b.txt", "dir"]

        got = await client.get_volume("w")
        assert got == volume and got.client is client
        assert await got.read_text("in/b.txt") == "hello"
        assert volume in await client.list_volume()

        mount = await volume.mount("drive", GdriveSource("id", "secret", "token"))
        assert (mount.path, mount.mount_type) == ("drive", MountType.GDRIVE)
        assert await got.mounts() == [mount]
        await volume.unmount("drive")
        assert await got.mounts() == []

        await volume.remove()
        with pytest.raises(VirtxError) as refused:
            await client.get_volume("w")
        assert refused.value.status == 404


async def test_a_snapshot_carries_its_client_to_its_files(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        snapshot = await client.create_snapshot("t")
        await snapshot.write_file("a.txt", "kept")
        assert await (await client.get_snapshot("t")).read_text("a.txt") == "kept"
        await snapshot.remove()
        with pytest.raises(VirtxError) as refused:
            await client.get_snapshot("t")
        assert refused.value.status == 404

        locked = await client.get_snapshot("locked")
        assert locked.in_use
        with pytest.raises(VirtxError) as refused:
            await locked.read_dir("")
        assert refused.value.status == 423


async def test_a_sandbox_s_files_are_its_own_and_go_with_it(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20")
        await sandbox.create_dir_all("work/dir")
        await sandbox.write_file("work/dir/a.txt", "hello")
        assert await sandbox.read_text("work/dir/a.txt") == "hello"
        assert await sandbox.read_file("work/dir/a.txt", 1, 3) == b"ell"
        got = await client.get_sandbox(sandbox.id)
        assert [entry.name for entry in await got.read_dir("work/dir")] == ["a.txt"]
        await sandbox.remove_file("work/dir/a.txt")
        assert not await got.exists("work/dir/a.txt")
        await sandbox.remove()
        with pytest.raises(VirtxError) as refused:
            await client.get_sandbox(sandbox.id)
        assert refused.value.status == 404
        with pytest.raises(VirtxError) as refused:
            await sandbox.read_file("")
        assert refused.value.status == 404


def test_metadata_reads_as_the_wire_has_it() -> None:
    assert Metadata.from_dict({"is_dir": True, "size": 0}) == Metadata(True, 0)
    entry = DirEntry.from_dict(
        {"name": "a", "metadata": {"is_dir": False, "size": 1, "modified": 2}}
    )
    assert entry == DirEntry("a", Metadata(False, 1, 2))


async def test_a_webdav_non_json_refusal_raises_without_quoting_html() -> None:
    import http.server
    import threading

    class _Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            if self.path == "/volumes/v":
                body = b'{"name": "v", "size": 0}'
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return

            body = (
                b"<html><head><title>503 Service Unavailable</title></head><body>503</body></html>"
            )
            self.send_response(503)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_PUT(self) -> None:
            body = (
                b"<html><head><title>503 Service Unavailable</title></head><body>503</body></html>"
            )
            self.send_response(503)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, format: str, *args: object) -> None:
            pass

    server = http.server.HTTPServer(("127.0.0.1", 0), _Handler)
    host, port = server.server_address
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with Client.connect(f"http://{host}:{port}/") as client:
            volume = await client.get_volume("v")
            with pytest.raises(VirtxError) as exc_info:
                await volume.read_file("a.txt")
            err = exc_info.value
            assert err.status == 503
            assert (
                err.body
                == "<html><head><title>503 Service Unavailable</title></head><body>503</body></html>"
            )
            err_msg = str(err)
            assert (
                "the server answered 503 Service Unavailable; not a virtx answer (text/html,"
                in err_msg
            )
            assert "the service is unreachable or restarting" in err_msg
            assert "<html>" not in err_msg

            with pytest.raises(VirtxError) as exc_info:
                await volume.write_file("a.txt", "content")
            err = exc_info.value
            assert err.status == 503
            assert (
                err.body
                == "<html><head><title>503 Service Unavailable</title></head><body>503</body></html>"
            )
            assert "<html>" not in str(err)
    finally:
        server.shutdown()
        server.server_close()


async def test_a_webdav_plain_text_refusal_keeps_its_message() -> None:
    import http.server
    import threading

    class _Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:
            if self.path == "/volumes/v":
                body = b'{"name": "v", "size": 0}'
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
                return

            body = b"no volume here is named v"
            self.send_response(404)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, format: str, *args: object) -> None:
            pass

    server = http.server.HTTPServer(("127.0.0.1", 0), _Handler)
    host, port = server.server_address
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with Client.connect(f"http://{host}:{port}/") as client:
            volume = await client.get_volume("v")
            with pytest.raises(VirtxError) as exc_info:
                await volume.read_file("a.txt")
            err = exc_info.value
            assert err.status == 404
            assert err.body is None
            assert str(err) == "the server answered 404 Not Found: no volume here is named v"
    finally:
        server.shutdown()
        server.server_close()
