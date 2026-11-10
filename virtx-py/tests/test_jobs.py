"""Images, volumes and their mounts, sandboxes and their ports, and the jobs of images and
sandboxes, against the fixture's ``virtx-stdio``, and its ``virtx-server-tcp`` where a port is
forwarded."""

import asyncio
import gc
import logging
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
    InboundRule,
    Output,
    PortForwarder,
    PortInbound,
    PortProtocol,
    PortSpec,
    Recipe,
    SandboxJob,
    SandboxJobEnd,
    SandboxJobKind,
    SandboxPort,
    SandboxState,
    SandboxVolume,
    StartedJob,
    Stderr,
    Stdout,
    VirtxError,
    Volume,
)


async def test_a_sandbox_stops_starts_again_and_is_found_by_its_name(client: Client) -> None:
    await alpine(client)
    name = unique("work")
    made = await client.create_sandbox(ALPINE, name=name, ports=[8000])
    assert made.state == SandboxState.RUNNING and made.running
    assert made.name == name and not made.ephemeral
    assert made.auto_stop_minutes == 15 and made.auto_delete_days == 30
    found = await client.find_sandbox(name)
    assert found is not None and found.id == made.id and found.client is client
    assert await client.find_sandbox(unique("nobody")) is None
    with pytest.raises(VirtxError) as refused:
        await client.create_sandbox(ALPINE, name=name)
    assert (refused.value.status, refused.value.code) == (409, "exists")

    stopped = await made.stop()
    assert stopped.state == SandboxState.STOPPED and stopped.client is client
    started = await stopped.start()
    assert started.running and [p.port for p in started.ports] == [8000]
    assert (await client.stop_sandbox(made.id)).state == SandboxState.STOPPED
    assert (await client.start_sandbox(made.id)).running

    kept = await client.create_sandbox(ALPINE, auto_delete_days="never")
    assert kept.auto_delete_days is None
    await kept.remove()
    gone = await client.create_sandbox(ALPINE, ephemeral=True)
    await gone.stop()
    with pytest.raises(VirtxError) as refused:
        await client.get_sandbox(gone.id)
    assert refused.value.status == 404

    first, second = f"{unique('mine')}:v1", f"{unique('mine')}:v2"
    job = await made.commit(first)
    assert job.kind == ImageJobKind.COMMIT and job.reference == first
    end = [entry.entry async for entry in job.log()][-1]
    assert end == ImageJobEnd((await tags(client))[first])
    job = await client.commit_image(made, second)
    assert job.reference == second
    [entry async for entry in job.log()]
    await client.remove_image(first)
    await client.remove_image(second)
    await made.remove()
    with pytest.raises(VirtxError) as refused:
        await client.get_sandbox(made.id)
    assert refused.value.status == 404


async def test_a_sandbox_made_for_a_block_is_deleted_on_leaving_it(client: Client) -> None:
    await alpine(client)
    async with await client.create_sandbox(ALPINE) as sandbox:
        assert sandbox.running
        id = sandbox.id
    with pytest.raises(VirtxError) as refused:
        await client.get_sandbox(id)
    assert refused.value.status == 404

    async with await client.create_sandbox(ALPINE) as sandbox:
        await sandbox.remove()


async def test_a_sandbox_used_with_async_with_is_deleted_on_leaving(client: Client) -> None:
    await alpine(client)
    async with await client.create_sandbox(ALPINE) as sandbox:
        assert (await client.get_sandbox(sandbox.id)).running
    with pytest.raises(VirtxError, match="404"):
        await client.get_sandbox(sandbox.id)

    with pytest.raises(RuntimeError):
        async with await client.create_sandbox(ALPINE) as failed:
            raise RuntimeError
    with pytest.raises(VirtxError, match="404"):
        await client.get_sandbox(failed.id)

    # One deleted already in the block is left so.
    async with await client.create_sandbox(ALPINE) as removed:
        await removed.remove()


async def test_a_port_opened_after_is_kept_in_the_sandbox_s_ports(client: Client) -> None:
    await alpine(client)
    sandbox = await client.create_sandbox(ALPINE, ports=[8000])
    opened = await sandbox.open_port(8080)
    assert opened == SandboxPort(8080, opened.url)
    assert [port.port for port in sandbox.ports] == [8000, 8080]
    assert await sandbox.open_port(8080) == opened
    assert [p.port for p in (await client.start_sandbox(sandbox.id)).ports] == [8000, 8080]

    got = await client.get_sandbox(sandbox.id)
    assert await got.open_port(8000) == sandbox.ports[0]
    assert got.ports == [sandbox.ports[0]]
    await sandbox.remove()


async def test_a_port_closed_is_dropped_from_the_sandbox_s_ports(client: Client) -> None:
    await alpine(client)
    sandbox = await client.create_sandbox(ALPINE, ports=[8000, 8080])
    kept = sandbox.ports[1]
    await sandbox.close_port(8000)
    assert sandbox.ports == [kept]
    await client.close_sandbox_port(sandbox.id, 8080)
    assert (await client.start_sandbox(sandbox.id)).ports == []
    with pytest.raises(VirtxError) as refused:
        await sandbox.close_port(8080)
    assert (refused.value.status, refused.value.code) == (404, "not_found")
    assert sandbox.ports == [kept]
    await sandbox.remove()


