"""Images, snapshots, volumes and their mounts, sandboxes, and the jobs of images and sandboxes,
against the fixture's ``virtx-stdio``."""

from pathlib import Path

import pytest
from conftest import ALPINE, alpine, tags, unique
from virtx import (
    BearerAuth,
    Client,
    ImageDownloadOutput,
    ImageJob,
    ImageJobEnd,
    ImageJobKind,
    ImageUploadOutput,
    Output,
    Recipe,
    SandboxJob,
    SandboxJobEnd,
    SandboxJobKind,
    SandboxVolume,
    Stderr,
    Stdout,
    VirtxError,
    Volume,
)


async def test_a_snapshot_is_created_got_listed_and_removed(client: Client) -> None:
    name = unique("snapshot")
    created = await client.create_snapshot(name, disk_gib=4)
    assert (created.name, created.in_use) == (name, False)
    assert await client.get_snapshot(name) == created
    assert created in await client.list_snapshot()
    await created.remove()
    with pytest.raises(VirtxError) as refused:
        await client.get_snapshot(name)
    assert refused.value.status == 404


async def test_a_volume_is_created_and_got(client: Client) -> None:
    name = unique("volume")
    created = await client.create_volume(name)
    assert created == Volume(client, name, 0)
    assert await client.get_volume(name) == created
    await created.remove()
    with pytest.raises(VirtxError) as refused:
        await client.get_volume(name)
    assert refused.value.status == 404


async def test_a_sandbox_runs_a_command_whose_log_ends_with_its_code(client: Client) -> None:
    await alpine(client)
    snapshot = await client.create_snapshot(unique("snapshot"))
    volume = await client.create_volume(unique("volume"))
    sandbox = await client.create_sandbox(
        ALPINE,
        snapshot=snapshot,
        volumes=[SandboxVolume(volume, "/data", readonly=True)],
        ports=[8000],
    )
    assert sandbox.snapshot == snapshot.name and sandbox.running
    assert [port.port for port in sandbox.ports] == [8000]
    assert SandboxVolume(volume, "/data").to_dict() == {
        "volume": volume.name,
        "path": "/data",
        "readonly": False,
    }
    got = await client.get_sandbox(sandbox.id)
    assert got.id == sandbox.id
    assert sandbox.id in [item.id for item in await client.list_sandbox()]

    job = await sandbox.exec(["echo", "hi"])
    assert isinstance(job, SandboxJob) and job.sandbox_id == sandbox.id
    assert job.kind == SandboxJobKind.EXEC and job.cmd == ["echo", "hi"]
    log = [entry.entry async for entry in job.log()]
    assert log == [Stdout("hi"), SandboxJobEnd(0)]
    assert (await job.get()).code == 0
    assert (await got.job(job.id)).code == 0
    assert job.id in [item.id for item in await got.jobs()]

    failed = await sandbox.exec(["sh", "-c", "echo failed >&2; exit 1"])
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
    await snapshot.remove()
    await volume.remove()


async def test_a_log_is_read_again_after_an_entry(client: Client) -> None:
    await alpine(client)
    sandbox = await client.create_sandbox(ALPINE)
    job = await sandbox.exec(["sh", "-c", "for i in 1 2 3; do echo $i; done"])
    log = [entry async for entry in job.log()]
    assert [entry.entry for entry in log] == [
        Stdout("1"),
        Stdout("2"),
        Stdout("3"),
        SandboxJobEnd(0),
    ]
    assert [entry async for entry in job.log(after=log[1].id)] == log[2:]
    await sandbox.remove()


@pytest.mark.xfail(
    strict=True,
    raises=VirtxError,
    reason="no server starts a process with a stdin to write to yet: `exec` with `stdin` is refused",
)
async def test_stdin_written_comes_back_until_it_is_closed(client: Client) -> None:
    await alpine(client)
    sandbox = await client.create_sandbox(ALPINE)
    cat = await sandbox.exec(["cat"], stdin=True)
    await cat.write_stdin("a\nb\n")
    await cat.write_stdin(eof=True)
    log = [entry.entry async for entry in cat.log()]
    assert log == [Stdout("a"), Stdout("b"), SandboxJobEnd(0)]
    await sandbox.remove()


