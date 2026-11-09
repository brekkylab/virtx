"""Images, snapshots, volumes and their mounts, sandboxes, and the jobs of images and sandboxes,
against ``virtx-core``'s ``test_server`` example over stdio."""

from pathlib import Path

import pytest

from virtx import (
    BearerAuth,
    Client,
    ImageDownloadOutput,
    ImageJob,
    ImageJobEnd,
    ImageJobKind,
    ImageUploadOutput,
    Log,
    LogEntry,
    NotionSource,
    Output,
    Recipe,
    SandboxJob,
    SandboxJobEnd,
    SandboxJobKind,
    SandboxVolume,
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
        await created.remove()
        with pytest.raises(VirtxError) as refused:
            await client.get_volume("w")
        assert refused.value.status == 404


async def test_a_sandbox_runs_a_command_whose_log_ends_with_its_code(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20", snapshot="s", ports=[8000])
        assert sandbox.snapshot == "s" and sandbox.running
        made = await client.create_snapshot("handed")
        on_handle = await client.create_sandbox(
            "alpine:3.20",
            snapshot=made,
            volumes=[SandboxVolume(await client.get_volume("v"), "/data", readonly=True)],
        )
        assert on_handle.snapshot == "handed"
        assert SandboxVolume(Volume(client, "v", 0), "/data").to_dict() == {
            "volume": "v",
            "path": "/data",
            "readonly": False,
        }
        await on_handle.remove()
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
        assert (await got.job(job.id)).code == 0
        assert job.id in [item.id for item in await got.jobs()]

        failed = await sandbox.exec(["fail"])
        log = [entry.entry async for entry in failed.log()]
        assert log == [Stderr("failed"), SandboxJobEnd(1)]

        await job.remove()
        with pytest.raises(VirtxError) as refused:
            await got.job(job.id)
        assert refused.value.status == 404
        await sandbox.remove()
        with pytest.raises(VirtxError) as refused:
            await client.get_sandbox(sandbox.id)
        assert refused.value.status == 404


async def test_a_log_is_truncated_at_its_limit_and_resumed_after_an_entry(
    test_server: str,
) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20")
        job = await sandbox.exec(["lines", "20"])
        log = [entry async for entry in job.log()]
        assert [entry.entry for entry in log[-2:]] == [Truncated(), SandboxJobEnd(0)]
        assert len(log) == 10
        rest = [entry async for entry in job.log(after=8)]
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
        got = await client.get_sandbox(sandbox.id)
        assert not (await got.job(sleep.id)).running


async def test_an_image_build_is_a_job_whose_log_ends_with_its_digest(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        job = await client.build_image(Recipe("alpine:3.20").run("true"), "built:latest")
        assert isinstance(job, ImageJob)
        assert (job.kind, job.reference) == (ImageJobKind.BUILD, "built:latest")
        assert job.digest is not None
        log = [entry.entry async for entry in job.log()]
        assert log == [Log("pulling alpine:3.20"), ImageJobEnd(digest=job.digest)]
        assert [entry.entry async for entry in job.log(after=1)] == log[1:]
        assert (await job.get()).digest == job.digest
        assert (await client.get_image_job(job.id)).digest == job.digest
        assert job.id in [item.id for item in await client.list_image_job()]
        await job.remove()
        with pytest.raises(VirtxError) as refused:
            await job.get()
        assert refused.value.status == 404


async def test_a_failed_pull_ends_its_log_with_its_error(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        job = await client.pull_image("missing")
        assert job.kind == ImageJobKind.PULL
        log = [entry.entry async for entry in job.log()]
        assert log[0] == Log("pulling missing")
        end = log[-1]
        assert isinstance(end, ImageJobEnd) and end.digest is None and end.error
        assert (await job.get()).error == end.error


async def test_a_build_iterated_yields_its_log_to_its_end(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        log = [entry async for entry in client.build_image(Recipe("alpine:3.20"), "it:latest")]
        built = (await tags(client))["it:latest"]
        assert log == [
            LogEntry(1, Log("pulling alpine:3.20")),
            LogEntry(2, ImageJobEnd(built)),
        ]


async def test_an_exec_iterated_yields_its_output_and_how_it_ended(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20")
        log = [entry async for entry in sandbox.exec(["echo", "hi"])]
        assert log == [LogEntry(1, Stdout("hi")), LogEntry(2, SandboxJobEnd(0))]


async def test_a_started_job_awaited_then_iterated_starts_it_once(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        before = len(await client.list_image_job())
        build = client.build_image(Recipe("alpine:3.20"), "once:latest")
        assert len(await client.list_image_job()) == before
        job = await build
        assert await build == job
        log = [entry.entry async for entry in build]
        assert job.digest is not None
        assert log[-1] == ImageJobEnd(digest=job.digest)
        assert (await client.get_image_job(job.id)).digest == job.digest
        assert len(await client.list_image_job()) == before + 1


async def tags(client: Client) -> dict[str, str]:
    """Every tag on the server's images, with the digest it points at."""
    return {item.reference: item.digest for item in await client.list_image()}


async def test_an_image_is_tagged_downloaded_removed_and_uploaded_back(
    test_server: str, tmp_path: Path
) -> None:
    with Client.spawn(test_server) as client:
        digest = (await tags(client))["alpine:3.20"]
        await client.tag_image("alpine:3.20", "copy:1")
        assert (await tags(client))["copy:1"] == digest

        archive = tmp_path / "copy.tar"
        events = [event async for event in client.download_image("copy:1", archive)]
        assert events[-1] == Output(ImageDownloadOutput(digest))
        await client.remove_image("copy:1")
        assert "copy:1" not in await tags(client)
        events = [event async for event in client.upload_image(archive, "copy:1")]
        assert events[-1] == Output(ImageUploadOutput(digest))
        assert (await tags(client))["copy:1"] == digest

        await client.untag_image("copy:1")
        assert "copy:1" not in await tags(client)


async def test_a_push_ends_with_the_digest_its_reference_points_at(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        digest = (await tags(client))["alpine:3.20"]
        push = client.push_image("alpine:3.20", BearerAuth("secret"))
        log = [entry.entry async for entry in push]
        assert log == [Log("pushing alpine:3.20 as bearer"), ImageJobEnd(digest=digest)]
        missing = await client.push_image("nothing:here")
        assert missing.error == "no image nothing:here"


async def test_a_mount_at_a_path_taken_is_refused(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        volume = await client.get_volume("v")
        await volume.mount("notion", NotionSource("secret"))
        with pytest.raises(VirtxError) as refused:
            await volume.mount("notion", NotionSource("secret"))
        assert refused.value.status == 409
        await volume.unmount("notion")
        assert await volume.mounts() == []
