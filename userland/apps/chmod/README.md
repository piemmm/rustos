# `rustos-chmod` — change file mode bits

Stage 6 deliverable (`AGENTS.md` §3 `userland/apps/`). `chmod` applies a
mode to each of its file operands. The mode is either an absolute octal
value (`644`, `0755`, …) that replaces the permission bits outright, or a
comma-separated list of symbolic clauses (`[ugoa]*[-+=][rwxXst]*`, e.g.
`g+w`, `o-rx`, `a=rx`, `u+s`) that transform the file's current bits. With
`-R` a directory operand is changed and then its contents are changed
recursively. This is the POSIX model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

## Usage

```
chmod [-cfRv] [--] MODE file...

  -R, --recursive       change files and directories recursively
  -c, --changes         report only files whose mode actually changed
  -v, --verbose         report every file processed
  -f, --silent, --quiet suppress most error messages
  -h, --help            show the usage banner
```

A mode and at least one file are required. `--` ends option parsing:
every later argument is an operand. POSIX `chmod` spells recursive `-R`;
a bare `-r` is not an option. To set a mode that begins with `-` (for
example, "remove write for all"), write it without the dash (`a-w`) or
end option parsing first (`chmod -- -w file`).

## The mode grammar

- **Octal**: one to four octal digits set the low twelve permission bits
  (the `rwx` triples plus the setuid/setgid/sticky bits) outright; the
  current mode is irrelevant.
- **Symbolic**: comma-separated clauses, each `[ugoa]*[-+=][rwxXst]*`.
  `u`/`g`/`o` select the owner/group/other field and `a` (or an omitted
  who) selects all. `+` turns the bits on, `-` off, and `=` sets the
  selected fields to exactly those bits. Permissions are `r`, `w`, `x`,
  `X` (execute only for a directory or a file that already carries an
  execute bit), `s` (setuid/setgid), and `t` (sticky). A clause may chain
  several operator sections that share its who (e.g. `u+x-w`).

## A mode-changing machine, not a data source

`run` asks the injected filesystem seam for each operand's kind and
current mode, computes the new mode, applies it, and walks each directory
`-R` must descend. The operations that reach the outside world are
injected seams, mirroring the other userland crates (`cat`'s
`FileSource`, `ls`'s `Listing`, `rm`'s `Removal`, `cp`'s and `mv`'s
`FileSystem`):

- `FileSystem` — learn a path's kind and current mode, set its mode, and
  read a directory's entries (for `-R`).
- `Output` — write the usage banner and the `-c`/`-v` change reports
  (`chmod` is otherwise silent on
  success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, mode-algebra, and recursion
decision is testable without a kernel.

## Fail closed

An unknown option or a missing operand is a `ChmodError::Usage` that
changes nothing; a mode operand that is neither octal nor symbolic is a
`ChmodError::BadMode`. An operand that cannot be inspected surfaces the
underlying `Errno` as `ChmodError::Stat`; a mode that cannot be applied
is `ChmodError::Apply`; a directory whose entries cannot be read during a
recursive descent is `ChmodError::Read`. The first failure stops the run
before any later operand, and there is no panic (`AGENTS.md` §2.9).

## Tests

`cargo test -p rustos-chmod` drives the parser, the symbolic-mode algebra,
and the engine against an in-memory tree and a recording output: the
command grammar (octal and symbolic modes, the recursive flag, the
`-r`-is-not-recursive and unknown-option refusals, `--`, too-few-operands
and bad-mode paths), the full mode algebra (`+`/`-`/`=`, omitted-who,
conditional `X`, setuid/setgid/sticky, left-to-right clause application,
empty-perm no-ops), an octal change, a symbolic change, several files, a
non-recursive directory change leaving its contents alone, a recursive
change touching the directory before its contents, per-node `X`
resolution under recursion, and the missing-operand / stat / apply /
read-during-recursion fail-closed paths.

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
