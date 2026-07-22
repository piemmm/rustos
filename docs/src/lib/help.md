# `tairix-help` — shared command-help engine

`tairix_help` (`lib/help`) is TAIRiX's one implementation of command help
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

`Help/` holds one directory per BCP-47 locale (`fr-FR`, `es-419`, …), of
which the mandatory `en-US/` is the canonical source. Each directory holds
one `<command>.md` per command or topic.

`load(source, requested, name)` selects a document by the first hit in a
fixed, deterministic chain:

1. `Help/<requested>/<name>.md` — the exact locale.
2. The lexicographically first same-language directory (any region) that
   holds the document, so the choice is stable across runs.
3. `Help/en-US/<name>.md` — the canonical document.

`load_raw` is the same single walk without the parse: it returns the
size-bounded raw bytes plus the selection, for a caller that must run the
parse elsewhere — `man` hands them to the sandboxed
[`tairix-sandbox`](./sandbox.md) `helpdoc` worker so a foreign bundle's
document is never parsed in its own process. `load` is `load_raw` plus
`HelpDoc::parse`; the walk has one definition.

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

Both renderers take a `RenderCtx { locale, styling }`:

- `render_short(doc, ctx)` — the `-h`/`-?` view: the `NAME` and `SYNOPSIS`
  content plus the `OPTIONS` list, compactly and without headings.
- `render_full(doc, ctx)` — the whole `man` page: every section in order, with
  its heading, verbatim indented code blocks, and width-padded tables honouring
  the declared column alignment.

**Localised headings.** The document keys stay the language-neutral `## NAME`
… `## SEE ALSO`, but the reader sees each heading in the *served* page's
language (`SectionKind::heading_label(locale)` — `NOM`/`BESCHREIBUNG`/`説明`
…, selected by primary language subtag, English for any untranslated
language). `man` passes the served locale (which may differ from the request
after a fallback) so the headings match the prose.

**Colour.** Styling follows the one standard scheme (`tairix_vt::scheme`):
headings and sub-headings in the heading role, `*emphasis*` in the emphasis
role, inline code and fenced blocks in the literal role, `**strong**` bold, and
table rules in the border role. Each styled run is emitted flat (open the
style, print, single reset), so stripping every escape leaves the exact same
text — the information never rests on colour alone. The `Styling` level chooses
how much of it to emit:

- `Styling::Plain` — no escape sequences at all, for a redirected or piped
  consumer (`man ls | cat` is clean text).
- `Styling::Monochrome` — the emphasis attributes but no colour, for a terminal
  that renders none.
- `Styling::Colour` — the full scheme.

Both emit `tairix_vt::Op` sequences (widths from `tairix_curses`), so the
escape vocabulary stays the one `lib/vt` definition and the output prints no
control bytes. Paging and the plain/monochrome/colour decision belong to the
`man` app (it resolves them from the console attestation and `TERM` through the
one `tairix_termcap` judgement); the active locale is resolved once by the
session/shell and passed in.

## A command's own short help, in one place

Every command app answers its reserved `-h`/`-?` switches the same way, so
that sequence lives here once rather than per tool:

- `own_short_help(source, locale, word)` — the pure helper: parse the raw
  `LANG` preference (a malformed or missing tag degrades to `en-US/`),
  load `word`'s document through the fallback chain, render the short view,
  and return it as encoded `lib/vt` bytes. It renders `Styling::Plain` (no
  escapes): the short view carries no headings, and a program emitting `-h`
  has not attested its standard output as a terminal, so a piped or captured
  `-h` is clean text. `None` when no document can be served — the caller then
  prints its own usage banner, so `-h` never fails.
- `BundleHelp` (the `rt` cargo feature) — the production `HelpSource`: the
  running command app's own `/System/Apps/<word>.app/Help/` tree, read
  through the `tairix-rt` file wrappers. It adds no authority (every
  per-inode and mount check stays kernel-side) and spells the bundle path
  from the shared `lib/abi` store/suffix constants, so it cannot drift from
  where the image builder plants the documents. Only a freestanding `Run`
  binary enables the feature; the engine itself stays seam-injected and
  performs no ambient I/O.

## The help-tree lint (the `lint` cargo feature)

`lint_help_trees` is the one judgement of a set of discovered `Help/` trees
(`plans/APPS.md` §8.1), shared by the `cargo xtask help-lint` CI gate and the
`tools/syshelp` aggregator tests so the two can never diverge. It is pure —
rows of `(bundle, locale, file, bytes)` in, violation messages out, no I/O —
and checks:

- locale and document-name spellings, and the fail-closed structural parse
  bounds, on every document;
- canonical `en-US/` presence, completeness across the standing
  `REQUIRED_LOCALES` set, and no translation-only documents;
- cross-locale `OPTIONS` switch-key drift: every item leads with a backticked
  language-neutral key and each translation's key sequence equals
  `en-US/`'s (the per-app unit tests separately pin `en-US/`'s keys to
  each program's actual argument parser);
- the closed content-policy screen, whole-word and case-insensitive, in
  every locale, plus a substring screen for the unsegmented CJK languages
  (Chinese and Japanese prose carries no word boundaries a word split can
  find).

The feature is host-only tooling; a TAIRiX program never links it. The
`help-lint` gate additionally verifies coverage: every command app the
`AppInfo.toml` discovery walk finds ships an `en-US/<command>.md` document.
