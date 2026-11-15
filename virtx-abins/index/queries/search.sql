select item.path,
       item.title,
       chunk.start_line,
       chunk.end_line,
       -bm25(chunk_fts),
       snippet(chunk_fts, 1, '', '', '…', 32),
       chunk_fts.text
from chunk_fts
join chunk on chunk.rowid = chunk_fts.rowid
join item on item.rowid = chunk.item_id
where chunk_fts match ?1
order by bm25(chunk_fts), chunk.rowid
limit ?2;
