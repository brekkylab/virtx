//! Cutting a document's body into the chunks a search answers with.
//!
//! A chunk is a span of the body, at most [`Chunking::size`] characters long, and the next one
//! starts no more than [`Chunking::overlap`] characters before it ends. Spans are cut where the
//! text already breaks, in this order of preference: before a Markdown heading, at a blank line,
//! at a line end, at whitespace, and only then mid-word. A heading also closes a chunk that is
//! already half full, so a section tends to start a chunk rather than trail off the end of one.
//!
//! Sizes count characters, not bytes and not tokens: there is no tokenizer here to count by,
//! and a byte count would cut a chunk of Korean to a third of a chunk of English.

use std::ops::Range;

/// How a store cuts its documents, as `init` was told and `meta` keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Chunking {
    pub size: usize,
    pub overlap: usize,
}

impl Chunking {
    pub const DEFAULT_SIZE: usize = 1200;
    pub const DEFAULT_OVERLAP: usize = 200;

    /// A chunking that can make progress: a chunk has room for something, and the overlap
    /// leaves each chunk something the one before did not have.
    pub fn new(size: usize, overlap: usize) -> Result<Chunking, String> {
        if size == 0 {
            return Err("a chunk size is at least 1".into());
        }
        if overlap >= size {
            return Err(format!(
                "the overlap ({overlap}) has to be smaller than the chunk size ({size})"
            ));
        }
        Ok(Chunking { size, overlap })
    }
}

/// One chunk: where it is in the body, in bytes and in lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub bytes: Range<usize>,
    /// 1-based and inclusive, as an editor or `sed -n` counts them.
    pub lines: (usize, usize),
}

/// The chunks of `body`, in order. An empty or blank body has one empty chunk, so that a
/// document is still found by its title.
pub(crate) fn split(body: &str, chunking: Chunking) -> Vec<Span> {
    let units = units(body, chunking.size);
    if units.is_empty() {
        return vec![Span {
            bytes: 0..0,
            lines: (1, 1),
        }];
    }

    // Whether the unit after `last` joins the chunk of `first` through `last`: it fits, and is
    // not a heading, which starts a chunk of its own once this one has half of what it may hold.
    let joins = |first: usize, last: usize| {
        let next = &units[last + 1];
        chars(body, units[first].range.start..next.range.end) <= chunking.size
            && !(next.heading
                && chars(body, units[first].range.start..units[last].range.end)
                    >= chunking.size / 2)
    };

    let mut spans = Vec::new();
    let mut first = 0;
    loop {
        // As many units as fit, and always at least one: no unit is longer than a chunk.
        let mut last = first;
        while last + 1 < units.len() && joins(first, last) {
            last += 1;
        }

        let bytes = units[first].range.start..units[last].range.end;
        spans.push(Span {
            lines: lines(body, &bytes),
            bytes: bytes.clone(),
        });
        if last + 1 == units.len() {
            return spans;
        }

        // The next chunk starts at the earliest unit after this one's first that lies within
        // the overlap of its end and still lets it reach past this one; with none, at the unit
        // after its last. One that could not reach past would be a chunk inside this one.
        first = (first + 1..=last)
            .find(|&k| {
                chars(body, units[k].range.start..bytes.end) <= chunking.overlap && joins(k, last)
            })
            .unwrap_or(last + 1);
    }
}

/// A run of text a chunk boundary may fall on either side of, and whether it is a heading.
struct Unit {
    range: Range<usize>,
    heading: bool,
}

/// The body as units no longer than `size`: paragraphs, then lines of a paragraph too long to
/// keep whole, then pieces of a line too long to keep whole. Blank lines are not part of any
/// unit, so a chunk never starts or ends on one.
fn units(body: &str, size: usize) -> Vec<Unit> {
    let mut out = Vec::new();
    for block in blocks(body) {
        let heading = body[block.clone()].starts_with('#');
        if chars(body, block.clone()) <= size {
            out.push(Unit {
                range: block,
                heading,
            });
            continue;
        }
        for (i, line) in line_ranges(body, block).into_iter().enumerate() {
            let heading = heading && i == 0;
            for piece in pieces(body, line, size) {
                out.push(Unit {
                    range: piece,
                    heading,
                });
            }
        }
    }
    out
}

/// The paragraphs of `body`: runs of non-blank lines, with a Markdown heading always starting
/// one of its own. Each range ends at the end of its last line, without the newline.
fn blocks(body: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut open: Option<Range<usize>> = None;
    for line in line_ranges(body, 0..body.len()) {
        let text = &body[line.clone()];
        if text.trim().is_empty() {
            out.extend(open.take());
            continue;
        }
        if text.starts_with('#') {
            out.extend(open.take());
        }
        open = Some(match open {
            Some(block) => block.start..line.end,
            None => line,
        });
    }
    out.extend(open);
    out
}

/// The lines within `range`, each without its newline (or `\r\n`).
fn line_ranges(body: &str, range: Range<usize>) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = range.start;
    for (at, _) in body[range.clone()].match_indices('\n') {
        let end = range.start + at;
        let end = if end > start && body[..end].ends_with('\r') {
            end - 1
        } else {
            end
        };
        out.push(start..end);
        start = range.start + at + 1;
    }
    if start < range.end {
        out.push(start..range.end);
    }
    out
}