async def test_a_port_says_what_it_is_for_and_whether_it_is_http(client: Client) -> None:
    await alpine(client)
    sandbox = await client.create_sandbox(
        ALPINE,
        ports=[
            5900,
            PortSpec(5173, label="dev server", protocol=PortProtocol.HTTP),
            {"port": 3000, "label": "api"},
        ],
    )
    vnc, dev, api = sandbox.ports
    assert (vnc.port, vnc.label, vnc.protocol) == (5900, None, PortProtocol.TCP)
    assert (dev.port, dev.label, dev.protocol) == (5173, "dev server", PortProtocol.HTTP)
    assert (api.port, api.label, api.protocol) == (3000, "api", PortProtocol.TCP)

    web = await sandbox.open_port(8080, label="docs", protocol="http")
    assert web == SandboxPort(8080, web.url, "docs", PortProtocol.HTTP)
    assert sandbox.ports[-1] == web
    renamed = await client.open_sandbox_port(sandbox.id, 8080, label="manual")
    assert (renamed.label, renamed.protocol) == ("manual", PortProtocol.HTTP)
    plain = await client.open_sandbox_port(sandbox.id, 9000)
    assert (plain.label, plain.protocol) == (None, PortProtocol.TCP)
    await sandbox.remove()


async def test_inbound_rules_are_refused_by_a_server_that_keeps_none(client: Client) -> None:
    await alpine(client)
    office = InboundRule("203.0.113.7", "office")
    with pytest.raises(VirtxError) as refused:
        await client.create_sandbox(ALPINE, ports=[PortSpec(22, inbound=[office])])
    assert (refused.value.status, refused.value.code) == (400, "invalid")
    assert "virtx cloud's" in str(refused.value)


async def test_a_forwarded_port_nothing_listens_on_says_so(
    tcp_client: Client, caplog: pytest.LogCaptureFixture
) -> None:
    caplog.set_level(logging.WARNING, logger="virtx")
    await alpine(tcp_client)
    sandbox = await tcp_client.create_sandbox(ALPINE, ports=[7000])
    with PortForwarder.open(tcp_client, sandbox.ports[0]) as forwarder:
        assert forwarder.last_error is None
        reader, writer = await asyncio.open_connection(forwarder.host, forwarder.port)
        assert await reader.read() == b""
        writer.close()

        # Kept once the connection that failed has closed: looked for a minute.
        for _ in range(6000):
            if forwarder.last_error is not None:
                break
            await asyncio.sleep(0.01)
        error = forwarder.last_error
        assert isinstance(error, VirtxError), "no error was kept"
        assert str(error).startswith(f"port 7000, forwarded at {forwarder.address}: ")
        assert "nothing listens on the port" in str(error)
        assert sandbox.ports[0].url not in str(error)

        for _ in range(6000):
            if any(r.name == "virtx" and r.getMessage() == str(error) for r in caplog.records):
                break
            await asyncio.sleep(0.01)
        warned = [r for r in caplog.records if r.name == "virtx"]
        assert [(r.levelno, r.getMessage()) for r in warned][-1:] == [(logging.WARNING, str(error))]
    await sandbox.remove()


def test_a_port_s_inbound_rules_read_and_write_as_the_wire_has_them() -> None:
    office = InboundRule("203.0.113.7", "office")
    assert office.to_dict() == {"source": "203.0.113.7", "description": "office"}
    assert InboundRule("10.0.0.0/8").to_dict() == {"source": "10.0.0.0/8"}
    assert InboundRule.from_dict(office.to_dict()) == office
    answered = {"port": 22, "rules": [{"source": "10.0.0.0/8"}], "address": "tcp.example:1"}
    assert PortInbound.from_dict(answered) == PortInbound(
        22, [InboundRule("10.0.0.0/8")], "tcp.example:1"
    )
    assert PortInbound.from_dict({"port": 22, "rules": []}) == PortInbound(22, [])


def test_a_port_spec_is_written_as_its_number_unless_it_says_more() -> None:
    assert PortSpec(8080).to_dict() == 8080
    assert PortSpec(22, label="ssh").to_dict() == {"port": 22, "label": "ssh"}
    assert PortSpec(5173, protocol="http").to_dict() == {"port": 5173, "protocol": "http"}
    assert PortSpec(22, inbound=[InboundRule("10.0.0.0/8", "lan")]).to_dict() == {
        "port": 22,
        "inbound": [{"source": "10.0.0.0/8", "description": "lan"}],
    }
    assert PortSpec(22, inbound=[]).to_dict() == {"port": 22, "inbound": []}
    assert SandboxPort.from_dict({"port": 1, "url": "u"}) == SandboxPort(1, "u")


