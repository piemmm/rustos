# Core CLI utilities (`userland/apps` and `userland/shell`)

Stage 6 ships a set of small command-line utilities, each its own crate.
This page documents the ones that have landed (`sysinfo`, `cat`, `ls`,
`rm`, and `cp`) and is extended as the others (`ps`, `mount`, …) arrive.

## `sysinfo` — the System Information CLI (`userland/shell/sysinfo`)

`rustos-sysinfo` is the single command-line tool that exposes the System
Information API to the terminal (`AGENTS.md` §16.6). RustOS has no
`/proc` and no `/sys`; every piece of live system information is served
by `/System/Services/sysinfod` over the typed, versioned, capability-
checked `sysinfo-v1` wire surface defined in `rustos_abi::sysinfo` (see
[System Information API (`sysinfo-v1`)](../abi/sysinfo.md) and the
[System Information service](./sysinfod.md)). `sysinfo` is a *client* of
that API: it does **not** read a virtual filesystem, and there is no
privileged path that bypasses the capability check.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Commands

| Command              | Query                 | Capability           |
|----------------------|-----------------------|----------------------|
| `processes`          | `SELF_PROCESS_LIST`   | none                 |
| `processes --all`    | `GLOBAL_PROCESS_LIST` | `CAP_SYSINFO_GLOBAL` |
| `memory`             | `KERNEL_MEMORY_STATS` | `CAP_SYSINFO_KERNEL` |
| `hardware`           | `HARDWARE_TREE`       | `CAP_SYSINFO_HW`     |
| `identity`           | `SYSTEM_IDENTITY`     | none                 |
| `uptime`             | `UPTIME`              | none                 |
| `help` (the default) | —                     | none                 |

`processes` accepts the `-a`/`--all` flag; the other subcommands take no
arguments and `ps`/`mem`/`hw`/`id` are accepted as short aliases. The
capability gate lives in `sysinfod`, not in this tool — `sysinfo` only
ever issues the queries the frozen registry defines, never a free-form
"raw query id".

### A request/render machine, not a data source

`run` turns one parsed `Command` into a typed request and renders the
typed reply, through three steps:

1. Build the `SysinfoRequestHeader` (and, for `processes`, a
   `ProcessListRequest` payload) from the `sysinfo-v1` ABI.
2. Hand the encoded request to the injected `Transport`, which carries
   it to `sysinfod` and returns the reply bytes. The transport owns the
   reply allocation, so the client never guesses a buffer size.
3. Decode the reply with the ABI's fail-closed `from_bytes` decoders and
   write one rendered line per row to the injected `Output`.

`Transport` and `Output` are the only two operations that reach the
outside world. On a running system they are IPC- and console-backed; in
tests they are in-memory fixtures, so every rendering and paging
decision is testable without a kernel — the same seam discipline as
`init` (`Spawner`/`Reaper`) and `login` (`Prompt`).

### Paging

A process list can be longer than a single reply, so `sysinfo` pages it:
it issues `ProcessListRequest`s with an increasing `offset` and a fixed
`limit`, rendering each page, until a page comes back shorter than the
limit. The paging loop lives in the client; the ABI carries only the
`offset`/`limit` fields.

### Fail closed

- A capability denial returns from `sysinfod` as
  `Errno::PermissionDenied`, which the CLI renders as a precise "this
  query requires a capability you do not hold" diagnostic
  (`SysinfoError::PermissionDenied`) without inventing a parallel policy
  (`AGENTS.md` §2.2, §16.6).
- An unknown subcommand, an unknown flag, or a stray trailing argument
  is a `SysinfoError::Usage` that issues no query and prints the usage
  banner.
- A reply that does not decode against `sysinfo-v1` — a truncated
  scalar, or a process page whose length is not a whole number of
  records — is a hard `SysinfoError::Service` error, never a
  partially-rendered guess.

The hardware-tree wire format is owned by `lib/abi` (`AGENTS.md` §18.1)
and is not built yet, so `sysinfo hardware` honestly reports the byte
length the service returned rather than pretending to decode it
(`AGENTS.md` §2.1).

### Tests

`cargo test -p rustos-sysinfo` drives the parser and the request/render
engine against an in-memory `sysinfod` stand-in and a recording output:
the command grammar (every subcommand, alias, and the usage-error
paths), every query's rendering, process-list paging across a page
boundary, self-vs-global query routing, and the denied, malformed,
truncated, and dead-console fail-closed paths.

## `cat` — concatenate files to the terminal (`userland/apps/cat`)

