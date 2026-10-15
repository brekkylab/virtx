import asyncio
import gc
import os
import shutil
import struct
import time
import uuid

import pytest

import virtx
from virtx import (
    ConsoleClient,
    ConsoleRefused,
    VirtxError,
    Directory,
    ErrorCode,
    ImageClient,
    ImageSource,
    Recipe,
    Step,
)


def test_image_is_a_value():
    base = Recipe("python:3.12-slim")
    extended = base.step("pip install duckdb").step(Step.env("TZ", "UTC"))

    assert "python:3.12-slim" in repr(extended)
    assert "duckdb" in repr(extended)
    assert "duckdb" not in repr(base)


def test_image_from_dockerfile():
    image = Recipe.from_dockerfile("FROM alpine:3.20\nRUN apk add jq\n")
    assert "alpine:3.20" in repr(image)


def test_image_source():
    recipe = Recipe("alpine:3.20").step("apk add jq")
    assert ImageSource.recipe(recipe) == ImageSource.recipe(Recipe("alpine:3.20", ["apk add jq"]))
    assert ImageSource.reference("myimg:latest") != ImageSource.digest("myimg:latest")
    assert repr(ImageSource.digest("sha256:0123")) == 'ImageSource.digest("sha256:0123")'
    assert recipe.base == "alpine:3.20"


def test_a_port_is_spelled_the_way_docker_spells_it():
    builder = ConsoleClient.builder().network(True).ports(["8080:80", "5901:5900"])
    with pytest.raises(ValueError):
        builder.ports(["5900"])
    with pytest.raises(ValueError):
        builder.ports(["8080:0"])
    with pytest.raises(ValueError):
        builder.ports(["http"])


def test_error_codes_are_virtx_s():
    assert ErrorCode.TIMED_OUT == -32000
    assert ErrorCode.INTERNAL_ERROR == -32603
    assert ErrorCode.UNKNOWN_JOB == -32015
    assert ErrorCode.TOO_MANY_JOBS == -32016


def test_jobs_are_exported():
    for name in ("Job", "Chunk", "ExecStream"):
        assert name in virtx.__all__
        assert getattr(virtx, name).__module__ == "virtx"


def test_directory_refuses_a_file_under_a_mount(tmp_path):
    directory = Directory().with_mount("project", tmp_path)
    with pytest.raises(OSError):
        directory.add_file("project/notes.md", "under a mount")


@pytest.mark.skipif(not hasattr(virtx, "HostMount"), reason="built without `mount`")
def test_host_mount_serves_the_directory(tmp_path):
    mountpoint = tmp_path / "mnt"
    mountpoint.mkdir()
    directory = Directory().with_file("notes/today.md", b"ship the release")

    mount = virtx.HostMount(directory, mountpoint)
    try:
        assert (mountpoint / "notes" / "today.md").read_bytes() == b"ship the release"
        # The mount owns the tree now.
        with pytest.raises(ValueError):
            directory.add_file("more.md", "")
    finally:
        del mount


@pytest.mark.skipif(not hasattr(virtx, "HostMount"), reason="built without `mount`")
async def test_unmount_takes_a_host_mount_down_now(tmp_path):
    mountpoint = tmp_path / "mnt"
    mountpoint.mkdir()
    mount = virtx.HostMount(Directory().with_file("notes.md", b"hi"), mountpoint)
    assert (mountpoint / "notes.md").read_bytes() == b"hi"

    await mount.unmount()
    # Down while the object lives on, and the mount point is the empty directory it was.
    assert mountpoint.is_dir()
    assert list(mountpoint.iterdir()) == []
    assert os.fspath(mount.mountpoint) == str(mountpoint)
    # A second call has nothing to take down.
    await mount.unmount()


async def test_building_without_a_server_fails_and_spends_the_builder(tmp_path, monkeypatch):
    # Point the default server lookup (virtx's cache) at an empty directory.
    monkeypatch.setenv("VIRTX_HOME", str(tmp_path))
    builder = ConsoleClient.builder()
    # `VirtxError` itself: not a refusal from a server found after all.
    with pytest.raises(VirtxError) as raised:
        await builder.build()
    assert raised.type is VirtxError
    with pytest.raises(ValueError):
        builder.vcpus(2)


async def test_building_against_a_missing_binary_fails():
    with pytest.raises(VirtxError):
        await ConsoleClient.builder().cmd(["virtx-no-such-console-server"]).build()


