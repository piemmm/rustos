# tairix-help

Shared command-help engine for TAIRiX (`lib/help`, `plans/APPS.md`).

Every application bundle may ship a `Help/` tree: one structured-Markdown
document per command or topic, under one directory per BCP-47 locale with the
mandatory `en-US/` directory as the canonical source. Three consumers read that tree —
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
  document, then the canonical `en-US/`. Reports which locale served (`Selection`) so
  `man` can surface a fallback on `stdinfo`; a missing document is a typed
  `NotFound`, never fabricated text.
- `load_raw(source, requested, name)` — the same single locale walk without
  the parse: the size-bounded raw bytes plus the `Selection`, for a caller
  that must run the parse somewhere else — `man` hands them to the
  `lib/sandbox` `helpdoc` worker so a foreign bundle's document is never
  parsed in its own process (`docs/src/security/sandbox.md`). `load` is
  `load_raw` + `HelpDoc::parse`; the walk has one definition.
- `HelpDoc::parse` — the bounded structured-Markdown parser: the closed,
  ordered `## NAME` … `## SEE ALSO` section set with paragraphs, `###`
  sub-headings, bullet/ordered lists (with two-space continuation lines),
  fenced code blocks, pipe tables, and `` `code` ``/`**strong**`/`*emphasis*`
  inline spans.
- `render_short` / `render_full` — the `-h`/`-?` view (`NAME`, `SYNOPSIS`,
  compact `OPTIONS`) and the whole `man` page, emitted as `lib/vt` operations
  (bold headings/code, underlined emphasis, width-padded tables via
  `lib/curses`) that the caller encodes and writes to its own stdout.
- `own_short_help` — a command app's own `-h`/`-?` render in one call:
  parse the raw `LANG` preference (malformed or missing degrades to the
  canonical `en-US/`), load the app's own document, render the short view, and
  return encoded `lib/vt` bytes — `None` when no document can be served, so
  the caller falls back to its own usage banner and `-h` never fails.
- `BundleHelp` (the `rt` cargo feature) — the production `HelpSource` over
  the running command app's own `/System/Apps/<word>.app/Help/` tree via the
  `tairix-rt` file wrappers, spelled from the shared `lib/abi` store/suffix
  constants. Enabled only by a freestanding `Run` binary; the engine itself
  stays seam-injected and performs no ambient I/O.
- `lint_help_trees` (the `lint` cargo feature, host-only tooling) — the one
  help-tree lint (`plans/APPS.md` §8.1) shared by `cargo xtask help-lint` and
  the `tools/syshelp` aggregator tests: spellings and parse bounds on every
  discovered document, canonical `en-US/` presence, `REQUIRED_LOCALES`
  completeness (the standing authoring set — thirteen locales from `en-US`
  to `he-IL` — defined once here and imported by every per-app switch pin;
  runtime selection never consults it, it scans the bundle's own tree),
  no translation-only documents, cross-locale `OPTIONS` switch-key drift, and
  the closed content-policy screen (whole-word matching plus a substring
  screen for the unsegmented CJK languages). Pure rows-in/violations-out;
  never linked into a TAIRiX program.

## Design

- `no_std` + `alloc`, `#![forbid(unsafe_code)]`, no
  `unwrap`/`expect`/`panic!` on any path.
- Help content is signed but parsed as hostile input: document size, line
  length/count, blocks, list items, and table dimensions are fixed security
  bounds (`MAX_*`), and any violation, control byte, unknown/duplicate/
  out-of-order section, or malformed structure rejects the whole document with
  a typed `HelpError`. Section headings inside fenced code blocks stay code.
- The renderers add no second escape vocabulary and no second width table:
  output is `tairix_vt::Op` values, widths come from `tairix_curses`.
- Fuzzed: `tests/fuzz_help.rs` (registered with `cargo xtask fuzz`) holds the
  parser total and the rendered output control-free under hostile bytes.

Paging, terminal probing, and locale discovery are deliberately out of scope:
the pager belongs to the `man` app, and the active locale is resolved once by
the session/shell and passed in.

## Stability

Tier: `experimental`.
