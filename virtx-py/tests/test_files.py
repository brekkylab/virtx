"""The files of a volume, against ``virtx-core``'s ``test_server`` example over stdio."""

import pytest

from virtx import (
    Client,
    DirEntry,
    GdriveSource,
    Metadata,
    MountType,
    VirtxError,
)


async def test_the_files_of_a_volume_are_written_listed_moved_and_read_back(
    test_server: str,
) -> None:
    with Client.spawn(test_server) as client:
        volume = await client.get_volume("v")
        await volume.create_dir("dir")
        await volume.write_file("dir/a.txt", b"hello world")
        await volume.create_dir_all("deep/er/dir")
        await volume.create_dir_all("deep/er/dir")
        assert (await volume.stat_file("deep/er/dir")).is_dir

        metadata = await volume.stat_file("dir/a.txt")
        assert isinstance(metadata, Metadata)
        assert (metadata.is_dir, metadata.size) == (False, 11)
        entries = await volume.read_dir("dir")
        assert entries == [DirEntry("a.txt", metadata)]

        assert await volume.read_file("dir/a.txt") == b"hello world"
        assert await volume.read_file("dir/a.txt", offset=6, length=3) == b"wor"

        # Text, an etag to write over, and what is there, told without an error.
        await volume.write_file("dir/t.txt", "텍스트")
        assert await volume.read_text("dir/t.txt") == "텍스트"
        etag = (await volume.stat_file("dir/t.txt")).etag
        assert etag
        await volume.write_file("dir/t.txt", "text 2", if_match=etag)
        with pytest.raises(VirtxError) as refused:
            await volume.write_file("dir/t.txt", "text 3", if_match=etag)
        assert refused.value.status == 412
        await volume.write_file("dir/bin", b"\xff\xfe")
        with pytest.raises(VirtxError, match="UTF-8"):
            await volume.read_text("dir/bin")
        assert await volume.exists("dir/t.txt")
        assert await volume.is_file("dir/t.txt")
        assert not await volume.is_dir("dir/t.txt")
        assert await volume.is_dir("dir")
        await volume.remove_file("dir/t.txt")
        await volume.remove_file("dir/bin")
        assert not await volume.exists("dir/t.txt")
        with pytest.raises(VirtxError) as refused:
            await volume.remove_file("dir/t.txt")
        assert refused.value.status == 404
        await volume.remove_file("dir/t.txt", missing_ok=True)

        await volume.rename_file("dir/a.txt", "b.txt")
        await volume.copy_file("b.txt", "c.txt")
        with pytest.raises(VirtxError) as refused:
            await volume.copy_file("b.txt", "c.txt")
        assert refused.value.status == 412
        await volume.remove_file("dir")
        with pytest.raises(VirtxError) as refused:
            await volume.stat_file("dir")
        assert refused.value.status == 404
        assert await (await client.get_volume("v")).read_file("c.txt") == b"hello world"


async def test_a_volume_carries_its_client_to_its_files_and_mounts(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        volume = await client.create_volume("w")
        assert (volume.name, volume.size) == ("w", 0)
        await volume.create_dir_all("in/dir")
        await volume.write_file("in/a.txt", "hello")
        await volume.rename_file("in/a.txt", "in/b.txt")
        await volume.copy_file("in/b.txt", "c.txt")
        assert await volume.read_text("c.txt") == "hello"
        assert await volume.is_dir("in/dir")
        assert sorted(entry.name for entry in await volume.read_dir("in")) == ["b.txt", "dir"]

        got = await client.get_volume("w")
        assert got == volume and got.client is client
        assert await got.read_text("in/b.txt") == "hello"
        assert volume in await client.list_volume()

        mount = await volume.mount("drive", GdriveSource("id", "secret", "token"))
        assert (mount.path, mount.mount_type) == ("drive", MountType.GDRIVE)
        assert await got.mounts() == [mount]
        await volume.unmount("drive")
        assert await got.mounts() == []

        await volume.remove()
        with pytest.raises(VirtxError) as refused:
            await client.get_volume("w")
        assert refused.value.status == 404


async def test_a_sandbox_s_files_are_its_own_and_go_with_it(test_server: str) -> None:
    with Client.spawn(test_server) as client:
        sandbox = await client.create_sandbox("alpine:3.20")
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
