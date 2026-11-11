//! The store: one SQLite file holding what a store knows about itself, a row per indexed
//! file, the chunks each is cut into, and the full-text index over those.
//!
//! # One file, and why that decides most of this
//!
//! A store is named the way a document is — `index ingest notes.db docs` — because it lives
//! beside what it indexes. Everything a search needs is therefore in that one file, and copying
//! the file copies everything that was indexed.
//!
//! Being an ordinary file is what SQLite makes possible. A store on the rollback journal needs
//! nothing of its host but the ability to create a sibling file: no `mmap` of its own data, no
//! lock file held across a session. That is what lets one sit on a directory this has no say
//! over — a network share, or a FUSE mount, where an engine wanting either of those would
//! quietly misbehave.
//!
//! # No write-ahead log
//!
//! A store may well sit on a FUSE mount, and WAL mode does not survive that assumption: the
//! log's index is a shared-memory file that every connection `mmap`s and expects to be coherent
//! between processes. A FUSE filesystem is under no obligation to give them that, and the
//! failure is silent. So the store stays on the rollback journal SQLite starts in, and
//! concurrent writers wait on [`sqlite::BUSY_TIMEOUT`].
//!
//! # A search answers with chunks
//!
//! `item` holds what a file *is* — its path, its name, the stamp it was read under, and its body
//! whole. `chunk` holds the spans [`chunk::split`] cut that body into, and `chunk_fts` the text
//! of each under the chunk's rowid, so a hit is a passage with the lines it spans rather than a
//! whole file. The text is in `chunk_fts` itself and not an external-content table, because
//! `snippet()` needs it there to cut around a match. The body is kept whole beside it so a store
//! can be cut again under another [`Chunking`] without reading a file — see [`Store::rechunk`].

use std::{io, path::Path, sync::Mutex};

use rusqlite::Connection;

use crate::{
    chunk::{self, Chunking},
    ingest::Stamp,
    sqlite::{self, sql_error},
};

/// Where a store keeps its [`Chunking`] in `meta`.
const CHUNK_SIZE_KEY: &str = "chunk_size";
const CHUNK_OVERLAP_KEY: &str = "chunk_overlap";

/// Every document — `queries/held.sql`. The whole set and not a filtered slice, because the
/// filter is [`under`](crate::ingest::under) and it is component-wise: `notes-other/a.md` is not under
/// `notes`, where a SQL `like 'notes%'` would say it was. Doing the prefix test in SQL would
/// mean either escaping `%` and `_` out of every path or keeping a second spelling of the
/// rule, and this reads three small columns with no body among them.
const HELD: &str = include_str!("../queries/held.sql");

/// One document removed — `queries/forget.sql`. The file it was built from is untouched.
const FORGET: &str = include_str!("../queries/forget.sql");

/// Every document's body — `queries/bodies.sql`. What [`Store::rechunk`] cuts again.
const BODIES: &str = include_str!("../queries/bodies.sql");

/// The chunks nearest a query, nearest first — `queries/search.sql`.
///
/// `bm25()` ascending is best first: FTS5 scores a better match as a more negative number. It
/// is negated on the way out so that the column a caller reads means what a score column
/// usually means: larger is better. A ranking number that runs the other way is the kind of
/// thing a reader gets wrong once and never checks again.
///
/// The rowid after it is a tiebreak and not a second opinion about nearness: two chunks the
/// ranking cannot separate would otherwise come back in whatever order the index walked them,
/// and a search that answers the same question two ways is one nobody can script against.
const SEARCH: &str = include_str!("../queries/search.sql");

/// A document on its way into the store.
pub(crate) struct Writing<'a> {
    /// What it is filed under, which is the path the caller named it by.
    pub path: &'a Path,
    pub title: String,
    pub body: String,
    pub stamp: Stamp,
}

/// A document the store holds.
pub(crate) struct Held {
    pub rowid: i64,
    pub path: String,
    pub stamp: Stamp,
}

/// One answer: which chunk of which document, how well it matched, and its text.
pub(crate) struct Hit {
    pub path: String,
    pub title: String,
    /// The lines of the document the chunk spans, 1-based and inclusive.
    pub lines: (u64, u64),
    pub score: f64,
    pub snippet: String,
    pub text: String,
}

/// An open store.
///
/// Every method here is synchronous and blocks — this is SQLite. Nothing here hides that: the
/// program a store is opened by is doing this one thing and has nothing else to get on with.
#[derive(Debug)]
pub(crate) struct Store {
    conn: Mutex<Connection>,
    chunking: Chunking,
}

impl Store {
    /// Make the store at `path`, cutting what it is given by `chunking`.
    ///
    /// [`AlreadyExists`](io::ErrorKind::AlreadyExists) when there is already a file there, and
    /// nothing half-made left behind when the schema will not install — see
    /// [`sqlite::create`], which is where both of those rules live.
    pub fn try_new(path: impl AsRef<Path>, chunking: Chunking) -> io::Result<Store> {
        let settings = [
            (CHUNK_SIZE_KEY, chunking.size.to_string()),
            (CHUNK_OVERLAP_KEY, chunking.overlap.to_string()),
        ];
        Ok(Store {
            conn: Mutex::new(sqlite::create(path.as_ref(), &settings)?),
            chunking,
        })
    }

