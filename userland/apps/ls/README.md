# `rustos-ls` — list directory contents

Stage 6 deliverable (`AGENTS.md` §3 `userland/apps/`). `ls` inspects each
of its path operands in order. A non-directory operand is listed by name;
a directory operand has its entries listed, sorted by name. With no
operand it lists the current directory (`.`). With `-a` it includes
entries whose name begins with `.`; with `-l` it prints the long format —
the type and permission bits, the size, then the name — the POSIX model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

## Usage

```
ls [-a] [-l] [--] [path...]

  -a, --all    do not hide entries whose name begins with `.`
  -l, --long   long format: type and permission bits, size, then name
  -h, --help   show the usage banner
```

With no path operand `ls` lists the current directory. Short options may
be combined (e.g. `-la`). `--` ends option parsing: every later argument
is a path.

## A render machine, not a data source

`run` asks the injected filesystem seam for the metadata of each operand
and the entries of each directory, then writes the sorted, formatted
listing to the terminal. The two operations that reach the outside world
are injected seams, mirroring the other userland crates (`init`'s
`Spawner`/`Reaper`, `login`'s `Prompt`, `sysinfo`'s `Transport`, `cat`'s
`FileSource`):

- `Listing` — stat a path and read a directory's entries by index.
- `Output` — write the rendered listing to the terminal.

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, filtering, sorting, and
formatting decision is testable without a kernel.

## Layout

When several operands are given, non-directory operands are listed first
(sorted by name), then each directory operand has its entries listed,
preceded by a `path:` header and separated from the previous block by a
blank line. A single directory operand is listed without a header.

## Fail closed

An unknown option is a `LsError::Usage` that inspects nothing. An operand
that cannot be stat'd surfaces the underlying `Errno` as `LsError::Stat`
and stops before any later operand. A directory that cannot be read is
`LsError::Read`. A failed terminal write is `LsError::Output`. There is
no partial-guess path and no panic (`AGENTS.md` §2.9).

## Tests

`cargo test -p rustos-ls` drives the parser and the listing engine
against an in-memory tree and a recording output: the command grammar
(every option, clustered short flags, `-`/`--`, and the usage-error
path), sorted directory listing, the hidden-file filter with and without
`-a`, a non-directory operand, the long format's mode string and
right-aligned size (across all four entry kinds), single- and
multi-operand layout (files first, then directory headers), an empty
directory, and the missing-operand, unreadable-directory, and
dead-console fail-closed paths.

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
