insert into chunk (item_id, ordinal, start_byte, end_byte, start_line, end_line)
values (?1, ?2, ?3, ?4, ?5, ?6)
returning rowid;
