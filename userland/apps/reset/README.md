# `tairix-reset` — restore the terminal to a sane state

A `plans/APPS.md` command app registered at `/System/Apps/reset.app/Run`
so the shell resolves the bare word `reset` to it. `reset` undoes the
state a crashed full-screen program can leave behind: it restores the
cooked input discipline (echo on) through `stream_input_mode`, and
writes the restoration sequence — leave the alternate screen, show the
cursor, reset the graphic rendition and the scroll region, then move
home and erase the display. Which operations are written is decided by
the inherited `TERM` through the compiled-in `lib/termcap` capability
database (fail-closed: an unknown `TERM` degrades to the dumb baseline,
whose sequence is empty — the discipline restore is the whole reset
there), and every operation is encoded through the one shared `lib/vt`
vocabulary. `-h`/`-?` render the tool's own short help from its bundled
`Help/` tree through the shared `lib/help` engine, in the locale the
inherited `LANG` variable names, falling back to the usage banner when
the tree is unavailable.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
shared `tairix-termcap`, `tairix-vt`, and `tairix-help` crates (plus the
`tairix-abi` input-mode vocabulary in the freestanding binary), so it
never links a kernel or driver crate. Its manifest (`AppInfo.toml`)
requests `CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ`, and `CAP_FS_ACCESS` —
within the session baseline — and the secured VFS still authorises every
path per-inode under the caller's attested identity.

## Usage

```
reset

  -h, -?         show this command's own short help
```

## Exit status

- `0` — the terminal was restored (or short help served).
- `1` — the output could not be delivered.
- `2` — the command line was not understood.

## Stability

`stable` — the tool follows its ncurses counterpart; divergences are
deliberate and documented in its `Help/` documents.
