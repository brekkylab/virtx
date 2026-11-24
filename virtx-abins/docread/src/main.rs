//! `docread` — documents parsed with docling, `docling convert`'s command line.
//!
//! ```text
//! docread report.pdf
//! docread report.pdf --to json --output out/
//! docread papers/ --from pdf --to md --to doclang --output out/
//! ```
//!
//! The options are `docling convert`'s, by the same names, for what this build does: the
//! formats docling reads, converted to Markdown (the default), docling's JSON, HTML, plain text or
//! DocLang. One document in one format is written to stdout; more need `--output`.
//!
//! A PDF is read through docling's models (`--pipeline standard`, the default), on the ONNX
//! Runtime `build.rs` links in: the layout model finds the headings, tables and pictures on a
//! page, and TableFormer a table's cells. The models, and the fonts a page is drawn in, are files
//! in `.models/` beside the program.
//! `--pipeline native` reads the text layer alone. There is no OCR, so a scanned PDF has no text.
//!
//! Source code is refused, being text already (a `.txt` docling reads as Markdown); so are
//! images, audio and video, which hold no text without OCR or speech recognition, and this build
//! has neither. Tracked changes are accepted unless `--track-changes` says otherwise.

use std::{
    collections::HashMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use clap::{Parser as _, ValueEnum};
use docling::{
    DoclingDocument, DocumentConverter, ImageMode, InputFormat, SourceDocument, TrackChanges,
};

/// The name every message this program writes about itself is spelled with.
const NAME: &str = env!("CARGO_BIN_NAME");

/// The most of a file this reads, in bytes. docling holds a document whole, and the model it
/// parses into is several times the file's size; on the guest a read past memory is the program
/// killed rather than an answer.
const MAX_FILE_BYTES: u64 = 128 << 20;

/// What `docread` accepts.
#[derive(Debug, clap::Parser)]
#[command(
    about = "Parse documents into text that keeps their headings, lists and tables",
    after_help = "Reads PDF, Word, PowerPoint, Excel, OpenDocument, RTF, HTML, Markdown, \
                  AsciiDoc, LaTeX, CSV, EPUB, email, HWP and more. Writes Markdown, JSON, HTML, \
                  text and DocLang.\n\n\
                  Uses docling inside, and its ML models where a file's structure has to be \
                  found, as in a PDF. There is no OCR: a scanned PDF has no text.\n\n\
                  Reading a PDF with the models takes up to about 2 GiB of memory, and a \
                  sandbox with less kills docread with exit code 137 and no message. \
                  --pipeline native needs far less.\n\n\
                  One document in one format goes to stdout; more need --output.",
    long_about = None,
    arg_required_else_help = true
)]
struct Cli {
    /// Files or directories to convert; a directory's documents are found by their extension
    #[arg(value_name = "SOURCE", required = true)]
    sources: Vec<PathBuf>,

    /// Input formats to accept, by docling's names (pdf, docx, pptx, xlsx, html, md, ...);
    /// defaults to all
    #[arg(long = "from", value_name = "FORMAT", value_parser = |text: &str| {
        InputFormat::from_id(text).ok_or_else(|| format!("docling has no input format `{text}`"))
    })]
    from: Vec<InputFormat>,

    /// Output formats
    #[arg(long = "to", value_enum, value_name = "FORMAT", default_values_t = [To::Md])]
    to: Vec<To>,

    /// Directory to write each document's outputs in, as <name>.<format>
    #[arg(long, value_name = "DIR", conflicts_with = "output_file")]
    output: Option<PathBuf>,

    /// File to write the output to, for one document in one format
    #[arg(long, value_name = "PATH")]
    output_file: Option<PathBuf>,

    /// How a PDF is read: with the layout and table models, or its text layer alone
    #[arg(long, value_enum, default_value_t = Pipeline::Standard)]
    pipeline: Pipeline,

    /// Do not recover tables' structure with TableFormer
    #[arg(long)]
    no_tables: bool,

    /// How pictures are written: a placeholder, embedded, or files beside the output
    #[arg(long, value_enum, default_value_t = ImageExportMode::Placeholder)]
    image_export_mode: ImageExportMode,

    /// The pages of a PDF to convert, from 1, as FIRST-LAST
    #[arg(long, value_name = "FIRST-LAST", value_parser = page_range)]
    page_range: Option<(usize, usize)>,

    /// The password of an encrypted PDF
    #[arg(long, value_name = "PASSWORD")]
    pdf_password: Option<String>,

    /// Tracked changes in Word and OpenDocument files: accept, reject or all
    #[arg(long, value_name = "MODE", default_value = "accept", value_parser = |text: &str| text.parse::<TrackChanges>())]
    track_changes: TrackChanges,

    /// Number headings by the levels their numbering and font sizes imply, in a PDF
    #[arg(long)]
    heading_hierarchy: bool,

    /// Give up on a document after this many seconds
    #[arg(long, value_name = "SECONDS", value_parser = seconds)]
    document_timeout: Option<Duration>,

    /// Stop at the first document that fails, rather than going on to the next
    #[arg(long)]
    abort_on_error: bool,
}

