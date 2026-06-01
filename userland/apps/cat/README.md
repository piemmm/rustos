# `rustos-cat` — concatenate files to the terminal

Stage 6 deliverable (`AGENTS.md` §3 `userland/apps/`). `cat` reads each
of its sources in order and writes the bytes to the terminal. A source is
either a path or standard input (the `-` operand, and the default when no
operand is given). With `-n` it numbers the output lines, continuously
across every source — the POSIX model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

## Usage

```
cat [-n] [--] [file...]

  -n, --number   number output lines, continuously across every source
  -h, --help     show the usage banner
```

With no file operand, or when a file operand is `-`, `cat` reads standard
input. `--` ends option parsing: every later argument is a file path.

## A stream/render machine, not a data source

`run` pulls bytes from each source in fixed-size chunks and writes them —
optionally line-numbered — to the terminal. The three operations that
reach the outside world are injected seams, mirroring the other userland
crates (`init`'s `Spawner`/`Reaper`, `login`'s `Prompt`, `sysinfo`'s
`Transport`):

- `FileSource` — read a byte range of a named file.
- `Input` — read the next bytes of standard input.
- `Output` — write rendered bytes to the terminal.

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
usage-error path), single- and multi-file concatenation, standard-input
streaming, continuous line numbering across files and across a chunk
boundary, a missing trailing newline, an empty numbered file, chunked
streaming of a multi-chunk file, and the missing-file and dead-console
fail-closed paths.

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
