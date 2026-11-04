"""Volumes, sandboxes and the jobs of images and sandboxes, against ``virtx-core``'s
``test_server`` example over stdio."""

import pytest

from virtx import (
    Client,
    ImageJob,
    ImageJobEnd,
    ImageJobKind,
    Log,
    LogEntry,
    Recipe,
    SandboxJobEnd,
    SandboxJobKind,
    SandboxState,
    Stderr,
    Stdout,
    Truncated,
    VirtxError,
    VolumeListOutputItem,
)


async def test_a_sandbox_stops_starts_again_and_is_found_by_its_name(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        made = await client.create_sandbox("alpine:3.20", name="work", ports=[8000])
        assert made.state == SandboxState.RUNNING and made.running
        assert made.name == "work" and not made.ephemeral
        assert made.auto_stop_minutes == 15 and made.auto_delete_days == 30
        found = await client.find_sandbox("work")
        assert found is not None and found.id == made.id
        assert await client.find_sandbox("nobody") is None
        with pytest.raises(VirtxError, match="409"):
            await client.create_sandbox("alpine:3.20", name="work")

        stopped = await client.stop_sandbox(made.id)
        assert stopped.state == SandboxState.STOPPED
        started = await client.start_sandbox(made.id)
        assert started.running and started.ports == made.ports

        kept = await client.create_sandbox("alpine:3.20", auto_delete_days="never")
        assert kept.auto_delete_days is None
        gone = await client.create_sandbox("alpine:3.20", ephemeral=True)
        await client.stop_sandbox(gone.id)
        with pytest.raises(VirtxError, match="404"):
            await client.get_sandbox(gone.id)

        job = await client.commit_image(made.id, "mine:v1")
        assert job.kind == ImageJobKind.COMMIT and job.reference == "mine:v1"
        await client.remove_sandbox(made.id)
        with pytest.raises(VirtxError, match="404"):
            await client.get_sandbox(made.id)


async def test_a_volume_is_created_and_got(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        assert await client.get_volume("v") == VolumeListOutputItem("v", 0)
        created = await client.create_volume("w")
        assert created.name == "w"
        assert await client.get_volume("w") == created
        await client.remove_volume("w")


async def test_a_sandbox_runs_a_command_whose_log_ends_with_its_code(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20", ports=[8000])
        assert sandbox.running
        assert [port.port for port in sandbox.ports] == [8000]
        got = await client.get_sandbox(sandbox.id)
        assert got.id == sandbox.id and got.ports == []
        assert sandbox.id in [item.id for item in await client.list_sandbox()]

        job = await client.exec_sandbox(sandbox.id, ["echo", "hi"])
        assert job.kind == SandboxJobKind.EXEC and job.cmd == ["echo", "hi"]
        log = [entry async for entry in client.sandbox_job_log(sandbox.id, job.id)]
        assert log == [LogEntry(1, Stdout("hi")), LogEntry(2, SandboxJobEnd(0))]
        assert (await client.get_sandbox_job(sandbox.id, job.id)).code == 0
        assert job.id in [item.id for item in await client.list_sandbox_job(sandbox.id)]

        failed = await client.exec_sandbox(sandbox.id, ["fail"])
        log = [entry.entry async for entry in client.sandbox_job_log(sandbox.id, failed.id)]
        assert log == [Stderr("failed"), SandboxJobEnd(1)]

        await client.remove_sandbox_job(sandbox.id, job.id)
        with pytest.raises(VirtxError, match="404"):
            await client.get_sandbox_job(sandbox.id, job.id)
        await client.remove_sandbox(sandbox.id)
        with pytest.raises(VirtxError, match="404"):
            await client.get_sandbox(sandbox.id)


async def test_a_log_is_truncated_at_its_limit_and_resumed_after_an_entry(
    test_server: str,
) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20")
        job = await client.exec_sandbox(sandbox.id, ["lines", "20"])
        log = [entry async for entry in client.sandbox_job_log(sandbox.id, job.id)]
        assert [entry.entry for entry in log[-2:]] == [Truncated(), SandboxJobEnd(0)]
        assert len(log) == 10
        rest = [entry async for entry in client.sandbox_job_log(sandbox.id, job.id, after=8)]
        assert rest == log[8:]


async def test_stdin_is_written_and_a_signal_ends_a_process(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20")
        cat = await client.exec_sandbox(sandbox.id, ["cat"], stdin=True)
        await client.write_sandbox_job_stdin(sandbox.id, cat.id, "a\nb\n")
        await client.write_sandbox_job_stdin(sandbox.id, cat.id, eof=True)
        log = [entry.entry async for entry in client.sandbox_job_log(sandbox.id, cat.id)]
        assert log[:2] == [Stdout("a"), Stdout("b")]
        assert isinstance(log[-1], SandboxJobEnd)

        sleep = await client.exec_sandbox(sandbox.id, ["sleep"])
        assert sleep.running
        await client.signal_sandbox_job(sandbox.id, sleep.id, "SIGTERM")
        log = [entry.entry async for entry in client.sandbox_job_log(sandbox.id, sleep.id)]
        assert isinstance(log[-1], SandboxJobEnd)
        assert not (await client.get_sandbox_job(sandbox.id, sleep.id)).running


async def test_an_image_build_is_a_job_whose_log_ends_with_its_digest(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        job = await client.build_image(Recipe("alpine:3.20").run("true"), "built:latest")
        assert isinstance(job, ImageJob)
        assert (job.kind, job.reference) == (ImageJobKind.BUILD, "built:latest")
        log = [entry.entry async for entry in client.image_job_log(job.id)]
        assert log == [Log("pulling alpine:3.20"), ImageJobEnd(digest="sha256:0")]
        assert (await client.get_image_job(job.id)).digest == "sha256:0"
        assert job.id in [item.id for item in await client.list_image_job()]
        await client.remove_image_job(job.id)
        with pytest.raises(VirtxError, match="404"):
            await client.get_image_job(job.id)


async def test_a_failed_pull_ends_its_log_with_its_error(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        job = await client.pull_image("missing")
        assert job.kind == ImageJobKind.PULL
        log = [entry.entry async for entry in client.image_job_log(job.id)]
        assert log[0] == Log("pulling missing")
        end = log[-1]
        assert isinstance(end, ImageJobEnd) and end.digest is None and end.error
        assert (await client.get_image_job(job.id)).error == end.error


async def test_a_build_iterated_yields_its_log_to_its_end(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        log = [entry async for entry in client.build_image(Recipe("alpine:3.20"), "it:latest")]
        assert log == [
            LogEntry(1, Log("pulling alpine:3.20")),
            LogEntry(2, ImageJobEnd("sha256:0")),
        ]


async def test_an_exec_iterated_yields_its_output_and_how_it_ended(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20")
        log = [entry async for entry in client.exec_sandbox(sandbox.id, ["echo", "hi"])]
        assert log == [LogEntry(1, Stdout("hi")), LogEntry(2, SandboxJobEnd(0))]


async def test_an_exec_iterated_then_awaited_answers_the_job_it_read(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20")
        echo = client.exec_sandbox(sandbox.id, ["echo", "hi"])
        log = [entry async for entry in echo]
        assert log[-1] == LogEntry(2, SandboxJobEnd(0))
        job = await echo
        assert job.cmd == ["echo", "hi"]
        assert [j.id for j in await client.list_sandbox_job(sandbox.id)] == [job.id]


async def test_a_started_job_awaited_then_iterated_starts_it_once(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        before = len(await client.list_image_job())
        build = client.build_image(Recipe("alpine:3.20"), "once:latest")
        assert len(await client.list_image_job()) == before
        job = await build
        assert await build == job
        log = [entry.entry async for entry in build]
        assert log[-1] == ImageJobEnd(digest="sha256:0")
        assert (await client.get_image_job(job.id)).digest == "sha256:0"
        assert len(await client.list_image_job()) == before + 1
