# `tairix-basename` — strip directory and suffix from names

A `plans/APPS.md` §12.1 Stage C command app, shipped as the self-contained
store bundle `/System/Apps/basename.app/` so the shell resolves the bare
word `basename` to it. `basename` is the GNU coreutils tool: it prints
the final component of each path spelling, optionally with a trailing
suffix removed, using the purely lexical POSIX algorithm — no operand
path is resolved, normalised, or touched on disk. The GNU surface is
implemented in full: the `NAME [SUFFIX]` operand form, `-a`/`--multiple`,
`-s`/`--suffix` (implying `-a`, in `--suffix=X`, `--suffix X`, `-sX`, and
bundled forms), `-z`/`--zero`, `--`, and option/operand permutation.
`-h`/`-?`/`--help` render the tool's own short help from its bundled
`Help/` tree through the shared `lib/help` engine, in the locale the
inherited `LANG` variable names.

One TAIRiX extension: a `Name:/` alias root (`plans/DRIVES.md`) plays
the role POSIX gives `/` — it is never stripped into, so
`basename Home:/` is `Home:/` exactly as `basename /` is `/`. Where the
root prefix ends is decided by the shared path grammar's own rule
(`tairix_path::alias_root_len`); the tool carries no second path parser.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
shared `tairix-path` and `tairix-help` crates, so it never links a kernel
or driver crate. Its manifest (`AppInfo.toml`) requests
`CAP_CONSOLE_WRITE` and `CAP_FS_ACCESS` — within the session baseline —
and the secured VFS still authorises every path per-inode under the
caller's attested identity.

## Usage

```
basename name [suffix]
basename [-az] [-s suffix] name...

  -a, --multiple        every operand is a name
  -s, --suffix <suffix> strip a trailing suffix (implies -a)
  -z, --zero            end each result with NUL, not newline
  -h, -?                show this command's own short help
```

## Exit status

- `0` — the results (or short help) were written.
- `1` — the output could not be delivered.
- `2` — the command line was not understood.

## Stability

`stable` — the tool follows its GNU coreutils counterpart; divergences are
deliberate and documented in its `Help/` documents.
