# `rustos-man` — show a command's help document

`plans/APPS.md` §7 deliverable (`AGENTS.md` §3 `userland/apps/`). RustOS
ships no troff/roff man pages: every program is a `<Name>.app` bundle whose
single internationalised `Help/` tree holds one structured-Markdown document
per command or topic (`AGENTS.md` §16.5). `man <cmd>` resolves `<cmd>`
through the **same** store-then-`PATH` policy the shell launches by
(`lib/cmdres`), so the page shown always documents the program the shell
would run, then renders that bundle's document in the active locale through
the one shared help engine (`lib/help`).

The crate is `no_std` + `alloc`, forbids `unsafe`, and has no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `rustos-abi` crate and the shared
`rustos-cmdres` / `rustos-help` / `rustos-vt` engines, so it never links a
kernel or driver crate (`AGENTS.md` §17.4) and defines no second resolution
policy, locale walker, or escape vocabulary (`AGENTS.md` §2.2).

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
`en-US/` document. A malformed or missing `LANG` degrades to
`en-US/` rather than making help unreadable. When the served locale is not
the requested one, `man` emits a `context` advisory record on `stdinfo`
(fd 3, code `help.locale_fallback`) — advisory only, never affecting output
or exit status (`AGENTS.md` §20.1).

## Pagination

On a console whose geometry the kernel attests (`terminal_size`), the page
is shown a screenful at a time: space turns the page, return advances one
line, `q` stops; local echo is suppressed while the pager can prompt and
restored on exit. On a serial line, a pipe, or a redirection the whole page
streams — the remote emulator or consumer owns the scrollback.

## Seams and tests

The two effectful edges are injected traits: `BundleStore` (bundle probe,
locale-directory listing, bounded document reads) and `Console` (stdout,
fd 3, rows, keys). The `Run` binary wires them to the kernel-authorised
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
