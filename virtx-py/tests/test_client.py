import pytest

from conftest import fixture, unique

import virtx
from virtx import Client, VirtxError


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


async def test_a_client_over_http_sends_its_token_and_project(tcp_client: Client) -> None:
    volume = await tcp_client.create_volume(unique("over-http"))
    assert volume in await tcp_client.list_volume()
    await volume.remove()
    project = {"virtx-project": fixture("VIRTX_TEST_PROJECT")}
    with Client.connect(fixture("VIRTX_TEST_URL"), None, project) as anonymous:
        with pytest.raises(VirtxError) as refused:
            await anonymous.list_volume()
        assert refused.value.status == 401


def test_virtx_error_keeps_body() -> None:
    err = VirtxError("something failed")
    assert str(err) == "something failed"
    assert err.status is None
    assert err.body is None

    err_with_status_and_body = VirtxError(
        "service unavailable", status=503, body="<html>error</html>"
    )
    assert str(err_with_status_and_body) == "service unavailable"
    assert err_with_status_and_body.status == 503
    assert err_with_status_and_body.body == "<html>error</html>"


async def test_a_non_json_refusal_raises_without_quoting_html() -> None:
    import http.server
    import threading

    class _503Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self) -> None:
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

    server = http.server.HTTPServer(("127.0.0.1", 0), _503Handler)
    host, port = server.server_address
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with Client.connect(f"http://{host}:{port}/") as client:
            with pytest.raises(VirtxError) as exc_info:
                await client.list_sandbox()
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
    finally:
        server.shutdown()
        server.server_close()