/// An output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum To {
    Md,
    Json,
    Html,
    Text,
    Doclang,
}

impl To {
    /// The extension a file of this format is written with.
    fn extension(self) -> &'static str {
        match self {
            To::Md => "md",
            To::Json => "json",
            To::Html => "html",
            To::Text => "txt",
            To::Doclang => "dclg",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Pipeline {
    Standard,
    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ImageExportMode {
    Placeholder,
    Embedded,
    Referenced,
}

fn seconds(text: &str) -> Result<Duration, String> {
    let seconds = text.trim().parse::<f64>().map_err(|e| e.to_string())?;
    Duration::try_from_secs_f64(seconds)
        .ok()
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| format!("{text} is not a number of seconds above 0"))
}

fn page_range(text: &str) -> Result<(usize, usize), String> {
    let (first, last) = text.split_once('-').unwrap_or((text, text));
    let parse = |n: &str| n.trim().parse::<usize>().map_err(|e| e.to_string());
    match (parse(first)?, parse(last)?) {
        (0, _) => Err("pages are counted from 1".into()),
        (first, last) if last < first => Err(format!("{last} comes before {first}")),
        range => Ok(range),
    }
}

/// The documents `sources` name: each file as given, and each file under a directory that
/// docling knows by its extension and `from` accepts, in order.
fn documents(sources: &[PathBuf], from: &[InputFormat]) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for source in sources {
        if !source.is_dir() {
            found.push(source.clone());
            continue;
        }
        let mut entries: Vec<PathBuf> = fs::read_dir(source)?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<_>>()?;
        entries.sort();
        for path in entries {
            if path.is_dir() {
                found.extend(documents(&[path], from)?);
                continue;
            }
            let format = path
                .extension()
                .and_then(|ext| ext.to_str())
                .and_then(|ext| InputFormat::from_extension(&ext.to_ascii_lowercase()));
            if format.is_some_and(|f| readable(f) && (from.is_empty() || from.contains(&f))) {
                found.push(path);
            }
        }
    }
    Ok(found)
}

/// Whether a format is one this build converts.
fn readable(format: InputFormat) -> bool {
    !matches!(
        format,
        InputFormat::Text | InputFormat::Image | InputFormat::Audio | InputFormat::Video
    ) && format.is_compiled_in()
}

/// `path`, converted, or what to say about why it was not.
fn convert(cli: &Cli, path: &Path) -> Result<DoclingDocument, String> {
    let about = fs::metadata(path).map_err(|e| e.to_string())?;
    if !about.is_file() {
        return Err("is not a regular file".into());
    }
    if about.len() > MAX_FILE_BYTES {
        return Err(format!(
            "is more than the {} MiB of file this reads at once",
            MAX_FILE_BYTES >> 20
        ));
    }

    let source = SourceDocument::from_file(path).map_err(|e| e.to_string())?;
    match source.format {
        InputFormat::Text => return Err("is source code, not a document: read it with cat".into()),
        format if !readable(format) => {
            return Err(format!(
                "is {}, which holds no text this build can read",
                format.as_str()
            ));
        }
        format if !cli.from.is_empty() && !cli.from.contains(&format) => {
            return Err(format!("is {}, which --from leaves out", format.as_str()));
        }
        _ => {}
    }

    let mut converter = DocumentConverter::new()
        .no_ocr(true)
        .track_changes(Some(cli.track_changes))
        .text_layer_only(matches!(cli.pipeline, Pipeline::Native))
        .no_table_former(cli.no_tables)
        .heading_hierarchy(cli.heading_hierarchy)
        .password(cli.pdf_password.clone())
        .document_timeout(cli.document_timeout);
    if let Some((first, last)) = cli.page_range {
        converter = converter.page_range(first, last);
    }

    // The input is somebody's file, and a reader meeting one it was not written for is a
    // document this cannot read, not a crash. The default hook would print the panic first.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    // Unwind-safe as used: neither is touched again once it has panicked.
    let converted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        converter.convert(source)
    }));
    std::panic::set_hook(hook);
    match converted {
        Ok(converted) => converted.map(|c| c.document).map_err(|e| e.to_string()),
        Err(panic) => Err(panic
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| panic.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "docling gave up on it".into())),
    }
}

