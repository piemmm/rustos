# rustos-help

Shared command-help engine for RustOS (`lib/help`, `plans/APPS.md`).

Every application bundle may ship a `Help/` tree: one structured-Markdown
document per command or topic, under one directory per BCP-47 locale plus the
mandatory `default/` (en-US) canonical source. Three consumers read that tree —
the `man` command, every command's short `-h`/`-?` help, and any graphical help
viewer — so the locale walk, the Markdown parse, and the terminal render live
here once and every consumer imports them.

## API

- `Locale::parse` / `DocumentName::parse` — fail-closed validation of the only
  spellings the engine will ever hand to a backing store; a document name can
  never spell a path separator or a dot, so traversal is unrepresentable.
- `HelpSource` — the injected, capability-scoped read seam over one bundle's
  `Help/` tree (list locale directories, read one document). The engine
  performs no ambient I/O.
- `load(source, requested, name)` — the deterministic fallback chain: the exact
  locale, then the lexicographically first same-language region holding the
  document, then `default/`. Reports which locale served (`Selection`) so
  `man` can surface a fallback on `stdinfo`; a missing document is a typed
  `NotFound`, never fabricated text.
- `HelpDoc::parse` — the bounded structured-Markdown parser: the closed,
  ordered `## NAME` … `## SEE ALSO` section set with paragraphs, `###`
  sub-headings, bullet/ordered lists (with two-space continuation lines),
  fenced code blocks, pipe tables, and `` `code` ``/`**strong**`/`*emphasis*`
  inline spans.
- `render_short` / `render_full` — the `-h`/`-?` view (`NAME`, `SYNOPSIS`,
  compact `OPTIONS`) and the whole `man` page, emitted as `lib/vt` operations
  (bold headings/code, underlined emphasis, width-padded tables via
  `lib/curses`) that the caller encodes and writes to its own stdout.

## Design

- `no_std` + `alloc`, `#![forbid(unsafe_code)]`, no
  `unwrap`/`expect`/`panic!` on any path.
- Help content is signed but parsed as hostile input: document size, line
  length/count, blocks, list items, and table dimensions are fixed security
  bounds (`MAX_*`), and any violation, control byte, unknown/duplicate/
  out-of-order section, or malformed structure rejects the whole document with
  a typed `HelpError`. Section headings inside fenced code blocks stay code.
- The renderers add no second escape vocabulary and no second width table:
  output is `rustos_vt::Op` values, widths come from `rustos_curses`.
- Fuzzed: `tests/fuzz_help.rs` (registered with `cargo xtask fuzz`) holds the
  parser total and the rendered output control-free under hostile bytes.

Paging, terminal probing, and locale discovery are deliberately out of scope:
the pager belongs to the `man` app, and the active locale is resolved once by
the session/shell and passed in.

## Stability

Tier: `experimental`.