async def test_image_client_against_a_missing_binary_fails():
    with pytest.raises(VirtxError):
        await ImageClient.try_from_cmd(["virtx-no-such-console-server"])


# Against a real console server, named by `$VIRTX_CONSOLE` (`virtx-uvm`, say).
SERVER = os.environ.get("VIRTX_CONSOLE")


@pytest.mark.skipif(not SERVER or not shutil.which(SERVER), reason="set $VIRTX_CONSOLE")
async def test_exec_read_write(tmp_path):
    builder = (
        ConsoleClient.builder()
        .cmd([SERVER])
        .image(Recipe("python:3.12-slim-trixie"))
        .mount(tmp_path, "/work")
        .network(False)
    )
    async with await builder.build() as console:
        assert console.mounts == ["/work"]

        assert await console.write("/work/hello.txt", "hi") == 2
        result = await console.exec(["cat", "/work/hello.txt"])
        assert result.code == 0
        assert result.stdout == b"hi"

        read = await console.read("/work/hello.txt")
        assert read.data == b"hi"
        assert read.size == 2


@pytest.mark.skipif(not SERVER or not shutil.which(SERVER), reason="set $VIRTX_CONSOLE")
async def test_build_list_remove():
    async with await ImageClient.try_from_cmd([SERVER]) as images:
        assert await images.version()

        built = await images.build(Recipe("alpine:3.20"), "virtx-py-test:latest")
        assert built.reference == "virtx-py-test:latest"
        assert any(entry.digest == built.digest for entry in await images.list())

        await images.remove(ImageSource.reference(built.reference))
        assert all(
            built.reference not in entry.refs for entry in await images.list()
        )


def strings_doc(**members):
    """A BSON document of string members: enough for a notification frame."""
    body = b"".join(
        b"\x02" + k.encode() + b"\x00" + struct.pack("<i", len(v.encode()) + 1) + v.encode() + b"\x00"
        for k, v in members.items()
    )
    return struct.pack("<i", len(body) + 5) + body + b"\x00"


async def relay(console, frame):
    """`_relay`'s answer, as the `(frame, broken)` its callback was called with."""
    loop = asyncio.get_running_loop()
    done = loop.create_future()
    console._relay(frame, lambda *answer: loop.call_soon_threadsafe(done.set_result, answer))
    return await done


@pytest.mark.skipif(not SERVER or not shutil.which(SERVER), reason="set $VIRTX_CONSOLE")
async def test_relay_answers_through_its_callback():
    console = await ConsoleClient.builder().cmd([SERVER]).image(Recipe("alpine:3.20")).build()

    # `quit` is the owner's to say: relayed, it is dropped, and the session goes on.
    assert await relay(console, strings_doc(jsonrpc="2.0", method="quit")) == (b"", None)
    assert (await console.exec(["true"])).code == 0

    frame, broken = await relay(console, b"not a message")
    assert frame is None and "relayed message" in broken

    await console.close()
    assert await relay(console, b"") == (None, "this console has been closed")


async def job_console():
    """A booted console for the job tests."""
    console = await ConsoleClient.builder().cmd([SERVER]).image(Recipe("alpine:3.20")).build()
    # Booted first, so the boot is not counted against what a test times.
    await console.exec(["true"])
    return console


@pytest.mark.skipif(not SERVER or not shutil.which(SERVER), reason="set $VIRTX_CONSOLE")
async def test_a_command_printing_once_a_second_is_seen_once_a_second():
    async with await job_console() as console:
        ticks, codes = [], []
        stream = console.exec_stream(["sh", "-c", "for i in 1 2 3; do echo tick $i; sleep 1; done"])
        async for chunk in stream:
            now = time.monotonic()
            ticks += [(line, now) for line in chunk.stdout.decode().splitlines()]
            codes.append(chunk.code)

        assert [line for line, _ in ticks] == ["tick 1", "tick 2", "tick 3"]
        for (_, earlier), (_, later) in zip(ticks, ticks[1:]):
            assert later - earlier > 0.5
        # Only the last chunk carries the code.
        assert codes[-1] == 0 and all(code is None for code in codes[:-1])


