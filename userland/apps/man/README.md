# `tairix-man` — show a command's help document

`plans/APPS.md` §7 deliverable (`AGENTS.md` §3 `userland/apps/`). TAIRiX
ships no troff/roff man pages: every program is a `<Name>.app` bundle whose
single internationalised `Help/` tree holds one structured-Markdown document
per command or topic (`AGENTS.md` §16.5). `man <cmd>` resolves `<cmd>`
through the **same** store-then-`PATH` policy the shell launches by
(`lib/cmdres`), so the page shown always documents the program the shell
would run; when no ordered candidate matches a bare word it falls back to
the bounded, breadth-first **recursive bundle search** of the app stores —
the machine-wide `/Apps`, then the user's own `<HOME>/Apps`
(`tairix_cmdres::search_roots`) — finding a bundle's help however deeply
it was filed (never descending into another `.app`; an exhausted directory
budget is reported, never silently "not found"). It then renders that
bundle's document in the active locale through
the one shared help engine (`lib/help`).

The document itself is **never parsed in `man`'s own process**: it is
foreign bundle content, so `man` locates and reads it with its own file
authority (`tairix_help::load_raw`, the same one locale walk `load`
wraps) and hands the raw bytes to a minimum-capability parser-sandbox
worker (`tairix_sandbox::helpdoc` — `man`'s own binary re-spawned,
`CAP_PROC_SPAWN` in its manifest). Only the whitelist-validated render
comes back (printable text, line feeds, and the standard colour scheme's
emphasis and colour SGRs); a crashed or hostile worker costs the page — the typed
`ManError::Render`, no in-process fallback — and `-h` degrades to the
usage banner (`docs/src/security/sandbox.md`).

The crate is `no_std` + `alloc`, forbids `unsafe`, and has no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `tairix-abi` crate and the shared
`tairix-cmdres` / `tairix-help` / `tairix-sandbox` / `tairix-log`
engines, so it never links a kernel or driver crate (`AGENTS.md` §17.4)
and defines no second resolution policy, locale walker, or escape
vocabulary (`AGENTS.md` §2.2).

## Usage

```
man [-h | -?] <command> [topic]

  <command>   the command whose Help document to render
  [topic]     a named topic within that command's bundle
  -h, -?      show man's own short help
```

`--` ends option parsing. A trailing `.app` on the word names the bundle
directly (`man top.app` shows `top`'s page). Exit codes: `0` page shown,
`1` command/document not found or delivery failed, `2` usage error.

## Locale selection

The requested locale is the `LANG` environment variable (a BCP-47 tag, set
once by the session/shell — `plans/APPS.md` §5). The engine falls back
deterministically: exact tag → same language, any region → the canonical
`en-US/` document — resolved by scanning the bundle's own `Help/` tree for
the locales it actually ships, never a compiled-in language list, so a
bundle carrying only `en-US/` still serves help. A malformed or missing
`LANG` degrades to
`en-US/` rather than making help unreadable. When the served locale is not
the requested one, `man` emits a `context` advisory record on `stdinfo`
(fd 3, code `help.locale_fallback`) — advisory only, never affecting output
or exit status (`AGENTS.md` §20.1).

The **section headings display in the served page's language** (`NOM`,
`BESCHREIBUNG`, `説明`, …): the document keys stay the language-neutral
`## NAME` … `## SEE ALSO`, but a reader of a French page sees French
headings over French prose, and an untranslated language degrades to the
English keys.

## Rendering and colour

The page is rendered through the one standard TAIRiX terminal colour scheme
(`tairix_vt::scheme`, `plans/APPS.md` §12.2): headings and sub-headings in
the heading role, `*emphasis*` in the emphasis role, inline code and fenced
blocks in the literal role, `**strong**` bold, and table rules in the border
role. Colour is render-to-terminal only — a redirected or piped page
(`man ls | cat`) is plain text with no escapes — and degrades through the
one `tairix_termcap` `TERM`→capability judgement: an attested colour
terminal gets the full scheme, an attested monochrome one the emphasis
attributes only, and an unattested output plain text. The information
survives with every attribute stripped, so colour is never the sole carrier
of a distinction.

## Pagination

On a console whose geometry the kernel attests (`terminal_size`), the page
is shown a screenful at a time: space turns the page, return advances one
line, `q` stops; local echo is suppressed while the pager can prompt and
restored on exit. A screenful is counted in **physical** rows, not
newlines: a line wider than the terminal wraps onto several rows and the
pager splits the page at exactly the columns the terminal wraps at
(measuring display width, skipping the zero-width colour/emphasis escapes),
so long lines no longer scroll off the top before the `--More--` prompt
appears. On a serial line, a pipe, or a redirection the whole page
streams — the remote emulator or consumer owns the scrollback.

## Seams and tests

The two effectful edges are injected traits: `BundleStore` (bundle probe,
locale-directory listing, bounded document reads) and `Console` (stdout,
fd 3, terminal geometry — rows and columns — and keys). The `Run` binary
also reads `TERM` from the environment for the colour decision, and wires
these to the kernel-authorised
`fs_*` syscalls and the inherited standard streams; every capability and
per-inode check stays kernel-side (`AGENTS.md` §5.4) and a candidate probe
mirrors the shell's launch rule — `NotFound` moves on, any other refusal is
final. Unit tests drive the whole engine against in-memory fixtures;
`src/help.rs` embeds the bundle's own `Help/` tree (`en-US` + the required
locales) and proves every shipped document parses, and the same table is
what `tools/mkimage` and the QEMU image fixture plant on the read-only
`/System` volume, so image and source cannot drift (`AGENTS.md` §2.2). The
`session_ceiling` QEMU vertical types `man man` end to end.

## Required capabilities

`CAP_CONSOLE_WRITE` (the page on fd 1), `CAP_CONSOLE_READ` (pager keys +
echo suppression), and `CAP_FS_ACCESS` (reading `Help/` documents — the
secured VFS still authorises every path per-inode).
