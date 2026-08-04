# `tairix-rmdir` — remove empty directories

A `plans/APPS.md` §12.1 Stage C command app, shipped as the self-contained
store bundle `/System/Commands/rmdir.app/` so the shell resolves the bare
word `rmdir` to it. `rmdir` is the GNU coreutils tool: it removes each
(empty) directory operand through the kernel's **directory-only**
`fs_unlink` (`UnlinkFlags::DIRECTORY`) under the caller's attested
identity — the filesystem decides the node's kind atomically in the same
locked walk that removes it, so a concurrent swap of the directory for a
file can never unlink the file (the `rmdir(2)` guarantee, with no
stat/remove race in this program). The implemented GNU surface is
`-p`/`--parents` (remove ancestors, innermost first),
`--ignore-fail-on-non-empty` (tolerate exactly the `Errno::NotEmpty`
refusal), `-v`/`--verbose` (GNU-worded `rmdir: removing directory, 'dir'`
reports), and `--`. `-h`/`-?`/`--help` render the tool's own short help
from its bundled `Help/` tree through the shared `lib/help` engine, in
the locale the inherited `LANG` variable names.

The `-p` ancestor walk spells each prefix through the shared path
grammar's own rule (`tairix_path::Path::prefix`), so an alias-rooted
operand (`Home:/tools/bin`) walks correctly, the bare root is never
asked to be removed, and the tool carries no second path parser.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
shared `tairix-abi`, `tairix-path`, and `tairix-help` crates, so it never
links a kernel or driver crate. Its manifest (`AppInfo.toml`) requests
`CAP_CONSOLE_WRITE` and `CAP_FS_ACCESS` — within the session baseline —
and the secured VFS still authorises every removal per-inode under the
caller's attested identity.

## Usage

```
rmdir [-pv] [--ignore-fail-on-non-empty] [--] directory...

  -p, --parents               remove each operand's ancestors too,
                              innermost first
  -v, --verbose               report each removal attempt
  --ignore-fail-on-non-empty  a directory that is not empty is not an
                              error
  -h, -?                      show this command's own short help
```

## Exit status

- `0` — every removal succeeded (a tolerated non-empty refusal is not a
  failure).
- `1` — a filesystem or output failure.
- `2` — the command line was not understood.

## Stability

`stable` — the tool follows its GNU coreutils counterpart; divergences are
deliberate and documented in its `Help/` documents.
