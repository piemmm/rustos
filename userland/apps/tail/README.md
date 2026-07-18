# `tairix-tail` — output the last part of files

A `plans/APPS.md` §12.1 Stage C command app, shipped as the self-contained
store bundle `/System/Apps/tail.app/` so the shell resolves the bare word
`tail` to it. `tail` is the GNU coreutils tool: it prints the last 10 lines
of each file operand (or standard input) to standard output, with
`==> file <==` headers between multiple files. The GNU surface implementable
on the current platform floor is implemented in full: `-n`/`--lines` and
`-c`/`--bytes` with the leading `+` "start at unit N" form (and the `-` /
plain "last N" form) and the multiplier suffix alphabet (`b`, `kB`, `K`,
`MB`, `M`, …, `iB` forms), `-q`/`--quiet`/`--silent`, `-v`/`--verbose`,
`-z`/`--zero-terminated`, `--`, short-option bundles, option/operand
permutation, and the obsolete first-argument `{+,-}COUNT[bcl]` form.
`-h`/`-?`/`--help` render the tool's own short help from its bundled
`Help/` tree through the shared `lib/help` engine, in the locale the
inherited `LANG` variable names.

Follow mode (`-f`, `-F`, `--follow`, `--retry`, `--pid`,
`--sleep-interval`, `--max-unchanged-stats`, and the obsolete trailing `f`)
is deliberately **staged, not stubbed**: following a growing file needs a
kernel wake source that fires on a file change, which the userland runtime
does not yet expose. A tight poll loop would be the busy-wait the charter
forbids, and a silent no-op would misrepresent the switch, so the follow
switches are reported as unrecognised options until that wake source exists
(the same staging the `tee -i` and `mkdir -m` switches use).

The streaming engine is constant-memory per source: the last-N modes retain
only the window the format requires (a circular byte ring for `-c N`, a
queue of the last `N` lines for `-n N`, whose unterminated final fragment
counts as a line exactly as in the GNU tool, both from the shared
`tairix-util` `tailwindow`), and the from-start modes skip the leading
units and then stream. When it drops leading content, `tail` emits one
advisory `omission` record on the standard information stream (fd 3) —
ignorable by contract, never affecting the output or the exit status. A
file that cannot be read is diagnosed on standard error and the run
continues with the next file, with the exit status reflecting the failure.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths. Its dependencies are the
audited `tairix-abi` vocabulary, the shared `tairix-help` engine, and the
shared `tairix-util` count parser and rolling windows, so it never links a
kernel or driver crate. Its manifest (`AppInfo.toml`) requests
`CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ`, and `CAP_FS_ACCESS` — within the
session baseline — and the secured VFS still authorises every path
per-inode under the caller's attested identity.

## Usage

```
tail [-qvz] [-c [+]num[suffix]] [-n [+]num[suffix]] [--] [file...]

  -c, --bytes <num>      print the last num bytes; +num starts at byte num
  -n, --lines <num>      print the last num lines; +num starts at line num
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

`stable` — the tool follows its GNU coreutils counterpart; divergences (the
staged follow family) are deliberate and documented in its `Help/`
documents.
