# `rustos-mv` — move (rename) files and directories

Stage 6 deliverable (`AGENTS.md` §3 `userland/apps/`). `mv` relocates
each of its source operands to a destination. With a single source and a
destination that is not a directory, the source is moved to that exact
path. When the destination is an existing directory — and always when
there is more than one source — each source is moved *into* it under its
own base name. Unlike `cp`, a directory needs no flag: a directory is
moved like any other operand. This is the POSIX model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). It
depends only on the audited `rustos-abi` crate and the shared `lib/help`
engine (plus `rustos-rt` for the freestanding `Run` binary), so it never
links a kernel or driver crate (`AGENTS.md` §17.4). The package is both
the move library and the `mv` command app's `Run` binary
(`src/run.rs`), registered as the self-contained store bundle
`/System/Apps/mv.app` with its thirteen-locale `Help/` tree (plans/APPS.md
§12.1 Stage B). The production seam maps the kernel's dedicated
`Errno::CrossVolume` rename refusal (the `EXDEV` equivalent) onto the
copy-then-remove fallback.

## Usage

```
mv [-finvT] [-t dir] [--] source... dest

  -f, --force                remove a blocking destination and retry the
                             rename; never prompt
  -i, --interactive          ask before overwriting an existing destination
  -n, --no-clobber           never overwrite an existing destination
  -v, --verbose              report each move (renamed 'src' -> 'dst')
  -t dir, --target-directory=dir
                             move every source into dir
  -T, --no-target-directory  treat dest as a normal file (one source)
  -h, -?, --help             show this command's own short help
```

At least one source and a destination are required. Short options may be
combined (e.g. `-fn`). `--` ends option parsing: every later argument is
a path. With more than one source the destination must be a directory.

## A move machine, not a data source

`run` asks the injected filesystem seam what each source is, then asks it
to `rename` the source onto its destination. The operations that reach
the outside world are injected seams, mirroring the other userland crates
(`cat`'s `FileSource`, `ls`'s `Listing`, `rm`'s `Removal`, `cp`'s
`FileSystem`):

- `FileSystem` — learn a path's kind, rename a path, read a file's bytes
  and a directory's entries, create directories/files/bytes, and remove
  files and directories (for the cross-device relocation and for `-f`).
- `Prompt` — ask the `-i` overwrite question, fail-closed on an
  unanswerable prompt (never treated as consent).
- `Output` — write the usage banner and the `-v` reports (`mv` is otherwise silent on
  success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every routing and fallback decision is
testable without a kernel.

## Cross-device relocation

A rename cannot be atomic when its source and destination live on
different filesystems. Rather than overload an `Errno`, the `rename` seam
reports that case as an explicit `RenameOutcome::CrossDevice` outcome
(`AGENTS.md` §2.11). The engine then performs the POSIX relocation: it
copies the source to the destination (streaming a regular file in
fixed-size chunks, reproducing a directory subtree) and only then removes
the source, depth-first. A failure during the copy leaves the source in
place.

## No-clobber and force

`-n` never overwrites: a source whose destination already exists is
skipped silently. `-f` covers a destination that blocks the rename: the
destination is removed and the rename retried exactly once. Without
either flag an existing destination is overwritten, the default POSIX
behaviour.

## Fail closed

An unknown option, fewer than two operands, or more than one source aimed
at a non-directory destination is an `MvError::Usage` that moves nothing.
An operand that cannot be inspected surfaces the underlying `Errno` as
`MvError::Stat`; a rename that fails for a non-boundary reason is
`MvError::Rename`. During a cross-device relocation an unreadable source
is `MvError::Read`, an uncreatable destination is `MvError::Create`, a
failed write is `MvError::Write`, and a source that cannot be removed
after a copy is `MvError::Remove`. The first failure stops the run before
any later operand, and there is no panic (`AGENTS.md` §2.9).

## Tests

`cargo test -p rustos-mv` drives the parser and the move engine against
an in-memory tree and a recording output: the command grammar, a file
renamed to a new path, a directory renamed, a file moved into a directory
under its base name, several files into a directory, the
several-sources-to-a-non-directory `Usage` refusal, the missing-source
fail-closed path, a failure stopping before a later source, `-n` skipping
an existing destination, the default overwrite, the failed-rename path,
the blocking-destination refusal and its `-f` remove-and-retry recovery,
the cross-device file / large-file / directory relocations, the
cross-device read/write/remove fail-closed paths, and the trailing-slash
base-name join.

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
