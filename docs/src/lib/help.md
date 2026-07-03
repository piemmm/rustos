# `rustos-help` — shared command-help engine

`rustos_help` (`lib/help`) is RustOS's one implementation of command help
(`plans/APPS.md`). Every application bundle may ship a `Help/` tree — the
internationalised, structured-Markdown command reference that replaced the
old `Documentation/` entry (`BundleEntry::Help`, [appinfo](../abi/appinfo.md))
— and three consumers read it: the `man` command, every command's short
`-h`/`-?` help, and any graphical help viewer. The locale walk, the bounded
Markdown parse, and the terminal render are identical wherever they happen,
so they live here once and every consumer imports them.

Stability tier: **experimental** (the surface grows as the `man` app and the
shell's short-help convention land).

## The `Help/` tree and locale fallback

`Help/` holds one directory per BCP-47 locale (`fr-FR`, `es-419`, …) plus the
mandatory `default/` sentinel, which is always en-US and is the canonical
source. Each directory holds one `<command>.md` per command or topic.

`load(source, requested, name)` selects a document by the first hit in a
fixed, deterministic chain:

1. `Help/<requested>/<name>.md` — the exact locale.
2. The lexicographically first same-language directory (any region) that
   holds the document, so the choice is stable across runs.
3. `Help/default/<name>.md`.

The result reports which directory served and how it relates to the request
(`Selection`/`Fallback`), so `man` can emit its locale-fallback `stdinfo`
record. A document is rendered whole from one file — falling back never mixes
languages within a page — and a miss everywhere is a typed `NotFound`, never
invented text.

I/O happens only through the injected `HelpSource` seam, which the caller
scopes (by capability) to exactly one bundle's `Help/` tree. The engine hands
the seam only spellings it validated itself: `Locale` and `DocumentName`
grammars make a path separator or dot unrepresentable, so a hostile command
name cannot traverse out of the tree.

## The document format

A help document is UTF-8 Markdown with a fixed, ordered set of level-2
sections whose keys are language-neutral and written verbatim; only the prose
under them is localised. `NAME`, `SYNOPSIS`, and `DESCRIPTION` are required;
`OPTIONS`, `EXAMPLES`, `EXIT STATUS`, `ENVIRONMENT`, and `SEE ALSO` are
optional. Command switches never change with the locale: the flag tokens in
`OPTIONS` live in backticked code spans and match the program's argument
parser exactly.

Within a section the parser accepts paragraphs, `###` sub-headings, bullet
(`- `) and ordered (`1. `) lists with two-space continuation lines, fenced
code blocks, and pipe tables, with `` `code` ``, `**strong**`, and
`*emphasis*` inline spans (a section-heading line inside a fence stays code).

Help content is signed but parsed as hostile input. Document size, line
length and count, blocks per section, list items, and table dimensions are
fixed security bounds — validation bounds, not growable capacities — and any
violation, control character, unknown/duplicate/out-of-order section, or
malformed structure rejects the whole document with a typed `HelpError`. The
parser is fuzzed (`fuzz_help`, run by `cargo xtask fuzz`).

## The two render surfaces

- `render_short` — the `-h`/`-?` view: the `NAME` and `SYNOPSIS` content plus
  the `OPTIONS` list, compactly and without headings.
- `render_full` — the whole `man` page: every section in order, bold
  headings, bold code/strong, underlined emphasis, verbatim indented code
  blocks, and width-padded tables honouring the declared column alignment.

Both emit `rustos_vt::Op` sequences (widths from `rustos_curses`), so the
escape vocabulary stays the one `lib/vt` definition and the output prints no
control bytes. Paging and terminal probing belong to the `man` app; the
active locale is resolved once by the session/shell and passed in.
