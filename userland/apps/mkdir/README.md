# `rustos-mkdir` — make directories

A `plans/APPS.md` §12.1 Stage C command app, shipped as the self-contained
store bundle `/System/Apps/mkdir.app/` so the shell resolves the bare
word `mkdir` to it. `mkdir` is the GNU coreutils tool: it creates each
directory operand through the kernel's `fs_mkdir` under the caller's
attested identity. The implemented GNU surface is `-p`/`--parents`
(create missing ancestors, tolerate an operand that is already a
directory), `-v`/`--verbose` (GNU-worded `mkdir: created directory 'dir'`
reports), and `--`. `-h`/`-?`/`--help` render the tool's own short help
from its bundled `Help/` tree through the shared `lib/help` engine, in
the locale the inherited `LANG` variable names.

GNU `mkdir`'s `-m`/`--mode` is deliberately not accepted yet: the
`fs_mkdir` syscall carries no creation mode, so the switch lands with the
mode-set kernel work `chmod` is staged on (`plans/APPS.md` §12.1
Stage B) — never as a stub that silently ignores its argument.

The `-p` ancestor walk spells each prefix through the shared path
grammar's own rule (`rustos_path::Path::prefix`), so an alias-rooted
operand (`Home:/tools/bin`) walks correctly and the tool carries no
second path parser; an operand the grammar cannot parse is handed to the
kernel whole, which stays the one validator.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
shared `rustos-abi`, `rustos-path`, and `rustos-help` crates, so it never
links a kernel or driver crate. Its manifest (`AppInfo.toml`) requests
`CAP_CONSOLE_WRITE` and `CAP_FS_ACCESS` — within the session baseline —
and the secured VFS still authorises every creation per-inode under the
caller's attested identity.

## Usage

```
mkdir [-pv] [--] directory...

  -p, --parents   make missing parent directories; an operand that is
                  already a directory is not an error
  -v, --verbose   report each created directory
  -h, -?          show this command's own short help
```

## Exit status

- `0` — every directory was created (or, under `-p`, already existed).
- `1` — a filesystem or output failure.
- `2` — the command line was not understood.

## Stability

`stable` — the tool follows its GNU coreutils counterpart; divergences are
deliberate and documented in its `Help/` documents.