/// `document` in `to`, and the pictures to write beside it, each at a path relative to the
/// output, under `artifacts`.
fn export(
    document: &DoclingDocument,
    to: To,
    images: ImageExportMode,
    artifacts: &str,
) -> (String, Vec<(String, Vec<u8>)>) {
    let mode = match images {
        ImageExportMode::Placeholder => ImageMode::Placeholder,
        ImageExportMode::Embedded => ImageMode::Embedded,
        ImageExportMode::Referenced => ImageMode::Referenced,
    };
    match to {
        To::Md => document.export_to_markdown_with_images(mode, artifacts),
        To::Html => document.export_to_html_with_images(mode, artifacts),
        To::Doclang => document.export_to_doclang_with_images(mode),
        To::Json => (document.export_to_json(), Vec::new()),
        To::Text => (document.export_to_text(), Vec::new()),
    }
}

/// Where the outputs go.
enum Destination<'a> {
    Stdout,
    File(&'a Path),
    Dir(&'a Path),
}

/// The command, done: what goes to stdout, and a line for each document that failed. A line
/// that cannot be carried out at all is the error.
fn run(cli: &Cli) -> Result<(String, Vec<String>), String> {
    let documents = documents(&cli.sources, &cli.from).map_err(|e| format!("{NAME}: {e}\n"))?;
    if documents.is_empty() {
        return Err(format!("{NAME}: no documents in what was given\n"));
    }
    let destination = match (&cli.output, &cli.output_file) {
        (Some(dir), _) => Destination::Dir(dir),
        (None, Some(file)) => Destination::File(file),
        (None, None) => Destination::Stdout,
    };
    if !matches!(destination, Destination::Dir(_)) && (documents.len() != 1 || cli.to.len() != 1) {
        return Err(format!(
            "{NAME}: {} documents in {} formats need --output DIR\n",
            documents.len(),
            cli.to.len()
        ));
    }
    if matches!(destination, Destination::Stdout)
        && cli.image_export_mode == ImageExportMode::Referenced
    {
        return Err(format!(
            "{NAME}: --image-export-mode referenced writes files, and needs --output\n"
        ));
    }
    // A directory's outputs are named by the document's name alone, so two of one name would
    // be written over each other.
    if matches!(destination, Destination::Dir(_)) {
        let mut named = HashMap::new();
        for path in &documents {
            if let Some(other) = named.insert(path.file_stem(), path) {
                return Err(format!(
                    "{NAME}: {} and {} would be written as one: convert them one at a time\n",
                    other.display(),
                    path.display()
                ));
            }
        }
    }

    let mut stdout = String::new();
    let mut failed = Vec::new();
    'documents: for path in &documents {
        let said = |e: &dyn std::fmt::Display| format!("{NAME}: {}: {e}\n", path.display());
        let document = match convert(cli, path) {
            Ok(document) => document,
            Err(e) => {
                failed.push(said(&e));
                if cli.abort_on_error {
                    break;
                }
                continue;
            }
        };
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let artifacts = format!("{stem}_artifacts");
        for &to in &cli.to {
            let (text, assets) = export(&document, to, cli.image_export_mode, &artifacts);
            let (file, base) = match destination {
                Destination::Stdout => {
                    stdout.push_str(&text);
                    if !text.ends_with('\n') {
                        stdout.push('\n');
                    }
                    continue;
                }
                Destination::File(file) => (file.to_path_buf(), file.parent()),
                Destination::Dir(dir) => {
                    (dir.join(format!("{stem}.{}", to.extension())), Some(dir))
                }
            };
            let base = base.unwrap_or(Path::new(""));
            let written = assets
                .iter()
                .try_for_each(|(asset, bytes)| write(&base.join(asset), bytes))
                .and_then(|()| write(&file, text.as_bytes()));
            if let Err(e) = written {
                failed.push(format!("{NAME}: {}: {e}\n", file.display()));
                if cli.abort_on_error {
                    break 'documents;
                }
            }
        }
    }
    Ok((stdout, failed))
}