@pytest.mark.skipif(not SERVER or not shutil.which(SERVER), reason="set $VIRTX_CONSOLE")
async def test_a_timeout_keeps_the_output_up_to_the_kill():
    cmd = ["sh", "-c", "echo started; echo err >&2; sleep 5"]
    async with await job_console() as console:
        with pytest.raises(ConsoleRefused) as raised:
            await console.exec(cmd, timeout_ms=1000)
        assert raised.value.code == ErrorCode.TIMED_OUT
        assert raised.value.stdout == b"started\n"
        assert raised.value.stderr == b"err\n"
        assert raised.value.truncated is False

        # A stream ends in the same refusal, with what the chunks before it did not carry.
        stdout, stderr = b"", b""
        with pytest.raises(ConsoleRefused) as raised:
            async for chunk in console.exec_stream(cmd, timeout_ms=1000):
                stdout, stderr = stdout + chunk.stdout, stderr + chunk.stderr
        assert raised.value.code == ErrorCode.TIMED_OUT
        assert stdout + raised.value.stdout == b"started\n"
        assert stderr + raised.value.stderr == b"err\n"


@pytest.mark.skipif(not SERVER or not shutil.which(SERVER), reason="set $VIRTX_CONSOLE")
async def test_a_console_answers_while_a_job_runs():
    log = f"/tmp/virtx-{uuid.uuid4().hex}.log"
    async with await job_console() as console:
        job = await console.spawn(["sh", "-c", f"echo partial | tee {log}; sleep 3; echo done"])
        started = time.monotonic()
        first = await job.poll(wait_ms=2000)
        assert first.stdout == b"partial\n" and first.code is None

        # The log the job is writing, read while it runs.
        assert (await console.read(log)).data == b"partial\n"
        assert (await console.exec(["echo", "meanwhile"])).stdout == b"meanwhile\n"
        assert time.monotonic() - started < 3

        stdout = b""
        while (chunk := await job.poll(wait_ms=1000)).code is None:
            stdout += chunk.stdout
        assert stdout + chunk.stdout == b"done\n"
        assert chunk.code == 0

        # Its end was reported, so the server holds it no longer.
        with pytest.raises(ConsoleRefused) as raised:
            await job.poll()
        assert raised.value.code == ErrorCode.UNKNOWN_JOB


@pytest.mark.skipif(not SERVER or not shutil.which(SERVER), reason="set $VIRTX_CONSOLE")
async def test_leaving_a_stream_early_kills_its_command():
    async def size(path):
        return (await console.read(path)).size

    async def beating(path):
        """A stream of a command that appends to `path` five times a second, one chunk in."""
        stream = console.exec_stream(
            ["sh", "-c", f"while :; do echo beat; echo x >> {path}; sleep 0.2; done"]
        )
        async for chunk in stream:
            if b"beat" in chunk.stdout:
                break
        return stream

    async with await job_console() as console:
        closed = f"/tmp/virtx-{uuid.uuid4().hex}"
        await (await beating(closed)).aclose()
        before = await size(closed)
        await asyncio.sleep(1)
        assert await size(closed) == before

        # Dropped rather than closed: the kill goes out on its own.
        collected = f"/tmp/virtx-{uuid.uuid4().hex}"
        stream = await beating(collected)
        del stream
        gc.collect()
        await asyncio.sleep(0.5)
        before = await size(collected)
        await asyncio.sleep(1)
        assert await size(collected) == before


@pytest.mark.skipif(not SERVER or not shutil.which(SERVER), reason="set $VIRTX_CONSOLE")
async def test_a_job_in_async_with_is_discarded_when_the_block_is_left():
    async with await job_console() as console:
        async with await console.spawn(["sleep", "60"]) as job:
            assert (await job.poll()).code is None
        with pytest.raises(ConsoleRefused) as gone:
            await job.poll()
        assert gone.value.code == ErrorCode.UNKNOWN_JOB


@pytest.mark.skipif(not SERVER or not shutil.which(SERVER), reason="set $VIRTX_CONSOLE")
async def test_a_job_let_go_of_is_reached_again_by_its_id():
    async with await job_console() as console:
        id = (await console.spawn(["sh", "-c", "echo hi; sleep 60"])).id
        gc.collect()

        job = console.job(id)
        assert job.id == id
        assert (await job.poll(wait_ms=5000)).stdout == b"hi\n"
        await job.discard()
        with pytest.raises(ConsoleRefused) as gone:
            await console.job(id).poll()
        assert gone.value.code == ErrorCode.UNKNOWN_JOB

        # A job already gone leaves nothing to do.
        await job.discard()
