---
name: docread
description: Read a document file (text, Word, PDF) by numbered lines, a window at a time
---

Read a document file (text, Word, PDF) by numbered lines, a window at a time

Usage: docread [OPTIONS] <FILE>

Arguments:
  <FILE>  the file to read

Options:
  -o, --offset <N>  the line to start at [default: 1]
  -n, --limit <N>   how many lines to show [default: 2000]
  -h, --help        Print help

Lines are counted from 1, and the numbers printed are the ones -o takes back. Output ends with a bracketed line only when the window is not the whole file; no such line means there is nothing else in it.
