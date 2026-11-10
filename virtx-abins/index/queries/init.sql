create table meta (key text primary key, value text not null);

create table item (
    rowid      integer primary key,
    path       text not null unique,
    title      text not null default '',
    body       text not null,
    mtime      integer not null default 0,
    len        integer not null default 0,
    written_at text not null
);

create table chunk (
    rowid       integer primary key,
    item_id     integer not null references item(rowid) on delete cascade,
    ordinal     integer not null,
    start_byte  integer not null,
    end_byte    integer not null,
    start_line  integer not null,
    end_line    integer not null,
    embedding   blob,
    unique(item_id, ordinal)
);

create index chunk_of_item on chunk(item_id);

create virtual table chunk_fts using fts5(
    title,
    text,
    tokenize='porter unicode61 remove_diacritics 2'
);
