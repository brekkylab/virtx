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

        metadata = await client.stat_file(volume, "dir/a.txt")
        assert isinstance(metadata, Metadata)
        assert (metadata.is_dir, metadata.size) == (False, 11)
        entries = await client.read_dir(volume, "dir")
        assert entries == [DirEntry("a.txt", metadata)]

        assert await client.read_file(volume, "dir/a.txt") == b"hello world"
        assert await client.read_file(volume, "dir/a.txt", offset=6, length=3) == b"wor"

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
