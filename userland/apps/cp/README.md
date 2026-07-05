# `rustos-cp` — copy files and directories

Stage 6 deliverable (`AGENTS.md` §3 `userland/apps/`). `cp` copies each
of its source operands to a destination. With a single source and a
destination that is not a directory, the source is copied to that exact
path. When the destination is an existing directory — and always when
there is more than one source — each source is copied *into* it under its
own base name. A directory source is copied only with `-r`, which
reproduces the whole subtree; naming a directory without `-r` is an
error. This is the POSIX model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). It
depends only on the audited `rustos-abi` crate and the shared `lib/help`
engine (plus `rustos-rt` for the freestanding `Run` binary), so it never
links a kernel or driver crate (`AGENTS.md` §17.4). The package is both
the copy library and the `cp` command app's `Run` binary
(`src/run.rs`), registered as the self-contained store bundle
`/System/Apps/cp.app` with its six-locale `Help/` tree (plans/APPS.md
§12.1 Stage B).

## Usage

```
cp [-finrRvT] [-t dir] [--] source... dest

  -r, -R, --recursive        copy directories and their contents
  -f, --force                remove an unwritable destination and retry
  -i, --interactive          ask before overwriting an existing file
  -n, --no-clobber           never overwrite an existing file
  -v, --verbose              report each copy ('src' -> 'dst')
  -t dir, --target-directory=dir
                             copy every source into dir
  -T, --no-target-directory  treat dest as a normal file (one source)
  -h, -?, --help             show this command's own short help
```

At least one source and a destination are required. Short options may be
combined (e.g. `-rf`). `--` ends option parsing: every later argument is
a path. With more than one source the destination must be a directory.

## A copy machine, not a data source

`run` asks the injected filesystem seam what each source is, streams a
regular file's bytes from source to destination, and walks each directory
`-r` must reproduce. The operations that reach the outside world are
injected seams, mirroring the other userland crates (`init`'s
`Spawner`/`Reaper`, `login`'s `Prompt`, `sysinfo`'s `Transport`, `cat`'s
`FileSource`, `ls`'s `Listing`, `rm`'s `Removal`):

- `FileSystem` — learn a path's kind, read a file's bytes and a
  directory's entries, and create directories, files, and bytes (plus
  remove a destination file for `-f`).
- `Prompt` — ask the `-i` overwrite question, fail-closed on an
  unanswerable prompt (never treated as consent).
- `Output` — write the usage banner and the `-v` reports (`cp` is otherwise silent on
  success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, recursion, and force decision
is testable without a kernel.

## Streaming and recursion

A regular file is streamed in fixed-size chunks (matching `cat`'s
granularity), so an arbitrarily large file copies with a bounded buffer.
A directory is reproduced by creating its destination (or merging into an
existing one), then copying each entry — files and nested directories
alike — under the destination. Entries are read into a list before the
copy descends, so the walk does not depend on directory indices staying
stable.

## Force

`-f` covers a destination that cannot be created (for example, an
existing read-only file): the destination is removed and the create is
retried exactly once. Without `-f`, a create failure surfaces as a
`CpError::Create` and stops the run.

## Fail closed

An unknown option, fewer than two operands, or more than one source aimed
at a non-directory destination is a `CpError::Usage` that copies nothing.
A directory source named without `-r` is a `CpError::IsDirectory`; a
directory source whose destination already exists as a non-directory is a
`CpError::NotADirectory`. An operand that cannot be inspected surfaces the
underlying `Errno` as `CpError::Stat`; an unreadable source is
`CpError::Read`; an uncreatable destination is `CpError::Create`; a failed
write is `CpError::Write`. The first failure stops the run before any
later operand, and there is no panic (`AGENTS.md` §2.9).

## Tests

`cargo test -p rustos-cp` drives the parser and the copy engine against
an in-memory tree and a recording output: the command grammar (every
option, clustered short flags, `-`/`--`, the too-few-operands and
unknown-option paths), a single file to a new path, a file copied across
the streaming-chunk boundary, an empty file, a file copied into a
directory under its base name, several files into a directory, the
several-sources-to-a-non-directory `Usage` refusal, the
directory-without-`-r` refusal, recursive reproduction of a nested tree,
a recursive merge into an existing directory, the recursive-onto-a-file
refusal, the missing-source fail-closed path, a failure stopping before a
later source, the unreadable-source / uncreatable-destination /
failed-write paths, the `-f` remove-and-retry recovery, and the
trailing-slash base-name join.

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
