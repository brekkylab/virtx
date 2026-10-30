"""The files of a volume, against ``virtx-core``'s ``files_server`` example over stdio."""

import pytest

from virtx import Client, DirEntry, Metadata, Snapshot, VirtxError, Volume


async def test_the_files_of_a_volume_are_written_listed_moved_and_read_back(
    files_server: str,
) -> None:
    volume = Volume("v")
    with Client.spawn(files_server) as client:
        await client.create_dir(volume, "dir")
        await client.write_file(volume, "dir/a.txt", b"hello world")
        await client.create_dir_all(volume, "deep/er/dir")
        await client.create_dir_all(volume, "deep/er/dir")
        assert (await client.stat_file(volume, "deep/er/dir")).is_dir

        metadata = await client.stat_file(volume, "dir/a.txt")
        assert isinstance(metadata, Metadata)
        assert (metadata.is_dir, metadata.size) == (False, 11)
        entries = await client.read_dir(volume, "dir")
        assert entries == [DirEntry("a.txt", metadata)]

        assert await client.read_file(volume, "dir/a.txt") == b"hello world"
        assert await client.read_file(volume, "dir/a.txt", offset=6, length=3) == b"wor"

        # Text, an etag to write over, and what is there, told without an error.
        await client.write_file(volume, "dir/t.txt", "텍스트")
        assert await client.read_text(volume, "dir/t.txt") == "텍스트"
        etag = (await client.stat_file(volume, "dir/t.txt")).etag
        assert etag
        await client.write_file(volume, "dir/t.txt", "text 2", if_match=etag)
        with pytest.raises(VirtxError, match="412"):
            await client.write_file(volume, "dir/t.txt", "text 3", if_match=etag)
        await client.write_file(volume, "dir/bin", b"\xff\xfe")
        with pytest.raises(VirtxError, match="UTF-8"):
            await client.read_text(volume, "dir/bin")
        assert await client.exists(volume, "dir/t.txt")
        assert await client.is_file(volume, "dir/t.txt")
        assert not await client.is_dir(volume, "dir/t.txt")
        assert await client.is_dir(volume, "dir")
        await client.remove_file(volume, "dir/t.txt")
        await client.remove_file(volume, "dir/bin")
        assert not await client.exists(volume, "dir/t.txt")
        with pytest.raises(VirtxError, match="404"):
            await client.remove_file(volume, "dir/t.txt")
        await client.remove_file(volume, "dir/t.txt", missing_ok=True)

        await client.rename_file(volume, "dir/a.txt", "b.txt")
        await client.copy_file(volume, "b.txt", "c.txt")
        with pytest.raises(VirtxError, match="412"):
            await client.copy_file(volume, "b.txt", "c.txt")
        await client.remove_file(volume, "dir")
        with pytest.raises(VirtxError, match="404"):
            await client.stat_file(volume, "dir")
        assert await client.read_file(volume, "c.txt") == b"hello world"

        with pytest.raises(VirtxError, match="423"):
            await client.read_dir(Snapshot("locked"), "")


def test_a_root_and_metadata_read_as_the_wire_has_them() -> None:
    assert Volume("v").to_dict() == {"volume": "v"}
    assert Snapshot("s").to_dict() == {"snapshot": "s"}
    assert Metadata.from_dict({"is_dir": True, "size": 0}) == Metadata(True, 0)
    entry = DirEntry.from_dict({"name": "a", "metadata": {"is_dir": False, "size": 1, "modified": 2}})
    assert entry == DirEntry("a", Metadata(False, 1, 2))