/// `bytes` at `path`, its directory made.
fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, bytes)
}

/// The models and fonts in `.models/` beside this program, named to docling by its own variables
/// for them, and every other `DOCLING_*` variable dropped: docling otherwise looks in the working
/// directory first and takes what the environment says, so what a document reads as would follow
/// wherever and however `docread` is run, not its command line.
fn use_models_beside_me() {
    let models = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map(|exe| exe.with_file_name(".models"))
        .unwrap_or_else(|_| PathBuf::from(".models"));
    let docling: Vec<_> = std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| name.to_string_lossy().starts_with("DOCLING_"))
        .collect();
    let set = [
        ("DOCLING_LAYOUT_ONNX", "layout_heron_int8.onnx"),
        (
            "DOCLING_TABLEFORMER_ENCODER",
            "tableformer/encoder_fp16.onnx",
        ),
        ("DOCLING_TABLEFORMER_DECODER", "tableformer/decoder_kv.onnx"),
        ("DOCLING_TABLEFORMER_BBOX", "tableformer/bbox.onnx"),
        ("DOCLING_RS_FONT_DIRS", "fonts"),
    ];
    // SAFETY: called first in `main`, before anything has started another thread.
    unsafe {
        for name in docling {
            std::env::remove_var(name);
        }
        for (name, file) in set {
            std::env::set_var(name, models.join(file));
        }
        // The page image is the layout model's input: drawn in the fonts beside the program
        // alone, it is the same on every machine.
        std::env::set_var("DOCLING_RS_SYSTEM_FONTS", "0");
    }
}

