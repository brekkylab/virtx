//! A PDF as lines, made by [`pdf_oxide`], which is in every build of this program: there is no
//! build of `docread` that meets a PDF and refuses it for want of an engine.
//!
//! # Whose lines these are
//!
//! A PDF has no lines. It has pages, and on each page a set of glyphs at coordinates; what
//! comes out of [`pdf_oxide`] is those glyphs put back into reading order by an
//! engine making its best guess at the layout. So the numbers this program prints in front of
//! extracted text are **the extraction's numbers and not the document's**, and two engines —
//! or two versions of this one — can disagree about them.
//!
//! Which is why the answer says so on its own line rather than looking exactly like a text
//! file's. The whole promise of this program is that a line number can be quoted back and mean
//! the same thing to whoever reads it next; here that promise is weaker, and hiding that would
//! be worse than not extracting at all.
//!
//! Page breaks are kept as `[page N]` lines for the same reason. A page is the one address a
//! PDF really has — it is what a person looking at the document can find — so it stays in the
//! text, even though it costs a line number of its own.
//!
//! # The whole document is extracted, then windowed
//!
//! `-o` and `-n` cut the text after the engine has laid it out, because there is nothing to
//! cut before: line 400 of a PDF is not knowable without laying out every page ahead of it.
//! A window is therefore cheap in what it prints and not in what it costs, which is the
//! opposite of the file case and worth knowing before pointing this at a thousand pages.

use std::{io::Cursor, path::Path};

use pdf_oxide::PdfDocument;

use crate::text::{self, Window};

/// The first bytes every PDF starts with.
pub const MAGIC: &[u8] = b"%PDF-";

/// What the answer calls these lines.
///
/// Named rather than left for a caller to infer, and named as the engine's: two versions of
/// the engine can put the same page into a different order, so a number quoted out of this is
/// worth what the engine is worth and no more.
pub const ORIGIN: &str = "pdf_oxide's reading of this PDF's pages, not lines in the file";

/// `limit` lines of `path`'s text from line `offset`, as the engine laid it out.
pub fn read(path: &Path, offset: usize, limit: usize) -> std::io::Result<Window> {
    let extracted = extract(path)?;

    // Through the same window every other read goes through, so that a cut line, a number and
    // the count at the end mean what they mean everywhere else. `None` for the format: what
    // is being read at this point is text, and a refusal about its bytes would be about the
    // extraction rather than about the PDF.
    text::window(Cursor::new(extracted.into_bytes()), offset, limit, None)
}

/// Every page's text, each under a `[page N]` line.
///
/// A panic in the engine is caught and answered as a refusal about this file: the input is
/// somebody's PDF, and a parser meeting one it was not written for is a document this cannot
/// read, not a crash for the caller to make sense of.
fn extract(path: &Path) -> std::io::Result<String> {
    let path = path.to_path_buf();
    let run = move || -> Result<String, String> {
        let document = PdfDocument::open(&path).map_err(|e| e.to_string())?;
        let pages = document.page_count().map_err(|e| e.to_string())?;
        let mut extracted = String::new();
        for index in 0..pages {
            extracted.push_str(&format!("[page {}]\n", index + 1));
            let page = document.extract_text(index).map_err(|e| e.to_string())?;
            // The engine pads lines to the width it laid the page out at, so a blank line comes
            // out as a run of spaces. What is at the end of a line is not text anybody wrote.
            for line in page.trim_end().lines() {
                extracted.push_str(line.trim_end());
                extracted.push('\n');
            }
        }
        Ok(extracted)
    };

    // The default hook would print the panic to stderr before this could say what it means.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(run);
    std::panic::set_hook(hook);

    match caught {
        Ok(Ok(extracted)) => Ok(extracted),
        Ok(Err(e)) => Err(unreadable(e)),
        Err(panic) => {
            let said = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "the PDF engine gave up on it".into());
            Err(unreadable(said))
        }
    }
}

/// What the engine said, as a refusal about this file.
///
/// [`ErrorKind::InvalidData`](std::io::ErrorKind::InvalidData) for all of it: by the time the
/// engine is answering, the file was found and opened, and what is left is the document being
/// one this cannot read — encrypted, truncated, or not really a PDF behind its first bytes.
fn unreadable(said: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, said)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A PDF written by the same crate that reads it, so that no `.pdf` is checked in.
    fn pdf(dir: &tempfile::TempDir, name: &str, text: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        pdf_oxide::api::Pdf::from_text(text)
            .expect("a PDF")
            .save(&path)
            .expect("written");
        path
    }

    #[test]
    fn a_page_is_headed_and_its_lines_follow() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = pdf(&dir, "a.pdf", "First line here.\nSecond line here.");
        let window = read(&path, 1, 10).expect("a PDF is read");
        let lines: Vec<&str> = window
            .shown
            .lines()
            .map(|l| l.split_once('\t').map_or(l, |(_, text)| text))
            .collect();
        assert_eq!(lines[0], "[page 1]", "{}", window.shown);
        assert!(lines.contains(&"First line here."), "{}", window.shown);
        assert!(lines.contains(&"Second line here."), "{}", window.shown);
    }

    #[test]
    fn no_line_ends_in_padding() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = pdf(&dir, "a.pdf", "One.\n\nTwo after a blank line.");
        let window = read(&path, 1, 10).expect("a PDF is read");
        for line in window.shown.lines() {
            let (_, text) = line.split_once('\t').expect("a numbered line");
            assert_eq!(text, text.trim_end(), "{:?}", window.shown);
        }
    }

    /// A file that starts like a PDF and is not one is refused, not answered with nothing.
    #[test]
    fn a_broken_pdf_is_refused() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("junk.pdf");
        std::fs::write(&path, b"%PDF-1.4\ngarbage").unwrap();
        let e = read(&path, 1, 10).expect_err("not a readable PDF");
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);
    }
}