`rustos-cat` is the first crate under `userland/apps/` (`AGENTS.md` §3).
It reads each of its sources in order and writes the bytes to the
terminal. A source is either a path or standard input — the `-` operand,
and the default when no operand is given. With `-n` it numbers the output
lines, continuously across every source, the POSIX model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
cat [-n] [--] [file...]
```

| Token            | Meaning                                            |
|------------------|----------------------------------------------------|
| `-n`, `--number` | number output lines, continuously across sources   |
| `-h`, `--help`   | print the usage banner (wins immediately)          |
| `--`             | end option parsing; every later argument is a path |
| `-`              | standard input                                     |
| *path*           | a file to read                                     |

With no `path` (or `-`) operand the single source is standard input. Any
other leading-dash argument before `--` is a `CatError::Usage` error,
never a silently ignored token.

### A stream/render machine, not a data source

`run` pulls bytes from each source in fixed-size chunks and writes them —
optionally line-numbered — to the terminal. The three operations that
reach the outside world are injected seams, the same discipline as
`sysinfo`'s `Transport`/`Output`:

- `FileSource` — read a byte range of a named file, streaming it with an
  advancing offset until a read returns zero (end-of-file).
- `Input` — read the next bytes of standard input until end-of-input.
- `Output` — write rendered bytes to the terminal.

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, streaming, and numbering
decision is testable without a kernel.

### Numbering

`-n` numbers each line once, when its first byte appears. The line state
is carried across read chunks and across sources, so a line that
straddles a chunk boundary — or a file boundary — is numbered exactly
once, and numbering is continuous across every source.

### Fail closed

- An unrecognised option is a `CatError::Usage` that reads nothing.
- A source that cannot be read surfaces the underlying `Errno` as
  `CatError::Read` and stops before any later source (a missing file
  among several aborts rather than skipping silently).
- A failed terminal write is `CatError::Output`.
- A seam that reports more bytes than the read buffer holds is refused
  (`CatError::Read`) rather than indexed out of bounds — no panic
  (`AGENTS.md` §2.9).

### Tests

`cargo test -p rustos-cat` drives the parser and the streaming engine
against an in-memory filesystem, a buffered standard input, and a
recording output: the command grammar (every option, `-`/`--`, and the
usage-error path), single- and multi-file concatenation, standard-input
streaming, continuous line numbering across files and across a chunk
boundary, a missing trailing newline, an empty numbered file, chunked
streaming of a multi-chunk file, and the missing-file and dead-console
fail-closed paths.

## `ls` — list directory contents (`userland/apps/ls`)

`rustos-ls` lists directory contents (`AGENTS.md` §3). It inspects each
of its path operands in order: a non-directory operand is listed by name,
and a directory operand has its entries listed, sorted by name. With no
operand it lists the current directory (`.`). With `-a` it includes
entries whose name begins with `.`; with `-l` it prints the long format —
the type and permission bits, the size, then the name — the POSIX model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
ls [-a] [-l] [--] [path...]
```

| Token          | Meaning                                            |
|----------------|----------------------------------------------------|
| `-a`, `--all`  | include entries whose name begins with `.`         |
| `-l`, `--long` | long format: type/permission bits, size, then name |
| `-h`, `--help` | print the usage banner (wins immediately)          |
| `--`           | end option parsing; every later argument is a path |
| *path*         | a file or directory to list                        |

With no `path` operand `ls` lists the current directory. Short options
may be combined into one argument (e.g. `-la` is `-l -a`); an
unrecognised letter anywhere in such a cluster is a `LsError::Usage`
error. The bare `-` is a path named `-`, not an option.

### A render machine, not a data source

`run` asks the injected filesystem seam for the metadata of each operand
and the entries of each directory, then writes the sorted, formatted
listing to the terminal in a single write. The two operations that reach
the outside world are injected seams, the same discipline as `cat`'s
`FileSource`/`Output`:

- `Listing` — stat a path (to learn whether it is a directory) and read a
  directory's entries by index until the index runs past the end.
- `Output` — write the rendered listing to the terminal.

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, filtering, sorting, and
formatting decision is testable without a kernel.

### Layout

When several operands are given, non-directory operands are listed first
(sorted by name), then each directory operand has its entries listed,
preceded by a `path:` header and separated from the previous block by a
blank line — the POSIX model. A single directory operand is listed
without a header. The short format prints one name per line; the long
format prints the ten-character mode string (a type character — `d`,
`-`, `l`, or `?` — followed by the nine `rwx` permission bits), the
size right-aligned across the listing, then the name.

### Fail closed

- An unrecognised option is a `LsError::Usage` that inspects nothing.
- An operand that cannot be stat'd surfaces the underlying `Errno` as
  `LsError::Stat` and stops before any later operand (a missing operand
  among several aborts rather than skipping silently).
- A directory that cannot be read is `LsError::Read`.
- A failed terminal write is `LsError::Output`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p rustos-ls` drives the parser and the listing engine
against an in-memory tree and a recording output: the command grammar
(every option, clustered short flags, `-`/`--`, and the usage-error
path), sorted directory listing, the hidden-file filter with and without
`-a`, a non-directory operand, the long format's mode string and
right-aligned size (across all four entry kinds), single- and
multi-operand layout (files first, then directory headers), an empty
directory, and the missing-operand, unreadable-directory, and
dead-console fail-closed paths.

## `rm` — remove files and directories (`userland/apps/rm`)

`rustos-rm` removes its operands in order (`AGENTS.md` §3). A
non-directory operand — a regular file, a symbolic link (removed, never
followed), a device node — is unlinked. A directory operand is removed
only with `-r`, which removes its contents depth-first and then the
directory itself; naming a directory without `-r` is an error. With `-f`
an operand that does not exist is skipped rather than reported. This is
the POSIX model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
rm [-r] [-f] [--] file...
```

