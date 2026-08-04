# `tairix-head` — output the first part of files

A `plans/APPS.md` §12.1 Stage C command app, shipped as the self-contained
store bundle `/System/Commands/head.app/` so the shell resolves the bare word
`head` to it. `head` is the GNU coreutils tool: it prints the first 10
lines of each file operand (or standard input) to standard output, with
`==> file <==` headers between multiple files. The GNU surface is
implemented in full: `-n`/`--lines` and `-c`/`--bytes` with the leading
`-` elide-from-the-end form and the multiplier suffix alphabet (`b`,
`kB`, `K`, `MB`, `M`, …, `iB` forms), `-q`/`--quiet`/`--silent`,
`-v`/`--verbose`, `-z`/`--zero-terminated`, `--`, short-option bundles,
option/operand permutation, and the obsolete first-argument
`-COUNT[bkm][lqvz]` form. `-h`/`-?`/`--help` render the tool's own short
help from its bundled `Help/` tree through the shared `lib/help` engine,
in the locale the inherited `LANG` variable names.

The streaming engine is constant-memory per source: the head modes stop
reading at the requested count, and the elide modes retain only the
window the semantics require (a circular byte ring for `-c -N`, a queue
of the last `N` lines for `-n -N`, whose unterminated final fragment
counts as a line exactly as in the GNU tool). A file that cannot be read
is diagnosed on standard error and the run continues with the next file,
with the exit status reflecting the failure.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
audited `tairix-abi` vocabulary and the shared `tairix-help` engine, so
it never links a kernel or driver crate. Its manifest (`AppInfo.toml`)
requests `CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ`, and `CAP_FS_ACCESS` —
within the session baseline — and the secured VFS still authorises every
path per-inode under the caller's attested identity.

## Usage

```
head [-qvz] [-c [-]num[suffix]] [-n [-]num[suffix]] [--] [file...]

  -c, --bytes <num>      print the first num bytes; -num elides the tail
  -n, --lines <num>      print the first num lines; -num elides the tail
  -q, --quiet, --silent  never print the ==> file <== headers
  -v, --verbose          always print the ==> file <== headers
  -z, --zero-terminated  NUL-delimited lines
  -h, -?                 show this command's own short help
```

## Exit status

- `0` — every file was printed (or the short help was written).
- `1` — a file could not be read, or the output could not be delivered.
- `2` — the command line was not understood.

## Stability

`stable` — the tool follows its GNU coreutils counterpart; divergences are
deliberate and documented in its `Help/` documents.
