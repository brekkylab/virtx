---
name: mem
description: Search an agent's memories, or change what they hold
---

Search an agent's memories, or change what they hold

Usage: mem <COMMAND>

Commands:
  init    make a store at <store>
  insert  remember each of <texts> [alias: add]
  search  the memories nearest <query>, nearest first
  list    the memories in <store>, newest first
  delete  forget the memories with these <ids>
  update  correct the memory with <id> so that it says <text> [alias: edit]

Options:
  -h, --help  Print help

<store> is the path the memory file is kept at. It is a path like any other this program is given, so a relative one resolves against the working directory. `init` creates it; every other command expects it to be there already.

The default output is one memory per line, which a memory containing a newline cannot be spelled in. A caller whose memories may be multi-line, Markdown for instance, should ask for `--json`, which is one JSON object per line and carries the id as well.

## mem init

make a store at <store>

Usage: mem init <STORE>

Arguments:
  <STORE>  the store to make

Options:
  -h, --help  Print help

## mem insert

remember each of <texts>

Usage: mem insert [OPTIONS] <STORE> [TEXTS]...

Arguments:
  <STORE>     the store to write to
  [TEXTS]...  what to remember: one memory per argument

Options:
      --json  answer with one line of JSON per memory: its id, its text, and when it was written
  -h, --help  Print help

## mem search

the memories nearest <query>, nearest first

Usage: mem search [OPTIONS] <STORE> <QUERY>

Arguments:
  <STORE>  the store to search
  <QUERY>  what to look for

Options:
  -n, --limit <LIMIT>  how many memories to answer with [default: 10]
      --json           answer with one line of JSON per memory: its id, its text, and when it was written
  -h, --help           Print help

## mem list

the memories in <store>, newest first

Usage: mem list [OPTIONS] <STORE>

Arguments:
  <STORE>  the store to read

Options:
  -n, --limit <LIMIT>  how many memories to answer with; every one of them when not given
      --json           answer with one line of JSON per memory: its id, its text, and when it was written
  -h, --help           Print help

## mem delete

forget the memories with these <ids>

Usage: mem delete [OPTIONS] <STORE> [IDS]...

Arguments:
  <STORE>   the store to write to
  [IDS]...  which memories to forget: one id per argument

Options:
      --json  answer with one line of JSON per id
  -h, --help  Print help

## mem update

correct the memory with <id> so that it says <text>

Usage: mem update [OPTIONS] <STORE> <ID> <TEXT>

Arguments:
  <STORE>  the store to write to
  <ID>     which memory to correct
  <TEXT>   what it should say now

Options:
      --json  answer with one line of JSON: the memory's id, its text, and when it was written
  -h, --help  Print help
