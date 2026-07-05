# `rustos-yes` — repeatedly output a line of text

A `plans/APPS.md` §12.1 Stage C command app, shipped as the self-contained
store bundle `/System/Apps/yes.app/` so the shell resolves the bare word
`yes` to it. `yes` is the GNU coreutils tool: it writes its operands,
joined by single spaces — or `y` when none are given — followed by a
newline, over and over until its output stops accepting bytes (a closed
pipe) or the process is terminated. Option handling matches GNU: an
unrecognised option is a usage error, option scanning stops at the first
operand, and `yes -- -x` prints `-x`. `-h`/`-?`/`--help` render the
tool's own short help from its bundled `Help/` tree through the shared
`lib/help` engine, in the locale the inherited `LANG` variable names,
falling back to the usage banner when the tree is unavailable.

The output is built once into a bounded block (whole lines, up to 4 KiB)
so the endless writer pays one write per block rather than one per line;
a full stream backing blocks the write kernel-side, so the tool never
idle-spins. The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its only dependency is
the shared `rustos-help` crate, so it never links a kernel or driver
crate. Its manifest (`AppInfo.toml`) requests `CAP_CONSOLE_WRITE` and
`CAP_FS_ACCESS` — within the session baseline — and the secured VFS still
authorises every path per-inode under the caller's attested identity.

## Usage

```
yes [string...]

  -h, -?         show this command's own short help
  --             end option parsing (`yes -- -x` prints `-x`)
```

## Exit status

- `0` — a requested short help was served.
- `1` — the output stopped accepting bytes (the tool's one stop
  condition).
- `2` — the command line was not understood.

## Stability

`stable` — the tool follows its GNU coreutils counterpart; divergences are
deliberate and documented in its `Help/` documents.