    /// Open the store that is already at `path`.
    ///
    /// [`NotFound`](io::ErrorKind::NotFound) for no file, [`InvalidData`](io::ErrorKind::InvalidData)
    /// for a file that is not one of these — see [`sqlite::open`] — or one whose chunking is
    /// missing or makes no sense.
    pub fn try_from_file(path: impl AsRef<Path>) -> io::Result<Store> {
        let conn = sqlite::open(path.as_ref())?;
        let invalid = |msg: String| io::Error::new(io::ErrorKind::InvalidData, msg);
        let number = |key: &str| -> io::Result<usize> {
            let value = sqlite::said(&conn, key)?
                .ok_or_else(|| invalid(format!("the store does not say its {key}")))?;
            value
                .parse()
                .map_err(|_| invalid(format!("the store's {key} is {value:?}, not a number")))
        };
        let chunking =
            Chunking::new(number(CHUNK_SIZE_KEY)?, number(CHUNK_OVERLAP_KEY)?).map_err(invalid)?;
        Ok(Store {
            conn: Mutex::new(conn),
            chunking,
        })
    }

    /// Whether `path` is a store — what `list` sifts a directory with, and what `drop` asks
    /// before it deletes anything. See [`sqlite::is_store`].
    pub fn looks_like_one(path: &Path) -> bool {
        sqlite::is_store(path)
    }

    /// How many documents it holds — what `list` reports.
    pub fn count(&self) -> io::Result<u64> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        // `count(*)` is an `i64` in SQLite and is never negative, so the widening is safe.
        conn.query_row("select count(*) from item", [], |row| row.get::<_, i64>(0))
            .map(|n| n as u64)
            .map_err(sql_error)
    }

    /// Write these documents and forget those, in one transaction.
    ///
    /// **One call and not two, because `sync` does both** and a store between them is a tree
    /// that half-existed: a reader would see the new bytes of one file beside a document for a
    /// file that is gone.
    ///
    /// All of them or none, for the same reason at a smaller scale. What produced a batch is
    /// one walk of one set of arguments, and a store left holding half of it is one nobody can
    /// tell from a store that was given half — the report would say what was written, and the
    /// next `sync` would decide what to re-read from a state that never was.
    ///
    /// The written documents share a `written_at`: what is being timestamped is the call, and a
    /// batch written in one transaction happened at one moment.
    pub fn apply(&self, writing: &[Writing<'_>], forgetting: &[i64]) -> io::Result<()> {
        let written_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction().map_err(sql_error)?;
        {
            let mut upsert = tx.prepare(sqlite::UPSERT).map_err(sql_error)?;
            let mut chunks = Chunks::prepare(&tx)?;
            let mut forget = tx.prepare(FORGET).map_err(sql_error)?;

            for doc in writing {
                let path = doc.path.to_string_lossy();
                let rowid: i64 = upsert
                    .query_row(
                        (
                            path.as_ref(),
                            &doc.title,
                            &doc.body,
                            // SQLite counts in `i64` and a `Stamp` is `u64`, so the two
                            // wide halves are swapped here and swapped back on the way out.
                            // The round trip is exact — it is the same bits read two ways —
                            // which is all `unchanged_from` needs, since it only ever compares
                            // a stamp to another stamp.
                            doc.stamp.mtime as i64,
                            doc.stamp.len as i64,
                            &written_at,
                        ),
                        |row| row.get(0),
                    )
                    .map_err(sql_error)?;
                chunks.clear(rowid)?;
                chunks.write(rowid, &doc.title, &doc.body, self.chunking)?;
            }

            for rowid in forgetting {
                // The text by hand, since nothing cascades into a virtual table; `chunk` does
                // cascade — `foreign_keys` is on, see `connect`.
                chunks.clear(*rowid)?;
                forget.execute((rowid,)).map_err(sql_error)?;
            }
        }
        tx.commit().map_err(sql_error)
    }

    /// Cut every document again by `chunking`, and keep it as this store's from now on.
    ///
    /// From the bodies the store already holds, so no file is read and a document whose file is
    /// gone keeps its place. One transaction: a store half cut one way and half the other would
    /// rank passages of two sizes against each other.
    pub fn rechunk(&mut self, chunking: Chunking) -> io::Result<u64> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction().map_err(sql_error)?;
        let mut done = 0;
        {
            let mut chunks = Chunks::prepare(&tx)?;
            let mut bodies = tx.prepare(BODIES).map_err(sql_error)?;
            let rows = bodies
                .query_map([], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .map_err(sql_error)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(sql_error)?;
            for (rowid, title, body) in rows {
                chunks.clear(rowid)?;
                chunks.write(rowid, &title, &body, chunking)?;
                done += 1;
            }
            sqlite::set(&tx, CHUNK_SIZE_KEY, &chunking.size.to_string())?;
            sqlite::set(&tx, CHUNK_OVERLAP_KEY, &chunking.overlap.to_string())?;
        }
        tx.commit().map_err(sql_error)?;
        drop(conn);
        self.chunking = chunking;
        Ok(done)
    }

    /// Every document the store holds, with the stamp it was written under.
    ///
    /// The whole table — see [`HELD`] for why the prefix test is not asked of SQL.
    pub fn held(&self) -> io::Result<Vec<Held>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(HELD).map_err(sql_error)?;
        let rows = stmt
            .query_map([], |row| {
                Ok(Held {
                    rowid: row.get(0)?,
                    path: row.get(1)?,
                    stamp: Stamp {
                        mtime: row.get::<_, i64>(2)? as u64,
                        len: row.get::<_, i64>(3)? as u64,
                    },
                })
            })
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
    }

    /// The chunks nearest `query`, nearest first, and at most `limit` of them.
    ///
    /// `query` is already an FTS5 expression — [`as_expression`] is what turns what a caller
    /// typed into one.
    pub fn search(&self, query: &str, limit: usize) -> io::Result<Vec<Hit>> {
        // SQLite counts in `i64`, and a `usize` that will not fit one is a caller asking for
        // more documents than a store could hold — which `i64::MAX` is also the answer to.
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);

        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(SEARCH).map_err(sql_error)?;
        let rows = stmt
            .query_map((query, limit), |row| {
                Ok(Hit {
                    path: row.get(0)?,
                    title: row.get(1)?,
                    lines: (row.get::<_, i64>(2)? as u64, row.get::<_, i64>(3)? as u64),
                    score: row.get(4)?,
                    snippet: row.get(5)?,
                    text: row.get(6)?,
                })
            })
            .map_err(sql_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
    }
}