async def test_a_signal_ends_a_process(client: Client) -> None:
    await alpine(client)
    sandbox = await client.create_sandbox(ALPINE)
    sleep = await sandbox.exec(["sleep", "60"])
    assert sleep.running
    await sleep.signal("SIGTERM")
    log = [entry.entry async for entry in sleep.log()]
    assert log[-1] == SandboxJobEnd(128 + 15)
    got = await client.get_sandbox(sandbox.id)
    assert not (await got.job(sleep.id)).running
    await sandbox.remove()


async def test_an_image_build_is_a_job_whose_log_ends_with_its_digest(client: Client) -> None:
    await alpine(client)
    reference = f"{unique('built')}:latest"
    job = await client.build_image(Recipe(ALPINE).run(f"echo {reference} > /built"), reference)
    assert isinstance(job, ImageJob)
    assert (job.kind, job.reference) == (ImageJobKind.BUILD, reference)
    log = [entry async for entry in job.log()]
    digest = (await tags(client))[reference]
    assert log[-1].entry == ImageJobEnd(digest=digest)
    assert [entry async for entry in job.log(after=log[0].id)] == log[1:]
    assert (await job.get()).digest == digest
    assert (await client.get_image_job(job.id)).digest == digest
    assert job.id in [item.id for item in await client.list_image_job()]
    await job.remove()
    with pytest.raises(VirtxError) as refused:
        await job.get()
    assert refused.value.status == 404
    await client.remove_image(reference)


async def test_a_build_iterated_yields_its_log_to_its_end(client: Client) -> None:
    await alpine(client)
    reference = f"{unique('iterated')}:latest"
    log = [
        entry
        async for entry in client.build_image(
            Recipe(ALPINE).run(f"echo {reference} > /built"), reference
        )
    ]
    built = (await tags(client))[reference]
    assert log[-1].entry == ImageJobEnd(built)
    await client.remove_image(reference)


async def test_an_exec_iterated_yields_its_output_and_how_it_ended(client: Client) -> None:
    await alpine(client)
    sandbox = await client.create_sandbox(ALPINE)
    log = [entry.entry async for entry in sandbox.exec(["echo", "hi"])]
    assert log == [Stdout("hi"), SandboxJobEnd(0)]
    await sandbox.remove()


async def test_a_started_job_awaited_then_iterated_starts_it_once(client: Client) -> None:
    await alpine(client)
    reference = f"{unique('once')}:latest"
    before = len(await client.list_image_job())
    build = client.build_image(Recipe(ALPINE).run(f"echo {reference} > /built"), reference)
    assert len(await client.list_image_job()) == before
    job = await build
    assert await build == job
    log = [entry.entry async for entry in build]
    digest = (await tags(client))[reference]
    assert log[-1] == ImageJobEnd(digest=digest)
    assert (await client.get_image_job(job.id)).digest == digest
    assert len(await client.list_image_job()) == before + 1
    await client.remove_image(reference)


async def test_an_image_is_tagged_downloaded_removed_and_uploaded_back(
    client: Client, tmp_path: Path
) -> None:
    digest = await alpine(client)
    reference = f"{unique('copy')}:1"
    await client.tag_image(ALPINE, reference)
    assert (await tags(client))[reference] == digest

    archive = tmp_path / "copy.tar"
    events = [event async for event in client.download_image(reference, archive)]
    assert events[-1] == Output(ImageDownloadOutput(digest))
    await client.remove_image(reference)
    assert reference not in await tags(client)
    events = [event async for event in client.upload_image(archive, reference)]
    assert events[-1] == Output(ImageUploadOutput(digest))
    assert (await tags(client))[reference] == digest

    await client.untag_image(reference)
    assert reference not in await tags(client)


async def test_a_push_is_a_job_that_ends_with_its_end(client: Client) -> None:
    await alpine(client)
    # A registry that does not resolve, so the push fails before it reaches anyone.
    reference = f"registry.invalid/{unique('pushed')}:latest"
    await client.tag_image(ALPINE, reference)
    job = await client.push_image(reference, BearerAuth("test-secret"))
    assert job.kind == ImageJobKind.PUSH
    end = [entry.entry async for entry in job.log()][-1]
    assert isinstance(end, ImageJobEnd) and end.error
    await client.remove_image(reference)
