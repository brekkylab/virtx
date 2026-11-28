import asyncio
import json
import socket
import time
import urllib.request

import pytest
import virtx
from conftest import ALPINE, alpine, fixture, unique
from virtx import (
    BearerAuth,
    Client,
    PortForwarder,
    Project,
    ProjectMemberListOutputItem,
    Role,
    VirtxError,
)


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
    project = fixture("VIRTX_TEST_PROJECT")
    with Client.connect(fixture("VIRTX_TEST_URL"), None, project) as anonymous:
        with pytest.raises(VirtxError) as refused:
            await anonymous.list_volume()
        assert refused.value.status == 401


async def test_a_port_forwarder_listens_on_the_loopback_until_closed(tcp_client: Client) -> None:
    await alpine(tcp_client)
    sandbox = await tcp_client.create_sandbox(ALPINE, ports=[8000])
    try:
        forwarder = await PortForwarder.open(sandbox, 8000)
        assert (forwarder.port, forwarder.on) == (8000, "sandbox")
        host, port = forwarder.local.rsplit(":", 1)
        assert host == "127.0.0.1"
        # A connection is taken, whatever the sandbox does with it.
        socket.create_connection((host, int(port)), timeout=5).close()
        # At an address asked for.
        with socket.socket() as free:
            free.bind(("127.0.0.1", 0))
            asked = f"127.0.0.1:{free.getsockname()[1]}"
        with await PortForwarder.open(sandbox, 8000, local=asked) as at:
            assert at.local == asked

        forwarder.close()
        forwarder.close()
        # Its listener goes a moment after: a connection made as it goes is reset.
        refused = False
        until = time.monotonic() + 5
        while not refused and time.monotonic() < until:
            try:
                socket.create_connection((host, int(port)), timeout=5).close()
            except ConnectionRefusedError:
                refused = True
            except ConnectionResetError:
                pass
            await asyncio.sleep(0.05)
        assert refused
    finally:
        await sandbox.remove()


async def test_a_project_is_made_its_members_set_and_removed() -> None:
    users = json.loads(fixture("VIRTX_TEST_USERS"))
    name = unique("project")
    with Client.connect(fixture("VIRTX_TEST_URL"), BearerAuth(users["alice"])) as alice:
        made = await alice.create_project(name)
        assert made == Project(alice, made.id, name, "alice@example.com", Role.OWNER)
        assert made in await alice.list_project()
        got = await alice.get_project(name)
        assert (got, got.client) == (made, alice)
        assert await alice.get_project(made.id) == made

        owner = ProjectMemberListOutputItem("alice@example.com", Role.OWNER)
        added = await made.set_member("bob@example.com", Role.MEMBER)
        assert added == ProjectMemberListOutputItem("bob@example.com", Role.MEMBER)
        assert await made.members() == [owner, added]
        await made.remove_member("bob@example.com")
        assert await made.members() == [owner]

        with pytest.raises(VirtxError) as refused:
            await alice.create_project(name)
        assert (refused.value.source, refused.value.status) == ("server", 409)
        await made.remove()
        with pytest.raises(VirtxError) as refused:
            await alice.get_project(name)
        assert refused.value.status == 403


async def test_set_project_changes_what_a_request_reaches(client: Client) -> None:
    name = unique("elsewhere")
    await client.create_project(name)
    in_default = await client.create_volume(unique("in-default"))
    client.set_project(name)
    in_project = await client.create_volume(unique("in-project"))
    assert await client.list_volume() == [in_project]
    client.set_project(None)
    assert in_default in await client.list_volume()
    assert in_project not in await client.list_volume()


def test_a_project_no_header_carries_is_refused(client: Client) -> None:
    with pytest.raises(VirtxError):
        client.set_project("bad\nname")
    with pytest.raises(VirtxError):
        Client.connect(fixture("VIRTX_TEST_URL"), project="bad\nname")


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


async def test_a_package_is_kept_makes_a_sandbox_and_is_removed(client: Client) -> None:
    await alpine(client)
    name = unique("package")
    access = [virtx.JwtAccess("https://idp.example", "app", claims={"r": 1})]
    package = await client.put_package(
        name,
        ALPINE,
        max={"vcpus": 8},
        access=access,
        origins=["https://app.example"],
        limits=virtx.PackageLimits(per_subject=1),
    )
    assert isinstance(package, virtx.Package)
    assert (package.name, package.sandbox["image"], package.max) == (
        name,
        {"type": "tag", "tag": ALPINE},
        {"vcpus": 8},
    )
    assert package.access == access
    assert package.limits == virtx.PackageLimits(per_subject=1)
    assert (await client.get_package(package.id)).name == name
    assert package.id in [kept.id for kept in await client.list_package()]

    made = await package.create_sandbox(name=unique("of-package"))
    assert isinstance(made, virtx.Sandbox)
    await made.remove()
    await package.remove()
    with pytest.raises(VirtxError) as refused:
        await client.get_package(package.id)
    assert refused.value.status == 404


async def test_an_app_client_reaches_the_sandbox_a_package_lets_it_make(tcp_client: Client) -> None:
    await alpine(tcp_client)
    package = await tcp_client.put_package(unique("members"), ALPINE)
    url = fixture("VIRTX_TEST_URL")
    with pytest.raises(VirtxError) as refused:
        await virtx.AppClient.connect(url, package.id)
    assert refused.value.status == 401
    await package.remove()

    # The fixture's own issuer lets bob in, a member of another project.
    signs_in = await asyncio.to_thread(urllib.request.urlopen, f"{url}auth")
    oidc = json.loads(signs_in.read())["oidc"]
    issuer = oidc["issuer"]
    access = virtx.JwtAccess(issuer, oidc["client_id"], jwks=f"{issuer}/keys")
    package = await tcp_client.put_package(unique("jwt"), ALPINE, access=[access])
    bob = BearerAuth(json.loads(fixture("VIRTX_TEST_USERS"))["bob"])
    async with await virtx.AppClient.connect(url, package.id, bob) as app:
        assert await app.sandbox.exec(["true"]).complete() == 0
        id = app.sandbox.id
    assert (await tcp_client.get_sandbox(id)).id == id, "closed, it stays"

    app = await virtx.AppClient.resume(url, package.id, bob, id)
    assert app.sandbox.id == id
    await app.remove()
    with pytest.raises(VirtxError) as gone:
        await tcp_client.get_sandbox(id)
    assert gone.value.status == 404, "removed, it is deleted"
    with pytest.raises(VirtxError) as refused:
        await virtx.AppClient.resume(url, package.id, bob, id)
    assert refused.value.status == 403
    await package.remove()


async def test_a_member_finds_a_package_s_sandboxes(tcp_client: Client) -> None:
    await alpine(tcp_client)
    package = await tcp_client.put_package(unique("found"), ALPINE)
    made = await package.create_sandbox()
    found = await package.list_sandbox()
    assert [sandbox.id for sandbox in found] == [made.id]
    assert all(isinstance(sandbox, virtx.Sandbox) for sandbox in found)
    assert [s.id for s in await tcp_client.list_package_sandbox(package.name)] == [made.id]
    await made.remove()
    await package.remove()