| Token                   | Meaning                                            |
|-------------------------|----------------------------------------------------|
| `-r`, `-R`, `--recursive` | remove directories and their contents            |
| `-f`, `--force`         | ignore operands that do not exist; never prompt    |
| `-h`, `--help`          | print the usage banner (wins immediately)          |
| `--`                    | end option parsing; every later argument is a path |
| *file*                  | a file or directory to remove                      |

At least one file operand is required unless `-f` is given (an empty
`rm -f` removes nothing and succeeds). Short options may be combined into
one argument (e.g. `-rf` is `-r -f`); an unrecognised letter anywhere in
such a cluster is a `RmError::Usage` error. The bare `-` is a path named
`-`, not an option.

### A removal machine, not a data source

`run` asks the injected filesystem seam what each operand is, walks each
directory `-r` must remove, and unlinks every reachable object. The
operations that reach the outside world are injected seams, the same
discipline as `ls`'s `Listing`/`Output`:

- `Removal` — learn a path's kind, read a directory's entries by index,
  and remove a file or an emptied directory.
- `Output` — write the usage banner to the terminal (`rm` is silent on
  success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, recursion, and force decision
is testable without a kernel.

### Recursion order

`rm -r` removes a directory's contents depth-first — files and nested
directories before the directory that holds them — so a parent is never
unlinked while it still has children. Entries are read into a list before
removal begins, so the walk does not depend on directory indices staying
stable as objects disappear.

### Fail closed

- An unknown option, or no operand without `-f`, is a `RmError::Usage`
  that removes nothing.
- A directory named without `-r` is a `RmError::IsDirectory`.
- An operand that cannot be inspected surfaces the underlying `Errno` as
  `RmError::Stat`, and the run stops before any later operand. `-f` makes
  a `NotFound` a silent no-op, but still surfaces any other errno (e.g.
  `PermissionDenied`).
- A directory that cannot be enumerated is `RmError::Read`.
- A failed unlink is `RmError::Remove`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

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

## `cp` — copy files and directories (`userland/apps/cp`)

`rustos-cp` copies each of its source operands to a destination
(`AGENTS.md` §3). With a single source and a destination that is not a
directory, the source is copied to that exact path. When the destination
is an existing directory — and always when there is more than one source
— each source is copied *into* it under its own base name. A directory
source is copied only with `-r`, which reproduces the whole subtree;
naming a directory without `-r` is an error. This is the POSIX model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
cp [-r] [-f] [--] source... dest
```

| Token                     | Meaning                                            |
|---------------------------|----------------------------------------------------|
| `-r`, `-R`, `--recursive` | copy directories and their contents                |
| `-f`, `--force`           | remove an unwritable destination and retry         |
| `-h`, `--help`            | print the usage banner (wins immediately)          |
| `--`                      | end option parsing; every later argument is a path |
| *source*                  | a file or directory to copy                        |
| *dest*                    | the destination path (the last operand)            |

At least one source and a destination are required (fewer than two path
operands is a `CpError::Usage`). The last path operand is the
destination; the rest are the sources. With more than one source the
destination must be a directory. Short options may be combined into one
argument (e.g. `-rf` is `-r -f`); an unrecognised letter anywhere in such
a cluster is a `CpError::Usage`. The bare `-` is a path named `-`, not an
option.

### A copy machine, not a data source

`run` asks the injected filesystem seam what each source is, streams a
regular file's bytes from source to destination, and walks each directory
`-r` must reproduce. The operations that reach the outside world are
injected seams, the same discipline as `rm`'s `Removal`/`Output`:

- `FileSystem` — learn a path's kind, read a file's bytes and a
  directory's entries, and create directories, files, and bytes (plus
  remove a destination file for `-f`).
- `Output` — write the usage banner to the terminal (`cp` is silent on
  success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, recursion, and force decision
is testable without a kernel.

### Streaming and recursion

A regular file is streamed in fixed-size chunks (matching `cat`'s
granularity), so an arbitrarily large file copies with a bounded buffer.
A directory is reproduced by creating its destination — or merging into
an existing one — then copying each entry, files and nested directories
alike, under the destination. Entries are read into a list before the
copy descends, so the walk does not depend on directory indices staying
stable. The destination directory is created before its contents, so a
parent always exists before a child is copied into it.

### Force

`-f` covers a destination that cannot be created (for example, an
existing read-only file): the destination is removed and the create is
retried exactly once. Without `-f`, a create failure surfaces as a
`CpError::Create` and stops the run.

### Fail closed

- An unknown option, fewer than two operands, or more than one source
  aimed at a non-directory destination is a `CpError::Usage` that copies
  nothing.
- A directory source named without `-r` is a `CpError::IsDirectory`; a
  directory source whose destination already exists as a non-directory is
  a `CpError::NotADirectory`.
- An operand that cannot be inspected surfaces the underlying `Errno` as
  `CpError::Stat`, and the run stops before any later operand.
- An unreadable source is `CpError::Read`; an uncreatable destination is
  `CpError::Create`; a failed write is `CpError::Write`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

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
