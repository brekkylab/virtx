insert into item (path, title, body, mtime, len, written_at)
values (?1, ?2, ?3, ?4, ?5, ?6)
on conflict(path) do update set
    title      = excluded.title,
    body       = excluded.body,
    mtime      = excluded.mtime,
    len        = excluded.len,
    written_at = excluded.written_at
returning rowid;
