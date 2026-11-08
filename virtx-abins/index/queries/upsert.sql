insert into item (path, title, mtime, len, written_at)
values (?1, ?2, ?3, ?4, ?5)
on conflict(path) do update set
    title      = excluded.title,
    mtime      = excluded.mtime,
    len        = excluded.len,
    written_at = excluded.written_at
returning rowid;
