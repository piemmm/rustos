# `tairix-clear` — clear the terminal screen

A `plans/APPS.md` command app registered at `/System/Commands/clear.app/Run`
so the shell resolves the bare word `clear` to it. `clear` writes the
byte sequence that moves the cursor home and erases the display — the
ncurses `clear` model. Which bytes those are is decided by the inherited
`TERM` through the compiled-in `lib/termcap` capability database
(fail-closed: an unknown `TERM` degrades to the dumb baseline, which
cannot clear, and the tool reports that honestly instead of printing
escape garbage), and the sequence is encoded through the one shared
`lib/vt` vocabulary. `-x` is accepted for GNU/ncurses compatibility
("do not clear the scrollback"); a TAIRiX console keeps no scrollback,
so the output is identical with and without it. `-h`/`-?` render the
tool's own short help from its bundled `Help/` tree through the shared
`lib/help` engine, in the locale the inherited `LANG` variable names,
falling back to the usage banner when the tree is unavailable.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
shared `tairix-termcap`, `tairix-vt`, and `tairix-help` crates, so it
never links a kernel or driver crate. Its manifest (`AppInfo.toml`)
requests `CAP_CONSOLE_WRITE` and `CAP_FS_ACCESS` — within the session
baseline — and the secured VFS still authorises every path per-inode
under the caller's attested identity.

## Usage

```
clear [-x]

  -x             ignored (a TAIRiX console keeps no scrollback)
  -h, -?         show this command's own short help
```

## Exit status

- `0` — the clear sequence was written (or short help served).
- `1` — the terminal cannot clear, or the output could not be delivered.
- `2` — the command line was not understood.

## Stability

`stable` — the tool follows its GNU/ncurses counterpart; divergences are
deliberate and documented in its `Help/` documents.