/// `line` in pieces of at most `size` characters, cut after the last whitespace that fits and
/// mid-word only where a piece has none. Whitespace at a cut belongs to neither side.
fn pieces(body: &str, line: Range<usize>, size: usize) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = line.start;
    while start < line.end {
        let rest = &body[start..line.end];
        let Some((limit, next)) = rest.char_indices().nth(size) else {
            out.push(start..line.end);
            break;
        };
        // Whitespace right after the piece is a word ending exactly where it may.
        let cut = match next.is_whitespace() {
            true => limit,
            false => rest[..limit]
                .rfind(char::is_whitespace)
                .filter(|&at| at > 0)
                .unwrap_or(limit),
        };
        out.push(start..start + cut);
        let skipped = rest[cut..].len() - rest[cut..].trim_start().len();
        start += cut + skipped;
    }
    out
}

fn chars(body: &str, range: Range<usize>) -> usize {
    body[range].chars().count()
}

/// The first and last line `bytes` touches, 1-based. A chunk never ends on a newline, so the
/// line its end is on is its last.
fn lines(body: &str, bytes: &Range<usize>) -> (usize, usize) {
    let line_at = |at: usize| body[..at].matches('\n').count() + 1;
    (line_at(bytes.start), line_at(bytes.end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(body: &str, size: usize, overlap: usize) -> Vec<&str> {
        split(body, Chunking::new(size, overlap).unwrap())
            .into_iter()
            .map(|span| &body[span.bytes])
            .collect()
    }

    #[test]
    fn a_short_document_is_one_chunk() {
        let body = "# Title\n\nOne paragraph.\n\nAnother.\n";
        let spans = split(body, Chunking::new(100, 10).unwrap());
        assert_eq!(spans.len(), 1);
        assert_eq!(&body[spans[0].bytes.clone()], body.trim_end());
        assert_eq!(spans[0].lines, (1, 5));
    }

    #[test]
    fn an_empty_document_is_one_empty_chunk() {
        for body in ["", "\n\n  \n"] {
            assert_eq!(
                split(body, Chunking::new(10, 0).unwrap()),
                [Span {
                    bytes: 0..0,
                    lines: (1, 1)
                }]
            );
        }
    }

    #[test]
    fn paragraphs_are_kept_whole_where_they_fit() {
        let body = "aaaa aaaa\n\nbbbb bbbb\n\ncccc cccc\n";
        assert_eq!(texts(body, 20, 0), ["aaaa aaaa\n\nbbbb bbbb", "cccc cccc"]);
    }

    #[test]
    fn the_overlap_starts_no_chunk_that_cannot_reach_past_the_last() {
        let body = "aaaaa\n\nb\n\ncccccccccc\n";
        assert_eq!(texts(body, 12, 2), ["aaaaa\n\nb", "cccccccccc"]);
    }

    #[test]
    fn the_overlap_repeats_whole_units_that_fit_in_it() {
        let body = "aaaa\n\nbbbb\n\ncccc\n\ndddd\n";
        assert_eq!(
            texts(body, 12, 4),
            ["aaaa\n\nbbbb", "bbbb\n\ncccc", "cccc\n\ndddd"]
        );
        assert_eq!(
            texts(body, 12, 3),
            ["aaaa\n\nbbbb", "cccc\n\ndddd"],
            "a unit longer than the overlap is not repeated"
        );
    }

    #[test]
    fn a_heading_starts_a_chunk_once_the_one_before_is_half_full() {
        let body = "intro text here\n\n# Section\n\nbody\n";
        assert_eq!(texts(body, 28, 0), ["intro text here", "# Section\n\nbody"]);
        assert_eq!(
            texts(body, 100, 0),
            [body.trim_end()],
            "under half full, the heading joins it"
        );
    }

    #[test]
    fn a_long_paragraph_is_cut_at_lines_then_at_whitespace() {
        let body = "one two three\nfour five six seven eight\n";
        assert_eq!(
            texts(body, 14, 0),
            ["one two three", "four five six", "seven eight"]
        );
        assert_eq!(texts("abcdefghij", 4, 0), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn sizes_count_characters_and_cuts_fall_on_them() {
        let body = "가나다라 마바사아 자차카타";
        assert_eq!(texts(body, 9, 0), ["가나다라 마바사아", "자차카타"]);
    }

    #[test]
    fn lines_are_counted_from_one_and_include_the_last() {
        let body = "a\n\nb\nc\n\nd\n";
        let spans = split(body, Chunking::new(3, 0).unwrap());
        let lines: Vec<_> = spans.iter().map(|s| s.lines).collect();
        assert_eq!(lines, [(1, 1), (3, 4), (6, 6)]);
    }

    #[test]
    fn crlf_is_not_part_of_a_chunk() {
        assert_eq!(texts("a\r\n\r\nb\r\n", 1, 0), ["a", "b"]);
    }

    #[test]
    fn a_chunking_that_cannot_progress_is_refused() {
        assert!(Chunking::new(0, 0).is_err());
        assert!(Chunking::new(10, 10).is_err());
        assert!(Chunking::new(10, 9).is_ok());
    }
}
