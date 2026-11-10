"""Volumes, sandboxes and the jobs of images and sandboxes, against ``virtx-core``'s
``test_server`` example over stdio."""

import pytest

from virtx import (
    Client,
    ImageJob,
    ImageJobEnd,
    ImageJobKind,
    InboundRule,
    Log,
    LogEntry,
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
    Stderr,
    Stdout,
    Truncated,
    VirtxError,
    Volume,
)


async def test_a_sandbox_stops_starts_again_and_is_found_by_its_name(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        made = await client.create_sandbox("alpine:3.20", name="work", ports=[8000])
        assert made.state == SandboxState.RUNNING and made.running
        assert made.name == "work" and not made.ephemeral
        assert made.auto_stop_minutes == 15 and made.auto_delete_days == 30
        found = await client.find_sandbox("work")
        assert found is not None and found.id == made.id and found.client is client
        assert await client.find_sandbox("nobody") is None
        with pytest.raises(VirtxError) as refused:
            await client.create_sandbox("alpine:3.20", name="work")
        assert refused.value.status == 409

        stopped = await made.stop()
        assert stopped.state == SandboxState.STOPPED and stopped.client is client
        started = await stopped.start()
        assert started.running and started.ports == made.ports
        assert (await client.stop_sandbox(made.id)).state == SandboxState.STOPPED
        assert (await client.start_sandbox(made.id)).running

        kept = await client.create_sandbox("alpine:3.20", auto_delete_days="never")
        assert kept.auto_delete_days is None
        gone = await client.create_sandbox("alpine:3.20", ephemeral=True)
        await gone.stop()
        with pytest.raises(VirtxError) as refused:
            await client.get_sandbox(gone.id)
        assert refused.value.status == 404

        job = await made.commit("mine:v1")
        assert job.kind == ImageJobKind.COMMIT and job.reference == "mine:v1"
        assert (await client.commit_image(made, "mine:v2")).reference == "mine:v2"
        await made.remove()
        with pytest.raises(VirtxError) as refused:
            await client.get_sandbox(made.id)
        assert refused.value.status == 404


async def test_a_sandbox_made_for_a_block_is_deleted_on_leaving_it(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        async with await client.create_sandbox("alpine:3.20") as sandbox:
            assert sandbox.running
            id = sandbox.id
        with pytest.raises(VirtxError) as refused:
            await client.get_sandbox(id)
        assert refused.value.status == 404

        async with await client.create_sandbox("alpine:3.20") as sandbox:
            await sandbox.remove()


async def test_a_port_opened_after_is_kept_in_the_sandbox_s_ports(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20", ports=[8000])
        opened = await sandbox.open_port(8080)
        assert opened == SandboxPort(8080, opened.url)
        assert [port.port for port in sandbox.ports] == [8000, 8080]
        assert await sandbox.open_port(8080) == opened
        assert sandbox.ports == (await client.start_sandbox(sandbox.id)).ports

        got = await client.get_sandbox(sandbox.id)
        assert await got.open_port(8000) == sandbox.ports[0]
        assert got.ports == [sandbox.ports[0]]


async def test_a_port_closed_is_dropped_from_the_sandbox_s_ports(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20", ports=[8000, 8080])
        kept = sandbox.ports[1]
        await sandbox.close_port(8000)
        assert sandbox.ports == [kept]
        await client.close_sandbox_port(sandbox.id, 8080)
        assert (await client.start_sandbox(sandbox.id)).ports == []
        with pytest.raises(VirtxError, match="404"):
            await sandbox.close_port(8080)
        assert sandbox.ports == [kept]


async def test_a_port_says_what_it_is_for_and_whether_it_is_http(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox(
            "alpine:3.20",
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


async def test_a_port_s_inbound_rules_are_set_as_it_opens_and_after(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        office = InboundRule("203.0.113.7", "office")
        sandbox = await client.create_sandbox(
            "alpine:3.20", ports=[PortSpec(22, inbound=[office]), 5900]
        )
        office = InboundRule("203.0.113.7/32", "office")
        assert await sandbox.inbound(22) == PortInbound(22, [office])

        lan = [InboundRule("10.0.0.0/8")]
        assert await sandbox.set_inbound(22, lan) == PortInbound(22, lan)
        assert await client.get_sandbox_port_inbound(sandbox.id, 22) == PortInbound(22, lan)
        cleared = await client.set_sandbox_port_inbound(sandbox.id, 22, [])
        assert cleared.rules == []

        # An HTTP port opened with them keeps them.
        await sandbox.open_port(5173, protocol="http", inbound=[{"source": "10.0.0.0/8"}])
        assert (await sandbox.inbound(5173)).rules == lan

        with pytest.raises(VirtxError) as refused:
            await sandbox.set_inbound(5900, lan)
        assert (refused.value.status, refused.value.code) == (409, "not_public")
        with pytest.raises(VirtxError) as refused:
            await sandbox.inbound(9000)
        assert (refused.value.status, refused.value.code) == (404, "not_found")
        with pytest.raises(VirtxError) as refused:
            await sandbox.set_inbound(22, [InboundRule("nowhere")])
        assert (refused.value.status, refused.value.code) == (400, "invalid")


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


async def test_a_sandbox_used_with_async_with_is_deleted_on_leaving(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        async with await client.create_sandbox("alpine:3.20") as sandbox:
            assert (await client.get_sandbox(sandbox.id)).running
        with pytest.raises(VirtxError, match="404"):
            await client.get_sandbox(sandbox.id)

        with pytest.raises(RuntimeError):
            async with await client.create_sandbox("alpine:3.20") as failed:
                raise RuntimeError
        with pytest.raises(VirtxError, match="404"):
            await client.get_sandbox(failed.id)

        # One deleted already in the block is left so.
        async with await client.create_sandbox("alpine:3.20") as removed:
            await removed.remove()


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
        sandbox = await client.create_sandbox("alpine:3.20", ports=[8000])
        assert sandbox.running
        on_handle = await client.create_sandbox(
            "alpine:3.20",
            volumes=[SandboxVolume(await client.get_volume("v"), "/data", readonly=True)],
        )
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
        log = [entry.entry async for entry in job.log()]
        assert log == [Log("pulling alpine:3.20"), ImageJobEnd(digest="sha256:0")]
        assert [entry.entry async for entry in job.log(after=1)] == log[1:]
        assert (await job.get()).digest == "sha256:0"
        assert (await client.get_image_job(job.id)).digest == "sha256:0"
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
        assert log == [
            LogEntry(1, Log("pulling alpine:3.20")),
            LogEntry(2, ImageJobEnd("sha256:0")),
        ]


async def test_an_exec_iterated_yields_its_output_and_how_it_ended(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20")
        log = [entry async for entry in sandbox.exec(["echo", "hi"])]
        assert log == [LogEntry(1, Stdout("hi")), LogEntry(2, SandboxJobEnd(0))]


async def test_an_exec_iterated_then_awaited_answers_the_job_it_read(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20")
        echo = sandbox.exec(["echo", "hi"])
        log = [entry async for entry in echo]
        assert log[-1] == LogEntry(2, SandboxJobEnd(0))
        job = await echo
        assert job.cmd == ["echo", "hi"] and job.client is client
        assert [j.id for j in await sandbox.jobs()] == [job.id]


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
