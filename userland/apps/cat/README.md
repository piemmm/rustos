# `rustos-cat` — concatenate files to the terminal

Stage 6 deliverable (`AGENTS.md` §3 `userland/apps/`), a `plans/APPS.md`
command app registered at `/System/Apps/cat.app/Run` so the shell
resolves the bare word `cat` to it. `cat` reads each of its sources in
order and writes the bytes to the terminal. A source is either a path or
standard input (the `-` operand, and the default when no operand is
given). The option surface is the GNU `cat` set (`AGENTS.md` §16.7):
numbering (`-n`, non-blank `-b`), blank-line squeezing (`-s`), and the
visibility markers (`-E`, `-T`, `-v`, plus the combinations `-e`, `-t`,
`-A`). `-h`/`-?` render the tool's own short help from its bundled
`Help/` tree through the shared `lib/help` engine (`plans/APPS.md` §4),
in the locale the inherited `LANG` variable names, falling back to the
usage banner when the tree is unavailable.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `rustos-abi` vocabulary and the shared
`rustos-help` engine, so it never links a kernel or driver crate
(`AGENTS.md` §17.4). Its manifest (`AppInfo.toml`) requests
`CAP_CONSOLE_WRITE`, `CAP_CONSOLE_READ`, and `CAP_FS_ACCESS` — within
the session baseline — and the secured VFS still authorises every path
per-inode under the caller's attested identity.

## Usage

```
cat [-AbeEnstTuv] [--] [file...]

  -A, --show-all          equivalent to -vET
  -b, --number-nonblank   number non-empty output lines; overrides -n
  -e                      equivalent to -vE
  -E, --show-ends         print `$` at the end of each line
  -n, --number            number output lines, continuously across sources
  -s, --squeeze-blank     suppress repeated adjacent blank lines
  -t                      equivalent to -vT
  -T, --show-tabs         print TAB as `^I`
  -u                      accepted and ignored (output is unbuffered)
  -v, --show-nonprinting  `^`/`M-` notation for control/non-ASCII bytes
  -h, -?                  show this command's own short help
```

Short options bundle as in the GNU tool (`-nE` is `-n -E`).

With no file operand, or when a file operand is `-`, `cat` reads standard
input. `--` ends option parsing: every later argument is a file path.

The bundle's thirteen-locale `Help/` tree (the canonical `en-US` plus the
`rustos_help::REQUIRED_LOCALES` translations, `plans/APPS.md` §8.1) is
authored on disk in this crate and
planted at `/System/Apps/cat.app/Help/` by the image builder from that
source (`tools/syshelp`) — never embedded in the binary
(`plans/APPS.md` §6.1).

## A stream/render machine, not a data source

`run` pulls bytes from each source in fixed-size chunks and writes them
— shaped by the render options — to the terminal. The operations that
reach the outside world are injected seams, mirroring the other userland
crates (`init`'s `Spawner`/`Reaper`, `login`'s `Prompt`, `sysinfo`'s
`Transport`):

- `FileSource` — read a byte range of a named file.
- `Input` — read the next bytes of standard input.
- `Output` — write rendered bytes to the terminal.
- `rustos_help::HelpSource` — the tool's own bundled `Help/` tree, read
  by the short-help switches.

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, streaming, and numbering
decision is testable without a kernel.

## Fail closed

An unknown option is a `CatError::Usage` that reads nothing. A source
that cannot be read surfaces the underlying `Errno` as `CatError::Read`
and stops before any later source — so a missing file never leaves a
half-written stream growing. A failed terminal write is
`CatError::Output`. There is no partial-guess path and no panic
(`AGENTS.md` §2.9); a seam that reports more bytes than the read buffer
holds is refused rather than indexed out of bounds.

## Tests

`cargo test -p rustos-cat` drives the parser and the streaming engine
against an in-memory filesystem, a buffered standard input, and a
recording output: the command grammar (every option, `-`/`--`, and the
usage-error path, bundled short flags, and the `-b`-overrides-`-n`
rule), single- and multi-file concatenation, standard-input streaming,
continuous line numbering across files and across a chunk boundary,
non-blank numbering, blank-line squeezing (including across source
boundaries and its numbering interaction), the `$`/`^I`/`^`/`M-` marker
renderings, a missing trailing newline, an empty numbered file, chunked
streaming of a multi-chunk file, the missing-file and dead-console
fail-closed paths, the short-help render from a Help document with its
usage-banner fallback, and the switch-drift pin that every locale's
`OPTIONS` section documents exactly the parser's switches
(`plans/APPS.md` §3.1).

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
