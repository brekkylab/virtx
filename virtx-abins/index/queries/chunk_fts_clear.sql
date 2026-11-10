delete from chunk_fts where rowid in (select rowid from chunk where item_id = ?1);
