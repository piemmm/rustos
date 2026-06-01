# `rustos-chown` — change file owner and group

Stage 6 deliverable (`AGENTS.md` §3 `userland/apps/`). `chown` applies an
ownership change to each of its file operands. The owner operand is
`OWNER`, `OWNER:GROUP`, or `:GROUP`, where `OWNER` and `GROUP` are
**decimal** user/group ids: `OWNER` changes only the owning user,
`:GROUP` only the owning group, and `OWNER:GROUP` both. With `-R` a
directory operand is changed and then its contents are changed
recursively. This is the POSIX model, restricted to numeric ids.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

## Usage

```
chown [-R] [--] OWNER[:GROUP] file...

  -R, --recursive  change files and directories recursively
  -h, --help       show the usage banner
```

An owner spec and at least one file are required. `--` ends option
parsing: every later argument is an operand. POSIX `chown` spells
recursive `-R`; a bare `-r` is not an option.

## The owner grammar

`OWNER` and `GROUP` are decimal ids. The accepted forms are:

- `OWNER` — change only the owning user, leaving the group.
- `OWNER:GROUP` — change both.
- `:GROUP` — change only the owning group.

A name (rather than a numeric id) is not accepted: RustOS has no
name-to-id seam in this tool, so resolving names would be interface creep
(`AGENTS.md` §2.4). An empty spec, a bare `:`, and a trailing-colon
`OWNER:` (which on POSIX systems means "the user's login group", and has
no meaning without a name database) are all rejected rather than guessed
(`AGENTS.md` §2.1).

## An ownership-changing machine, not a data source

`run` asks the injected filesystem seam for each operand's kind, applies
the new owner, and walks each directory `-R` must descend (changing the
directory before its contents). The operations that reach the outside
world are injected seams, mirroring the other userland crates (`cat`'s
`FileSource`, `ls`'s `Listing`, `rm`'s `Removal`, `cp`'s and `mv`'s
`FileSystem`, `chmod`'s `FileSystem`):

- `FileSystem` — learn a path's kind, set its owner, and read a
  directory's entries (for `-R`). A child's kind is carried in its
  directory entry, so the recursion never re-inspects it.
- `Output` — write the usage banner to the terminal (`chown` is silent on
  success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, owner-spec, and recursion
decision is testable without a kernel.

## Fail closed

An unknown option or a missing operand is a `ChownError::Usage` that
changes nothing; an owner operand that is not a valid
`OWNER`/`OWNER:GROUP`/`:GROUP` spec is a `ChownError::BadOwner`. An
operand that cannot be inspected surfaces the underlying `Errno` as
`ChownError::Stat`; an owner that cannot be applied is
`ChownError::Apply`; a directory whose entries cannot be read during a
recursive descent is `ChownError::Read`. The first failure stops the run
before any later operand, and there is no panic (`AGENTS.md` §2.9).

## Tests

`cargo test -p rustos-chown` drives the parser and the engine against an
in-memory tree and a recording output: the command grammar (every owner
form, the recursive flag, the `-r`-is-not-recursive and unknown-option
refusals, `--`, the too-few-operands and bad-owner paths), the owner-spec
parser (the three valid forms, the empty/`:`/trailing-colon refusals, and
the non-decimal / overflow / multi-colon refusals), an owner-only change
leaving the group, an owner:group change, a group-only change leaving the
user, several files, a non-recursive directory change leaving its
contents alone, a recursive change touching the directory before its
contents, and the missing-operand / stat / apply / read-during-recursion
fail-closed paths.

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
