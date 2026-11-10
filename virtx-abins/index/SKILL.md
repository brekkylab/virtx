---
name: index
description: Index document files and search them
---

Index document files and search them

Usage: index <COMMAND>

Commands:
  init     Make a store where there is no file yet
  ingest   Index files into a store
  search   Search a store
  sync     Make a store match the tree under the paths given
  purge    Take documents back out of a store, by the paths they were ingested under
  rechunk  Cut every document in a store into chunks again, by a new size and overlap
  list     The stores in a directory, and how much is in each
  drop     Remove a store. The files it was built from are untouched
  help     Print this message or the help of the given subcommand(s)

Options:
  -h, --help  Print help

<STORE> is the path the index file is kept at. It is a path like any other this program is given. `init` makes one; every other command expects it to be there already.

## index init

Make a store where there is no file yet.

The one command that brings a store into being. Every other one expects the file to be there, which is what keeps a mistyped path from becoming an empty store that answers every search with nothing.

Usage: index init [OPTIONS] <STORE>

Arguments:
  <STORE>
          Where to make it, `notes.db` for instance

Options:
      --chunk-size <CHARS>
          The most characters a chunk holds. A chunk is cut before a heading, at a blank line, at a line end or at whitespace where it can be, so most are shorter
          
          [default: 1200]

      --overlap <CHARS>
          How many characters at the end of a chunk the next one may start with again, so a passage cut in two is still whole in one of them
          
          [default: 200]

  -h, --help
          Print help (see a summary with '-h')

## index ingest

Index files into a store.

A directory means everything under it. Re-ingesting a path replaces it, so running this twice means the same as running it once.

Usage: index ingest <STORE> <PATH>...

Arguments:
  <STORE>
          Which store to write

  <PATH>...
          What to index, `docs` or `docs/a.md` for instance. A document is filed under the path as it is written here, so this is also what `sync` and `purge` name it by

Options:
  -h, --help
          Print help (see a summary with '-h')

## index search

Search a store.

Each hit is a chunk of a document, on a line of its own: the score, larger being nearer, then `path:first-last` with the lines the chunk spans, then the document's name, all tab-separated. Under it, indented by a tab, is a snippet cut around the match, or the chunk's whole text with `--text`.

Usage: index search [OPTIONS] <STORE> <QUERY>...

Arguments:
  <STORE>
          Which store to ask

  <QUERY>...
          The words to look for, joined back together since a shell has already split them

Options:
  -n, --limit <N>
          How many hits to answer with
          
          [default: 10]

      --text
          Answer with each chunk's whole text, every line of it indented under the hit, in place of a snippet

  -h, --help
          Print help (see a summary with '-h')

## index sync

Make a store match the tree under the paths given.

What a second `ingest` cannot do: it picks up what was added and replaces what changed, but leaves a document behind for a file that is gone. This removes those too, so the store says what the tree says.

Only files whose size or modification time moved are read again.

Usage: index sync [OPTIONS] <STORE> <PATH>...

Arguments:
  <STORE>
          Which store to bring up to date

  <PATH>...
          Paths, spelled as `ingest` spelled them. A path that is no longer there means everything the store held under it goes

Options:
      --force
          Re-read every file, not only the ones whose size or modification time moved.
          
          What to reach for when a file was edited within one timestamp tick and kept its length, which some filesystems round to a second.

  -h, --help
          Print help (see a summary with '-h')

## index purge

Take documents back out of a store, by the paths they were ingested under.

A path removes the document at it and everything under it. Opens no file in the tree: a file gone from the tree is the reason to do this, and it is still in the store.

Usage: index purge <STORE> <PATH>...

Arguments:
  <STORE>
          Which store to take them out of

  <PATH>...
          Paths, spelled as `ingest` spelled them

Options:
  -h, --help
          Print help (see a summary with '-h')

## index rechunk

Cut every document in a store into chunks again, by a new size and overlap.

From the text the store already holds: no file is read, so a document whose file is gone is cut too. The store keeps the new chunking for what is ingested after.

Usage: index rechunk [OPTIONS] <STORE>

Arguments:
  <STORE>
          Which store to cut again

Options:
      --chunk-size <CHARS>
          The most characters a chunk holds. A chunk is cut before a heading, at a blank line, at a line end or at whitespace where it can be, so most are shorter
          
          [default: 1200]

      --overlap <CHARS>
          How many characters at the end of a chunk the next one may start with again, so a passage cut in two is still whole in one of them
          
          [default: 200]

  -h, --help
          Print help (see a summary with '-h')

## index list

The stores in a directory, and how much is in each.

Looks at that one directory and does not descend into it. Files that are not stores are passed over in silence, since a directory holding other things is the normal case.

Usage: index list [DIR]

Arguments:
  [DIR]
          Which directory to look in. The one this was run in, if it is left out
          
          [default: .]

Options:
  -h, --help
          Print help (see a summary with '-h')

## index drop

Remove a store. The files it was built from are untouched.

Refuses anything that is not a store, which is the whole of what this does that `rm` does not: a store is a path, so one mistyped argument could otherwise be somebody's data.

Usage: index drop <STORE>

Arguments:
  <STORE>
          Which store to forget

Options:
  -h, --help
          Print help (see a summary with '-h')