def test_a_tcp_port_answered_with_an_address_has_it() -> None:
    reached = SandboxPort.from_dict({"port": 22, "url": "u", "address": "tcp.example:20001"})
    assert reached == SandboxPort(22, "u", address="tcp.example:20001")
    assert SandboxPort.from_dict({"port": 1, "url": "u"}).address is None


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
    volume = await client.create_volume(unique("volume"))
    sandbox = await client.create_sandbox(
        ALPINE,
        volumes=[SandboxVolume(volume, "/data", readonly=True)],
        ports=[8000],
        abins=["index"],
    )
    assert sandbox.running
    assert sandbox.abins == ["index"]
    assert [port.port for port in sandbox.ports] == [8000]
    assert SandboxVolume(volume, "/data").to_dict() == {
        "volume": volume.name,
        "path": "/data",
        "readonly": False,
    }
    got = await client.get_sandbox(sandbox.id)
    assert got.id == sandbox.id and got.ports == [] and got.abins == ["index"]
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


async def test_an_exec_iterated_then_awaited_answers_the_job_it_read(client: Client) -> None:
    await alpine(client)
    sandbox = await client.create_sandbox(ALPINE)
    echo = sandbox.exec(["echo", "hi"])
    log = [entry async for entry in echo]
    assert log[-1].entry == SandboxJobEnd(0)
    job = await echo
    assert job.cmd == ["echo", "hi"] and job.client is client
    assert [j.id for j in await sandbox.jobs()] == [job.id]
    await sandbox.remove()


async def test_a_started_job_failing_when_iterated_leaves_no_unretrieved_future_exception() -> None:
    loop = asyncio.get_running_loop()
    warnings: list[str] = []
    old_handler = loop.get_exception_handler()
    loop.set_exception_handler(lambda _l, ctx: warnings.append(ctx.get("message", "")))
    try:

        async def failing_run():
            raise RuntimeError("exec failed to start")
            yield

        job = StartedJob(lambda: None, lambda j: None, failing_run)
        with pytest.raises(RuntimeError, match="exec failed to start"):
            async for _ in job:
                pass

        del job
        gc.collect()
        assert not any("Future exception was never retrieved" in w for w in warnings)
    finally:
        loop.set_exception_handler(old_handler)


async def test_a_started_job_failing_when_iterated_still_raises_when_awaited() -> None:
    async def failing_run():
        raise RuntimeError("exec failed to start")
        yield

    job = StartedJob(lambda: None, lambda j: None, failing_run)
    with pytest.raises(RuntimeError, match="exec failed to start"):
        async for _ in job:
            pass

    with pytest.raises(RuntimeError, match="exec failed to start"):
        await job


async def test_a_started_job_ending_before_its_job_leaves_no_unretrieved_future_exception() -> None:
    loop = asyncio.get_running_loop()
    warnings: list[str] = []
    old_handler = loop.get_exception_handler()
    loop.set_exception_handler(lambda _l, ctx: warnings.append(ctx.get("message", "")))
    try:

        async def empty_run():
            if False:
                yield

        job = StartedJob(lambda: None, lambda j: None, empty_run)
        log = [entry async for entry in job]
        assert log == []

        del job
        gc.collect()
        assert not any("Future exception was never retrieved" in w for w in warnings)
    finally:
        loop.set_exception_handler(old_handler)


async def test_a_started_job_ending_before_its_job_raises_when_awaited() -> None:
    async def empty_run():
        if False:
            yield

    job = StartedJob(lambda: None, lambda j: None, empty_run)
    log = [entry async for entry in job]
    assert log == []

    with pytest.raises(RuntimeError, match="the job's log ended before its job"):
        await job


async def test_an_exec_failing_to_start_when_iterated_leaves_no_unretrieved_future_exception(
    client: Client,
) -> None:
    loop = asyncio.get_running_loop()
    warnings: list[str] = []
    old_handler = loop.get_exception_handler()
    loop.set_exception_handler(lambda _l, ctx: warnings.append(ctx.get("message", "")))
    try:
        await alpine(client)
        sandbox = await client.create_sandbox(ALPINE)
        exec_job = sandbox.exec(["no-such-command"])
        with pytest.raises(VirtxError) as refused:
            async for _ in exec_job:
                pass
        assert (refused.value.status, refused.value.code) == (422, "not_started")

        # Its traceback holds the job: let go, for it to be collected.
        del exec_job, refused
        await sandbox.remove()
        gc.collect()
        await asyncio.sleep(0)
        assert not any("Future exception was never retrieved" in w for w in warnings)
    finally:
        loop.set_exception_handler(old_handler)


async def test_an_exec_failing_to_start_when_iterated_still_raises_when_awaited(
    client: Client,
) -> None:
    await alpine(client)
    sandbox = await client.create_sandbox(ALPINE)
    exec_job = sandbox.exec(["no-such-command"])
    with pytest.raises(VirtxError) as refused:
        async for _ in exec_job:
            pass
    assert (refused.value.status, refused.value.code) == (422, "not_started")

    with pytest.raises(VirtxError) as awaited_refused:
        await exec_job
    assert (awaited_refused.value.status, awaited_refused.value.code) == (422, "not_started")
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
