"""The files of a volume, a snapshot and a sandbox, against the fixture's ``virtx-stdio``."""

import pytest
from conftest import ALPINE, alpine, unique

from virtx import (
    Client,
    DirEntry,
    Files,
    GdriveSource,
    Metadata,
    MountType,
    VirtxError,
)


async def every_call(files: Files, base: str) -> None:
    """Each of a handle's calls on its files, in ``base``: a directory of them, or its root."""

    def at(path: str) -> str:
        return f"{base}/{path}" if base else path

    await files.create_dir(at("dir"))
    await files.write_file(at("dir/a.txt"), b"hello world")
    await files.create_dir_all(at("deep/er/dir"))
    await files.create_dir_all(at("deep/er/dir"))
    assert (await files.stat_file(at("deep/er/dir"))).is_dir

    metadata = await files.stat_file(at("dir/a.txt"))
    assert isinstance(metadata, Metadata)
    assert (metadata.is_dir, metadata.size) == (False, 11)
    entries = await files.read_dir(at("dir"))
    assert entries == [DirEntry("a.txt", metadata)]

    assert await files.read_file(at("dir/a.txt")) == b"hello world"
    assert await files.read_file(at("dir/a.txt"), offset=6, length=3) == b"wor"

    # Text, an etag to write over, and what is there, told without an error.
    await files.write_file(at("dir/t.txt"), "텍스트")
    assert await files.read_text(at("dir/t.txt")) == "텍스트"
    etag = (await files.stat_file(at("dir/t.txt"))).etag
    assert etag
    await files.write_file(at("dir/t.txt"), "text 2", if_match=etag)
    await files.write_file(at("dir/bin"), b"\xff\xfe")
    with pytest.raises(VirtxError, match="UTF-8"):
        await files.read_text(at("dir/bin"))
    assert await files.exists(at("dir/t.txt"))
    assert await files.is_file(at("dir/t.txt"))
    assert not await files.is_dir(at("dir/t.txt"))
    assert await files.is_dir(at("dir"))
    await files.remove_file(at("dir/t.txt"))
    await files.remove_file(at("dir/bin"))
    assert not await files.exists(at("dir/t.txt"))
    await files.remove_file(at("dir/t.txt"), missing_ok=True)

    await files.rename_file(at("dir/a.txt"), at("b.txt"))
    await files.copy_file(at("b.txt"), at("c.txt"))
    await files.remove_file(at("dir"))
    with pytest.raises(VirtxError) as refused:
        await files.stat_file(at("dir"))
    assert refused.value.status == 404
    assert await files.read_file(at("c.txt")) == b"hello world"


async def test_a_volume_s_files_answer_every_call(client: Client) -> None:
    volume = await client.create_volume(unique("files"))
    await every_call(volume, "")
    await volume.remove()


async def test_a_snapshot_s_files_answer_every_call(client: Client) -> None:
    snapshot = await client.create_snapshot(unique("files"))
    await every_call(snapshot, "")
    await snapshot.remove()


async def test_a_sandbox_s_files_answer_every_call(client: Client) -> None:
    await alpine(client)
    sandbox = await client.create_sandbox(ALPINE)
    await every_call(sandbox, "root")
    await sandbox.remove()


async def test_a_volume_carries_its_client_to_its_files_and_mounts(client: Client) -> None:
    name = unique("carried")
    volume = await client.create_volume(name)
    assert (volume.name, volume.size) == (name, 0)
    await volume.create_dir_all("in/dir")
    await volume.write_file("in/a.txt", "hello")
    await volume.rename_file("in/a.txt", "in/b.txt")
    await volume.copy_file("in/b.txt", "c.txt")
    assert await volume.read_text("c.txt") == "hello"
    assert await volume.is_dir("in/dir")
    assert sorted(entry.name for entry in await volume.read_dir("in")) == ["b.txt", "dir"]

    got = await client.get_volume(name)
    assert got.client is client
    assert await got.read_text("in/b.txt") == "hello"
    assert name in [item.name for item in await client.list_volume()]

    mount = await volume.mount("drive", GdriveSource("id", "secret", "token"))
    assert (mount.path, mount.mount_type) == ("drive", MountType.GDRIVE)
    assert await got.mounts() == [mount]
    await volume.unmount("drive")
    assert await got.mounts() == []

    await volume.remove()
    with pytest.raises(VirtxError) as refused:
        await client.get_volume(name)
    assert refused.value.status == 404


async def test_a_snapshot_carries_its_client_to_its_files(client: Client) -> None:
    name = unique("snapshot")
    snapshot = await client.create_snapshot(name)
    await snapshot.write_file("a.txt", "kept")
    assert await (await client.get_snapshot(name)).read_text("a.txt") == "kept"
    await snapshot.remove()
    with pytest.raises(VirtxError) as refused:
        await client.get_snapshot(name)
    assert refused.value.status == 404


async def test_a_sandbox_s_files_are_its_own_and_go_with_it(client: Client) -> None:
    await alpine(client)
    sandbox = await client.create_sandbox(ALPINE)
    await sandbox.create_dir_all("work/dir")
    await sandbox.write_file("work/dir/a.txt", "hello")
    assert await sandbox.read_text("work/dir/a.txt") == "hello"
    assert await sandbox.read_file("work/dir/a.txt", 1, 3) == b"ell"
    got = await client.get_sandbox(sandbox.id)
    assert [entry.name for entry in await got.read_dir("work/dir")] == ["a.txt"]
    await sandbox.remove_file("work/dir/a.txt")
    assert not await got.exists("work/dir/a.txt")
    await sandbox.remove()
    with pytest.raises(VirtxError) as refused:
        await client.get_sandbox(sandbox.id)
    assert refused.value.status == 404
    with pytest.raises(VirtxError) as refused:
        await sandbox.read_file("")
    assert refused.value.status == 404


def test_metadata_reads_as_the_wire_has_it() -> None:
    assert Metadata.from_dict({"is_dir": True, "size": 0}) == Metadata(True, 0)
    entry = DirEntry.from_dict(
        {"name": "a", "metadata": {"is_dir": False, "size": 1, "modified": 2}}
    )
    assert entry == DirEntry("a", Metadata(False, 1, 2))