fn main() -> ExitCode {
    use_models_beside_me();
    let (stdout, failed) = match run(&Cli::parse()) {
        Ok(done) => done,
        Err(said) => {
            std::io::stderr().write_all(said.as_bytes()).ok();
            return ExitCode::FAILURE;
        }
    };
    for said in &failed {
        std::io::stderr().write_all(said.as_bytes()).ok();
    }
    let written = match std::io::stdout().write_all(stdout.as_bytes()) {
        // A reader that stopped reading — `| head` — is not this program failing.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => true,
        Err(e) => {
            let said = format!("{NAME}: writing the answer: {e}\n");
            std::io::stderr().write_all(said.as_bytes()).ok();
            false
        }
        Ok(()) => true,
    };
    if written && failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind as ClapErrorKind;

    use super::*;

    fn parse(line: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once(NAME).chain(line.iter().copied()))
    }

    fn markdown(dir: &tempfile::TempDir, name: &str, text: &str) -> String {
        let path = dir.path().join(name);
        fs::write(&path, text).expect("a file");
        path.to_str().expect("UTF-8").to_owned()
    }

    /// One document is written to stdout as Markdown, unasked.
    #[test]
    fn a_document_is_markdown_on_stdout() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = markdown(&dir, "a.md", "# A heading\n\nA paragraph.\n");

        let (stdout, failed) = run(&parse(&[&path]).unwrap()).expect("converted");
        assert!(failed.is_empty(), "{failed:?}");
        assert!(stdout.contains("# A heading"), "{stdout}");
        assert!(stdout.contains("A paragraph."), "{stdout}");
    }

    /// Each format asked for is a file in `--output`, named after the document.
    #[test]
    fn formats_are_files_in_the_output_directory() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = markdown(&dir, "a.md", "# A heading\n");
        let out = dir.path().join("out");
        let out = out.to_str().expect("UTF-8");

        let line = [
            path.as_str(),
            "--to",
            "json",
            "--to",
            "doclang",
            "--to",
            "text",
            "--output",
            out,
        ];
        let (stdout, failed) = run(&parse(&line).unwrap()).expect("converted");
        assert!(
            stdout.is_empty() && failed.is_empty(),
            "{stdout} {failed:?}"
        );
        let json = fs::read_to_string(dir.path().join("out/a.json")).expect("a.json");
        assert!(json.contains("A heading"), "{json}");
        let doclang = fs::read_to_string(dir.path().join("out/a.dclg")).expect("a.dclg");
        assert!(doclang.starts_with("<doclang"), "{doclang}");
        assert!(dir.path().join("out/a.txt").exists());
    }

    /// Several documents need somewhere to go other than stdout, and are not written there.
    #[test]
    fn several_documents_need_an_output_directory() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let a = markdown(&dir, "a.md", "a\n");
        let b = markdown(&dir, "b.md", "b\n");

        let said = run(&parse(&[&a, &b]).unwrap()).expect_err("two documents, no --output");
        assert!(said.contains("--output"), "{said}");
    }

    /// Two documents of one name would be one output in a directory, and are refused before
    /// either is written.
    #[test]
    fn documents_of_one_name_are_not_written_over_each_other() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let md = markdown(&dir, "report.md", "# From Markdown\n");
        let html = markdown(&dir, "report.html", "<h1>From HTML</h1>\n");
        let out = dir.path().join("out");

        let line = [
            md.as_str(),
            html.as_str(),
            "--output",
            out.to_str().unwrap(),
        ];
        let said = run(&parse(&line).unwrap()).expect_err("one name, two documents");
        assert!(said.contains("would be written as one"), "{said}");
        assert!(!out.exists());
    }

    /// Sources with no document in them are said to be, not answered with nothing.
    #[test]
    fn sources_with_no_documents_are_refused() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        markdown(&dir, "main.rs", "fn main() {}\n");
        let path = dir.path().to_str().unwrap();
        let out = dir.path().join("out");

        for line in [vec![path], vec![path, "--output", out.to_str().unwrap()]] {
            let said = run(&parse(&line).unwrap()).expect_err("no documents");
            assert!(said.contains("no documents"), "{said}");
        }
    }

    /// An output that cannot be written is a failure of its document, said with the others,
    /// and the documents after it are still converted.
    #[test]
    fn an_output_that_cannot_be_written_is_said_and_the_rest_go_on() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let a = markdown(&dir, "a.md", "# A\n");
        let b = markdown(&dir, "b.md", "# B\n");
        let out = dir.path().join("out");
        fs::create_dir_all(out.join("a.md")).expect("a directory where a.md goes");

        let line = [a.as_str(), b.as_str(), "--output", out.to_str().unwrap()];
        let (_, failed) = run(&parse(&line).unwrap()).expect("ran");
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert!(failed[0].contains("a.md"), "{failed:?}");
        assert!(out.join("b.md").is_file());
    }

    /// A directory's documents are the files docling knows by their extension that `--from`
    /// accepts; source code in it is not one.
    #[test]
    fn a_directory_is_its_documents() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        markdown(&dir, "a.md", "# A\n");
        markdown(&dir, "b.html", "<h1>B</h1>\n");
        markdown(&dir, "main.rs", "fn main() {}\n");

        let all = documents(&[dir.path().to_path_buf()], &[]).expect("listed");
        let names: Vec<_> = all
            .iter()
            .map(|p| p.file_name().unwrap().to_owned())
            .collect();
        assert_eq!(names, ["a.md", "b.html"]);

        let md = documents(&[dir.path().to_path_buf()], &[InputFormat::Md]).expect("listed");
        assert_eq!(md.len(), 1);
    }

    /// The document as it stands by default: what was inserted is in it, what was deleted is
    /// not; `--track-changes reject` is the other way round.
    #[test]
    fn tracked_changes_are_accepted_unless_asked_otherwise() {
        use docx_rs::{Delete, Docx, Insert, Paragraph, Run};

        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("a.docx");
        Docx::new()
            .add_paragraph(
                Paragraph::new().add_insert(Insert::new(Run::new().add_text("Inserted"))),
            )
            .add_paragraph(
                Paragraph::new()
                    .add_delete(Delete::new().add_run(Run::new().add_delete_text("Deleted"))),
            )
            .build()
            .pack(fs::File::create(&path).expect("a file"))
            .expect("a document");
        let path = path.to_str().expect("UTF-8");

        let (accepted, _) = run(&parse(&[path]).unwrap()).expect("converted");
        assert!(
            accepted.contains("Inserted") && !accepted.contains("Deleted"),
            "{accepted}"
        );
        let (rejected, _) =
            run(&parse(&[path, "--track-changes", "reject"]).unwrap()).expect("converted");
        assert!(
            !rejected.contains("Inserted") && rejected.contains("Deleted"),
            "{rejected}"
        );
    }

    /// A document that fails is said, named as it was given, and the others are still
    /// converted; `--abort-on-error` stops at it.
    #[test]
    fn a_failure_is_said_and_the_rest_go_on() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let broken = dir.path().join("a.pdf");
        fs::write(&broken, b"%PDF-1.4\ngarbage").expect("a file");
        let broken = broken.to_str().expect("UTF-8");
        let good = markdown(&dir, "b.md", "# B\n");
        let text = markdown(&dir, "c.rs", "fn main() {}\n");
        let out = dir.path().join("out");
        let out = out.to_str().expect("UTF-8");

        let (_, failed) =
            run(&parse(&[broken, &good, &text, "--output", out]).unwrap()).expect("ran");
        assert_eq!(failed.len(), 2, "{failed:?}");
        assert!(
            failed[0].starts_with(&format!("{NAME}: {broken}: ")),
            "{failed:?}"
        );
        assert!(failed[1].contains("source code"), "{failed:?}");
        assert!(dir.path().join("out/b.md").exists());

        let (_, failed) =
            run(&parse(&[broken, &good, "--output", out, "--abort-on-error"]).unwrap())
                .expect("ran");
        assert_eq!(failed.len(), 1, "{failed:?}");
    }

    /// Options are refused where they are typed wrong, not where they are used.
    #[test]
    fn options_are_checked_as_they_are_parsed() {
        for line in [
            ["a.pdf", "--page-range=0-3"],
            ["a.pdf", "--page-range=5-2"],
            ["a.pdf", "--from=nonsense"],
            ["a.pdf", "--track-changes=maybe"],
            ["a.pdf", "--document-timeout=-1"],
            ["a.pdf", "--document-timeout=0"],
            ["a.pdf", "--document-timeout=inf"],
            ["a.pdf", "--document-timeout=NaN"],
        ] {
            let e = parse(&line).expect_err("refused");
            assert_eq!(e.kind(), ClapErrorKind::ValueValidation, "{line:?}");
        }
        assert_eq!(
            parse(&["a.pdf", "--page-range", "3"]).unwrap().page_range,
            Some((3, 3))
        );
    }

    /// Help is an answer, and bare `docread` is answered with it.
    #[test]
    fn help_is_an_answer() {
        let e = parse(&["--help"]).expect_err("help is not a conversion");
        assert_eq!(e.kind(), ClapErrorKind::DisplayHelp);
        assert!(e.render().to_string().contains(&format!("Usage: {NAME}")));
        assert_eq!(
            parse(&[]).expect_err("nothing was asked for").kind(),
            ClapErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
    }
}