/// What a caller typed, as something FTS5 will accept.
///
/// **Every word is quoted, and that is the whole of the escaping.** FTS5 reads a pattern as an
/// expression in a small language of its own — `AND`, `OR`, `NOT`, `NEAR`, `*`, `:`, `-` and
/// parentheses all mean something in it — and a query written by an agent that was never told
/// that language would otherwise be a syntax error rather than a search. Quoting says "these
/// are words"; doubling is FTS5's own escape for a quote inside a quoted string.
///
/// Joined with `OR` and not `AND`: every term is a hard
/// requirement under `AND`, so a question asked in a sentence — the way a person asks one —
/// would answer nothing at all the moment one of its words was absent. Under `OR` the document
/// holding most of them is simply the top line, and `bm25` does the work the filter was doing
/// badly. The cost is a long tail, which the limit makes a non-issue.
///
/// Once each, because `bm25` adds up a contribution per term of the pattern: a word repeated in
/// the question would otherwise count twice and outweigh what the question is about.
///
/// An empty answer for a query with no words in it — punctuation, whitespace, nothing at all.
/// There is no question there for the index to answer, and saying so is truer than refusing.
pub(crate) fn as_expression(query: &str) -> Option<String> {
    let mut seen = std::collections::HashSet::new();
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|word| seen.insert(*word))
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" OR "))
}

/// The statements that write and clear a document's chunks, prepared once per transaction.
struct Chunks<'c> {
    write: rusqlite::Statement<'c>,
    write_text: rusqlite::Statement<'c>,
    clear_text: rusqlite::Statement<'c>,
    clear: rusqlite::Statement<'c>,
}

impl<'c> Chunks<'c> {
    fn prepare(conn: &'c Connection) -> io::Result<Chunks<'c>> {
        let prepare = |sql| conn.prepare(sql).map_err(sql_error);
        Ok(Chunks {
            write: prepare(sqlite::CHUNK_WRITE)?,
            write_text: prepare(sqlite::CHUNK_FTS_WRITE)?,
            clear_text: prepare(sqlite::CHUNK_FTS_CLEAR)?,
            clear: prepare(sqlite::CHUNK_CLEAR)?,
        })
    }

    /// Every chunk of the document at `item`, gone.
    fn clear(&mut self, item: i64) -> io::Result<()> {
        self.clear_text.execute((item,)).map_err(sql_error)?;
        self.clear.execute((item,)).map_err(sql_error)?;
        Ok(())
    }

    /// `body` cut by `chunking`, each chunk filed under `item` with its text searchable.
    fn write(&mut self, item: i64, title: &str, body: &str, chunking: Chunking) -> io::Result<()> {
        for (ordinal, span) in chunk::split(body, chunking).into_iter().enumerate() {
            let rowid: i64 = self
                .write
                .query_row(
                    (
                        item,
                        ordinal as i64,
                        span.bytes.start as i64,
                        span.bytes.end as i64,
                        span.lines.0 as i64,
                        span.lines.1 as i64,
                    ),
                    |row| row.get(0),
                )
                .map_err(sql_error)?;
            self.write_text
                .execute((rowid, title, &body[span.bytes]))
                .map_err(sql_error)?;
        }
        Ok(())
    }
}
