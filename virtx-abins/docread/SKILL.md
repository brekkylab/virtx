---
name: docread
description: "Parse documents into text that keeps their headings, lists and tables"
---

Parse documents into text that keeps their headings, lists and tables

Usage: docread [OPTIONS] <SOURCE>...

Arguments:
  <SOURCE>...  Files or directories to convert; a directory's documents are found by their extension

Options:
      --list-input-formats
          List the file extensions read, one a line, and exit
      --from <FORMAT>
          Input formats to accept, by docling's names (pdf, docx, pptx, xlsx, html, md, ...); defaults to all
      --to <FORMAT>
          Output formats [default: md] [possible values: md, json, html, text, doclang]
      --output <DIR>
          Directory to write each document's outputs in, as <name>.<format>
      --output-file <PATH>
          File to write the output to, for one document in one format
      --pipeline <PIPELINE>
          How a PDF is read: with the layout and table models, or its text layer alone [default: standard] [possible values: standard, native]
      --no-tables
          Do not recover tables' structure with TableFormer
      --image-export-mode <IMAGE_EXPORT_MODE>
          How pictures are written: a placeholder, embedded, or files beside the output [default: placeholder] [possible values: placeholder, embedded, referenced]
      --page-range <FIRST-LAST>
          The pages of a PDF to convert, from 1, as FIRST-LAST
      --pdf-password <PASSWORD>
          The password of an encrypted PDF
      --track-changes <MODE>
          Tracked changes in Word and OpenDocument files: accept, reject or all [default: accept]
      --heading-hierarchy
          Number headings by the levels their numbering and font sizes imply, in a PDF
      --document-timeout <SECONDS>
          Give up on a document after this many seconds
      --abort-on-error
          Stop at the first document that fails, rather than going on to the next
  -h, --help
          Print help

Reads PDF, Word, PowerPoint, Excel, OpenDocument, RTF, HTML, Markdown, AsciiDoc, LaTeX, CSV, EPUB, email, HWP and more, each by its file's extension: --list-input-formats lists them. Writes Markdown, JSON, HTML, text and DocLang.

Uses docling inside, and its ML models where a file's structure has to be found, as in a PDF. There is no OCR: a scanned PDF has no text.

Reading a PDF with the models takes up to about 2 GiB of memory, and a sandbox with less kills docread with exit code 137 and no message. --pipeline native needs far less.

One document in one format goes to stdout; more need --output.
