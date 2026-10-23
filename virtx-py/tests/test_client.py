import asyncio

import pytest

import virtx
from virtx import Client, Output, Progress, Recipe, VirtxError


async def test_a_call_answers_its_output(client: Client) -> None:
    assert await client.list_image() == [{"reference": "a:latest", "digest": "sha256:0", "size": 1}]


async def test_a_call_raises_the_server_s_error(client: Client) -> None:
    with pytest.raises(VirtxError, match="no such image"):
        await client.remove_image("a:latest")


async def test_a_build_yields_its_log_then_its_output(client: Client) -> None:
    events = [e async for e in client.build_image(Recipe("alpine:3.20").run("true"), "jq:latest")]
    assert events == [
        Progress("pulling alpine:3.20"),
        Output({"reference": "jq:latest", "digest": "sha256:0"}),
    ]


async def test_a_failed_build_raises_after_its_log(client: Client) -> None:
    build = client.build_image(Recipe("missing"))
    assert await anext(build) == Progress("pulling missing")
    with pytest.raises(VirtxError, match="no image missing"):
        await anext(build)


async def test_bytes_go_and_come_back_as_bytes(client: Client) -> None:
    await client.write_volume_file("v", "f", b"\0\xffhi")
    assert await client.read_volume_file("v", "f") == b"\0\xffhi"
    assert await client.read_volume_file("v", "hello", 1, 3) == b"ell"


async def test_an_exec_yields_its_output_then_how_it_ended(client: Client) -> None:
    events = [e async for e in client.exec_sandbox("s", ["echo", "hi"])]
    assert events == [Progress("hi"), Output({"code": 0})]


async def test_calls_run_at_once(client: Client) -> None:
    lists = await asyncio.gather(*(client.list_image() for _ in range(20)))
    assert all(len(images) == 1 for images in lists)


async def test_a_call_cancelled_frees_its_request(client: Client) -> None:
    task = asyncio.ensure_future(client.list_image())
    task.cancel()
    with pytest.raises(asyncio.CancelledError):
        await task
    assert len(await client.list_image()) == 1


async def test_a_client_handed_on_as_a_pointer_is_the_same_session(client: Client) -> None:
    raw = client.clone().into_raw()
    taken = Client.from_raw(raw)
    assert taken.as_raw() == client.as_raw()
    taken.close()
    assert len(await client.list_image()) == 1, "still held"


async def test_a_closed_client_refuses_calls(server: str) -> None:
    client = Client.spawn(server)
    await client.aclose()
    with pytest.raises(VirtxError, match="closed"):
        await client.list_image()


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
