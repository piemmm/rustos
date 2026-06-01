# `rustos-rm` — remove files and directories

Stage 6 deliverable (`AGENTS.md` §3 `userland/apps/`). `rm` removes each
of its operands in order. A non-directory operand (a regular file, a
symbolic link — removed, never followed — a device node) is unlinked. A
directory operand is removed only with `-r`, which removes its contents
depth-first and then the directory itself; naming a directory without
`-r` is an error. With `-f` an operand that does not exist is skipped
rather than reported. This is the POSIX model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

## Usage

```
rm [-r] [-f] [--] file...

  -r, -R, --recursive  remove directories and their contents
  -f, --force          ignore operands that do not exist; never prompt
  -h, --help           show the usage banner
```

At least one file operand is required unless `-f` is given (an empty
`rm -f` removes nothing and succeeds). Short options may be combined
(e.g. `-rf`). `--` ends option parsing: every later argument is a path.

## A removal machine, not a data source

`run` asks the injected filesystem seam what each operand is, walks each
directory `-r` must remove, and unlinks every reachable object. The
operations that reach the outside world are injected seams, mirroring the
other userland crates (`init`'s `Spawner`/`Reaper`, `login`'s `Prompt`,
`sysinfo`'s `Transport`, `cat`'s `FileSource`, `ls`'s `Listing`):

- `Removal` — learn a path's kind, read a directory's entries by index,
  and remove a file or an emptied directory.
- `Output` — write the usage banner to the terminal (`rm` is silent on
  success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, recursion, and force decision
is testable without a kernel.

## Recursion order

`rm -r` removes a directory's contents depth-first — files and nested
directories before the directory that holds them — so a parent is never
unlinked while it still has children. Entries are read into a list before
removal begins, so the walk does not depend on directory indices staying
stable as objects disappear.

## Fail closed

An unknown option (or no operand without `-f`) is a `RmError::Usage` that
removes nothing. A directory named without `-r` is a
`RmError::IsDirectory`. An operand that cannot be inspected surfaces the
underlying `Errno` as `RmError::Stat` — except that `-f` makes a
`NotFound` a silent no-op, while still surfacing any other errno (e.g.
`PermissionDenied`). A directory that cannot be enumerated is
`RmError::Read`; a failed unlink is `RmError::Remove`. The first failure
stops the run before any later operand, and there is no panic
(`AGENTS.md` §2.9).

## Tests

`cargo test -p rustos-rm` drives the parser and the removal engine
against an in-memory tree and a recording output: the command grammar
(every option, clustered short flags, `-`/`--`, the no-operand and
usage-error paths), a single file, several files in order, the
directory-without-`-r` refusal, recursive depth-first removal of a nested
tree (asserting contents are unlinked before their directory), an empty
directory, the missing-operand fail-closed path and the `-f` skip, the
`-f`-does-not-mask-permission guarantee, a failure stopping before a
later operand, the unreadable-directory and failed-unlink paths, and the
trailing-slash path join.

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
