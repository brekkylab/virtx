"""Snapshots, volumes, sandboxes and the jobs of images and sandboxes, against ``virtx-core``'s
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
    SandboxJob,
    SandboxJobEnd,
    SandboxJobKind,
    Snapshot,
    Stderr,
    Stdout,
    Truncated,
    VirtxError,
    Volume,
)


async def test_a_snapshot_is_created_got_listed_and_removed(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        created = await client.create_snapshot("new", disk_gib=4)
        assert created == Snapshot(client, "new", 0, False)
        assert await client.get_snapshot("new") == created
        assert created in await client.list_snapshot()
        assert (await client.get_snapshot("locked")).in_use
        await created.remove()
        with pytest.raises(VirtxError) as refused:
            await client.get_snapshot("new")
        assert refused.value.status == 404


async def test_a_volume_is_created_and_got(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        assert await client.get_volume("v") == Volume(client, "v", 0)
        created = await client.create_volume("w")
        assert created.name == "w"
        assert await client.get_volume("w") == created
        await client.remove_volume("w")
        with pytest.raises(VirtxError) as refused:
            await client.get_volume("w")
        assert refused.value.status == 404


async def test_a_sandbox_runs_a_command_whose_log_ends_with_its_code(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20", snapshot="s", ports=[8000])
        assert sandbox.snapshot == "s" and sandbox.running
        assert [port.port for port in sandbox.ports] == [8000]
        got = await client.get_sandbox(sandbox.id)
        assert got.id == sandbox.id and got.ports == []
        assert sandbox.id in [item.id for item in await client.list_sandbox()]

        job = await sandbox.exec(["echo", "hi"])
        assert isinstance(job, SandboxJob) and job.sandbox_id == sandbox.id
        assert job.kind == SandboxJobKind.EXEC and job.cmd == ["echo", "hi"]
        log = [entry async for entry in job.log()]
        assert log == [LogEntry(1, Stdout("hi")), LogEntry(2, SandboxJobEnd(0))]
        assert (await job.get()).code == 0
        assert (await client.get_sandbox_job(sandbox.id, job.id)).code == 0
        assert job.id in [item.id for item in await sandbox.jobs()]
        assert job.id in [item.id for item in await client.list_sandbox_job(sandbox.id)]

        failed = await client.exec_sandbox(sandbox.id, ["fail"])
        log = [entry.entry async for entry in client.sandbox_job_log(sandbox.id, failed.id)]
        assert log == [Stderr("failed"), SandboxJobEnd(1)]

        await (await sandbox.job(job.id)).remove()
        with pytest.raises(VirtxError) as refused:
            await client.get_sandbox_job(sandbox.id, job.id)
        assert refused.value.status == 404
        await client.remove_sandbox(sandbox.id)
        with pytest.raises(VirtxError) as refused:
            await client.get_sandbox(sandbox.id)
        assert refused.value.status == 404


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
        cat = await sandbox.exec(["cat"], stdin=True)
        await cat.write_stdin("a\nb\n")
        await cat.write_stdin(eof=True)
        log = [entry.entry async for entry in cat.log()]
        assert log[:2] == [Stdout("a"), Stdout("b")]
        assert isinstance(log[-1], SandboxJobEnd)

        sleep = await sandbox.exec(["sleep"])
        assert sleep.running
        await sleep.signal("SIGTERM")
        log = [entry.entry async for entry in sleep.log()]
        assert isinstance(log[-1], SandboxJobEnd)
        assert not (await sleep.get()).running

        # The client's requests by id reach the same jobs.
        cat = await client.exec_sandbox(sandbox.id, ["cat"], stdin=True)
        await client.write_sandbox_job_stdin(sandbox.id, cat.id, "c\n", eof=True)
        log = [entry.entry async for entry in client.sandbox_job_log(sandbox.id, cat.id)]
        assert log[0] == Stdout("c")


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
        with pytest.raises(VirtxError) as refused:
            await client.get_image_job(job.id)
        assert refused.value.status == 404


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
