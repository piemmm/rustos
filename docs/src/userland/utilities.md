# Core CLI utilities (`userland/apps` and `userland/shell`)

Stage 6 ships a set of small command-line utilities, each its own crate.
This page documents the ones that have landed (`sysinfo`, `ps`, `cat`,
`ls`, `rm`, `cp`, `mv`, `chmod`, `chown`, `getcap`, and `setcap`) and is
extended as the others (`mount`, …) arrive.

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
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). It
depends only on the audited `rustos-abi` crate and the shared
`rustos-procinfo` client helpers, so it never links a kernel or driver
crate (`AGENTS.md` §17.4).

### Commands

| Command              | Query                 | Capability           |
|----------------------|-----------------------|----------------------|
| `processes`          | `SELF_PROCESS_LIST`   | none                 |
| `processes --all`    | `GLOBAL_PROCESS_LIST` | `CAP_SYSINFO_GLOBAL` |
| `memory`             | `KERNEL_MEMORY_STATS` | `CAP_SYSINFO_KERNEL` |
| `hardware`           | `HARDWARE_TREE`       | `CAP_SYSINFO_HW`     |
| `identity`           | `SYSTEM_IDENTITY`     | none                 |
| `uptime`             | `UPTIME`              | none                 |
| `limits`             | `RESOURCE_LIMITS`     | none (self-scoped)   |
| `help` (the default) | —                     | none                 |

`processes` accepts the `-a`/`--all` flag; the other subcommands take no
arguments and `ps`/`mem`/`hw`/`id`/`rlimits` are accepted as short
aliases. `limits` reports the calling process's *own* effective resource
limits and live usage (`AGENTS.md` §24.3) — the read-only counterpart of
the `ulimit` shell builtin that *changes* them. The
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

The `Transport`/`Output` seams, the request framing and capability-aware
call, and the process-list paging and row rendering are shared with `ps`
through the `lib/procinfo` crate. Sibling userland crates may not depend
on one another (`AGENTS.md` §17.4), so the common piece lives in `lib/*`
rather than being copied (`AGENTS.md` §2.2); `sysinfo` adds only the
scalar queries (`memory`/`hardware`/`identity`/`uptime`/`limits`) and its
own command grammar on top.

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

## `ps` — list processes (`userland/apps/ps`)

`rustos-ps` is the POSIX-named process lister. Like `sysinfo`, it is a
*client* of the System Information API (`AGENTS.md` §16.6): there is no
`/proc`, so `ps` issues the `sysinfo-v1` process-list queries served by
`/System/Services/sysinfod` and has no privileged path that bypasses the
capability check. By default it lists the caller's own processes (the
ungated `SELF_PROCESS_LIST`); `-e`/`-A`/`--all` request every process
(`GLOBAL_PROCESS_LIST`, which the service gates on `CAP_SYSINFO_GLOBAL`).

The crate is `no_std` (with `alloc`, used only by the test fixtures), has
no `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths
(`AGENTS.md` §2.9). It depends only on the audited `rustos-abi` crate and
the shared `rustos-procinfo` client helpers, so it never links a kernel
or driver crate (`AGENTS.md` §17.4).

### Grammar

```
ps [-e | -A | --all]

  (default)   list your own processes
  -e, -A      list every process (needs CAP_SYSINFO_GLOBAL)
  -h, --help  show the usage banner
```

`ps` takes no file operands. `--` ends option parsing. An unknown option,
an unknown letter inside a cluster, or any positional operand is a
fail-closed `PsError::Usage`.

### Shared with `sysinfo`

`ps` and `sysinfo` read the same process list, so the request seams
(`Transport`/`Output`), the request framing and capability-aware `call`,
the `offset`/`limit` page walk, and the fixed-column row rendering
(`PID PPID UID GID S CPU NAME`, with a single-letter state code) live
once in the `lib/procinfo` crate rather than being copied (`AGENTS.md`
§2.2). Because sibling userland crates may not depend on one another
(`AGENTS.md` §17.4), that shared piece is a `lib/*` crate. `ps` supplies
only its own argument grammar, usage banner, and `PsError`.

### A renderer, not a policy point

`run` pages through the process list via `lib/procinfo` and writes one
rendered row per process to the injected `Output`. The capability gate
lives in `sysinfod`, not here: a denied global listing comes back as
`Errno::PermissionDenied`, which `ps` renders honestly as
`PsError::PermissionDenied` (`AGENTS.md` §5.4 — the service is the policy
point). The two operations that reach the outside world — issuing the
request and writing the terminal — are the injected `Transport` and
`Output` seams; on a running system they are IPC- and console-backed, and
in tests they are in-memory fixtures.

### Fail closed

- An unknown option or a positional operand is a `PsError::Usage` that
  issues no query and prints the usage banner.
- A denied global listing is `PsError::PermissionDenied`; any other
  transport failure or a reply that does not decode against `sysinfo-v1`
  (a process page whose length is not a whole number of records) is a
  hard `PsError::Service`, never a partially-rendered guess.
- A failed terminal write is `PsError::Output`. There is no panic
  (`AGENTS.md` §2.9).

### Tests

`cargo test -p rustos-ps` drives the parser and the request/render engine
against an in-memory `sysinfod` stand-in and a recording output: the
command grammar (default self-listing, the `-e`/`-A`/`--all` selectors,
`-h`/`--help`, unknown-option and positional-operand rejection), the
self-vs-global query routing, header + rows rendering, the empty listing,
the denied-global capability mapping, and the header/row write-failure
paths. The shared page walk and rendering carry their own unit tests in
`lib/procinfo` (`cargo test -p rustos-procinfo`).

## `mount` — list and attach filesystems (`userland/apps/mount`)

`rustos-mount` both reports and changes the mount table, and the two
halves take deliberately different paths. **Listing** the mounted
filesystems is a *read* of live system state, so — like `ps` — it goes
through the System Information API (`AGENTS.md` §16.6): there is no
`/proc` and no mount-table file, so `mount` issues the ungated
`sysinfo-v1` `MOUNT_LIST` query served by `/System/Services/sysinfod`.
**Attaching** a filesystem is privileged (it needs `CAP_FS_MOUNT`,
`AGENTS.md` §5.2), and the kernel — not this tool — makes that decision
(`AGENTS.md` §5.4).

The crate is `no_std` (with `alloc`, used only by the test fixtures), has
no `unsafe`, and no `unwrap`/`expect`/`panic!` in production paths
(`AGENTS.md` §2.9). It depends only on the audited `rustos-abi` crate and
the shared `rustos-procinfo` client helpers, so it never links a kernel
or driver crate (`AGENTS.md` §17.4).

### Grammar

```
mount [-r] [-t TYPE] [-o OPTIONS] [--] [SOURCE TARGET]

  (no operands)        list the mounted filesystems
  SOURCE TARGET        mount SOURCE at TARGET (needs CAP_FS_MOUNT)
  -r, --read-only      mount read-only (same as -o ro)
  -t, --types TYPE     filesystem type (probed when omitted)
  -o, --options LIST   comma-separated: ro,rw,nosuid,nodev,noexec
  -h, --help           show the usage banner
```

With no operands `mount` lists the table; with exactly `SOURCE TARGET` it
attaches. Value options accept their value attached (`-text4`,
`--types=ext4`) or as the following argument; `-r` may cluster with other
toggles. `--` ends option parsing. The recognised `-o` names map onto the
frozen `MountFlags` bitmap (`ro`/`rw` plus the `nosuid`/`nodev`/`noexec`
restrictions, `AGENTS.md` §5.3).

### Listing — a client of the mount-list query

A listing pages the `MOUNT_LIST` reply through the same `lib/procinfo`
machinery `ps` uses — the `Transport`/`Output` seams, the request framing,
and the generic `offset`/`limit` page walk — so none of it is copied
(`AGENTS.md` §2.2). The shared renderer prints one familiar
`source on target type fstype (options)` line per mount; the option list
opens with `ro`/`rw` and then names each restriction in force. The query
is ungated: the mount table is system-wide and secret-free, so any task
may read it (`AGENTS.md` §16.6).

### Attaching — a presenter, not a policy point

For a `SOURCE TARGET` request `mount` parses and validates the arguments
and hands a `MountSpec` to the injected `Mounter` seam; it makes no
permission decision of its own. The kernel is the policy point
(`AGENTS.md` §5.4): a caller lacking `CAP_FS_MOUNT`, an unknown source, a
bad superblock, or an already-mounted target is refused there and
surfaced as `MountError::Mount(errno)`. `mount` writes nothing on a
successful attach. A `None` filesystem type asks the kernel to identify
the volume by probing; `mount` never guesses one (`AGENTS.md` §2.1).

### Fail closed

- An unknown option, a missing option value, or a number of operands
  other than zero or two is a `MountError::Usage`; mount options given
  with no operands are also a usage error (there is nothing to mount).
- An unknown or empty `-o`/`-t` value is a `MountError::BadOption`.
- A listing transport failure or a reply that does not decode against
  `sysinfo-v1` is a hard `MountError::Service`, never a partially-rendered
  guess; a refused or failed attach is `MountError::Mount`; a failed
  terminal write is `MountError::Output`. There is no panic (`AGENTS.md`
  §2.9).

### Tests

`cargo test -p rustos-mount` drives the parser and the engine against an
in-memory `sysinfod` fixture, a recording output, and an in-memory
mounter: the command grammar (list vs mount vs help, every option form,
attached/space values, the read-only shorthand, `--`, and the
usage/bad-option rejections), the mount-table listing and its query
routing, the empty table, the service- and output-failure paths, the
attach request reaching the mounter with the right fields, and the denied
attach mapping to `MountError::Mount`. The shared page walk and the
`source on target type fstype (options)` rendering carry their own unit
tests in `lib/procinfo` (`cargo test -p rustos-procinfo`).

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

## `mv` — move (rename) files and directories (`userland/apps/mv`)

`rustos-mv` relocates each of its source operands to a destination
(`AGENTS.md` §3). With a single source and a destination that is not a
directory, the source is moved to that exact path. When the destination
is an existing directory — and always when there is more than one
source — each source is moved *into* it under its base name. Unlike
`cp`, a directory needs no flag: a directory is moved like any other
operand. This is the POSIX model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
mv [-f] [-n] [--] source... dest

  -f, --force        remove a blocking destination and retry the rename
  -n, --no-clobber   never overwrite an existing destination
  -h, --help         show the usage banner
```

At least one source and a destination are required. Short options may be
combined (e.g. `-fn`). `--` ends option parsing: every later argument is
a path. With more than one source the destination must be a directory.
`-h`/`--help` wins immediately.

### A move machine, not a data source

`run` asks the injected filesystem seam what each source is, then asks it
to `rename` the source onto its destination. A rename within one
filesystem is atomic and is the whole operation. The operations that
reach the outside world are injected seams, mirroring the other userland
crates (`cat`'s `FileSource`, `ls`'s `Listing`, `rm`'s `Removal`, `cp`'s
`FileSystem`):

- `FileSystem` — learn a path's kind, rename a path, read a file's bytes
  and a directory's entries, create directories/files/bytes, and remove
  files and directories (for the cross-device relocation and for `-f`).
- `Output` — write the usage banner to the terminal (`mv` is silent on
  success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every routing and fallback decision is
testable without a kernel.

### Cross-device relocation

A rename cannot be atomic when its source and destination live on
different filesystems. Rather than overload an `Errno`, the `rename` seam
reports that case as an explicit `RenameOutcome::CrossDevice` outcome
(`AGENTS.md` §2.11). The engine then performs the POSIX relocation: it
copies the source to the destination — streaming a regular file in
fixed-size chunks (matching `cat`'s and `cp`'s granularity) and
reproducing a directory subtree depth-first — and only then removes the
source, depth-first, so a directory is unlinked after its contents. A
failure during the copy leaves the source in place.

### No-clobber and force

`-n` never overwrites: a source whose destination already exists is
skipped silently. `-f` covers a destination that blocks the rename (for
example, an existing read-only file): the destination is removed and the
rename is retried exactly once. Without either flag an existing
destination is overwritten, the default POSIX behaviour.

### Fail closed

- An unknown option, fewer than two operands, or more than one source
  aimed at a non-directory destination is an `MvError::Usage` that moves
  nothing.
- An operand that cannot be inspected surfaces the underlying `Errno` as
  `MvError::Stat`, and the run stops before any later operand.
- A rename that fails for a reason other than crossing a filesystem
  boundary is `MvError::Rename`.
- During a cross-device relocation an unreadable source is
  `MvError::Read`, an uncreatable destination is `MvError::Create`, a
  failed write is `MvError::Write`, and a source that cannot be removed
  after a successful copy is `MvError::Remove`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p rustos-mv` drives the parser and the move engine against
an in-memory tree and a recording output: the command grammar (every
option, clustered short flags, `-`/`--`, the too-few-operands and
unknown-option paths), a file renamed to a new path, a directory renamed,
a file moved into a directory under its base name, several files into a
directory, the several-sources-to-a-non-directory `Usage` refusal, the
missing-source fail-closed path, a failure stopping before a later
source, `-n` skipping an existing destination, the default overwrite, the
failed-rename path, the blocking-destination refusal and its `-f`
remove-and-retry recovery, the cross-device file / large-file /
directory relocations, the cross-device read/write/remove fail-closed
paths, and the trailing-slash base-name join.

## `chmod` — change file mode bits (`userland/apps/chmod`)

`rustos-chmod` applies a mode to each of its file operands (`AGENTS.md`
§3). The mode is either an absolute octal value (`644`, `0755`, …) that
replaces the permission bits outright, or a comma-separated list of
symbolic clauses (`[ugoa]*[-+=][rwxXst]*`, e.g. `g+w`, `o-rx`, `a=rx`,
`u+s`) that transform the file's current bits. With `-R` a directory
operand is changed and then its contents are changed recursively. This is
the POSIX model, and it is a building block of the §5.3 filesystem
permission model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
chmod [-R] [--] MODE file...

  -R, --recursive  change files and directories recursively
  -h, --help       show the usage banner
```

A mode and at least one file are required. `--` ends option parsing:
every later argument is an operand. POSIX `chmod` spells recursive `-R`;
a bare `-r` is not an option. To set a mode that begins with `-`, write
it without the dash (`a-w`) or end option parsing first
(`chmod -- -w file`). `-h`/`--help` wins immediately.

### The mode grammar

- **Octal**: one to four octal digits set the low twelve permission bits
  (the `rwx` triples plus the setuid/setgid/sticky bits) outright; the
  current mode is irrelevant.
- **Symbolic**: comma-separated clauses, each `[ugoa]*[-+=][rwxXst]*`.
  `u`/`g`/`o` select the owner/group/other field and `a` (or an omitted
  who) selects all. `+` turns the bits on, `-` off, and `=` sets the
  selected fields to exactly those bits. Permissions are `r`, `w`, `x`,
  `X` (execute only for a directory or a file that already carries an
  execute bit), `s` (setuid/setgid), and `t` (sticky). A clause may chain
  several operator sections that share its who (e.g. `u+x-w`). An omitted
  who is treated as `a` (RustOS has no per-process umask seam to honour,
  so the `a` interpretation is exact, not umask-masked).

### A mode-changing machine, not a data source

`run` asks the injected filesystem seam for each operand's kind and
current mode, computes the new mode, applies it, and walks each directory
`-R` must descend (changing the directory before its contents). The
operations that reach the outside world are injected seams, mirroring the
other userland crates (`cat`'s `FileSource`, `ls`'s `Listing`, `rm`'s
`Removal`, `cp`'s and `mv`'s `FileSystem`):

- `FileSystem` — learn a path's kind and current mode, set its mode, and
  read a directory's entries (for `-R`).
- `Output` — write the usage banner to the terminal (`chmod` is silent on
  success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, mode-algebra, and recursion
decision is testable without a kernel.

### Fail closed

- An unknown option or a missing operand is a `ChmodError::Usage` that
  changes nothing.
- A mode operand that is neither octal nor symbolic is a
  `ChmodError::BadMode`.
- An operand that cannot be inspected surfaces the underlying `Errno` as
  `ChmodError::Stat`, and the run stops before any later operand.
- A mode that cannot be applied is `ChmodError::Apply`; a directory whose
  entries cannot be read during a recursive descent is `ChmodError::Read`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p rustos-chmod` drives the parser, the symbolic-mode
algebra, and the move engine against an in-memory tree and a recording
output: the command grammar (octal and symbolic modes, the recursive
flag, the `-r`-is-not-recursive and unknown-option refusals, `--`,
too-few-operands and bad-mode paths), the full mode algebra (`+`/`-`/`=`,
omitted-who, conditional `X`, setuid/setgid/sticky, left-to-right clause
application, empty-perm no-ops), an octal change, a symbolic change,
several files, a non-recursive directory change leaving its contents
alone, a recursive change touching the directory before its contents,
per-node `X` resolution under recursion, and the missing-operand / stat /
apply / read-during-recursion fail-closed paths.

## `chown` — change file owner and group (`userland/apps/chown`)

`rustos-chown` applies an ownership change to each of its file operands
(`AGENTS.md` §3). The owner operand is `OWNER`, `OWNER:GROUP`, or
`:GROUP`, where `OWNER` and `GROUP` are **decimal** user/group ids:
`OWNER` changes only the owning user, `:GROUP` only the owning group, and
`OWNER:GROUP` both. With `-R` a directory operand is changed and then its
contents are changed recursively. This is the POSIX model, restricted to
numeric ids, and it is a building block of the §5.3 filesystem permission
model.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
chown [-R] [--] OWNER[:GROUP] file...

  -R, --recursive  change files and directories recursively
  -h, --help       show the usage banner
```

An owner spec and at least one file are required. `--` ends option
parsing: every later argument is an operand. POSIX `chown` spells
recursive `-R`; a bare `-r` is not an option. `-h`/`--help` wins
immediately.

### The owner grammar

`OWNER` and `GROUP` are decimal ids, in one of three forms:

- `OWNER` — change only the owning user, leaving the group.
- `OWNER:GROUP` — change both.
- `:GROUP` — change only the owning group.

A name (rather than a numeric id) is not accepted: RustOS has no
name-to-id seam in this tool, so resolving names would be interface creep
(`AGENTS.md` §2.4). An empty spec, a bare `:`, and a trailing-colon
`OWNER:` (which on POSIX systems means "the user's login group", and has
no meaning without a name database) are all rejected rather than guessed
(`AGENTS.md` §2.1).

### An ownership-changing machine, not a data source

`run` asks the injected filesystem seam for each operand's kind, applies
the new owner, and walks each directory `-R` must descend (changing the
directory before its contents, and reusing the kind carried in each
directory entry so it re-inspects nothing). The operations that reach the
outside world are injected seams, mirroring the other userland crates
(`cat`'s `FileSource`, `ls`'s `Listing`, `rm`'s `Removal`, `cp`'s and
`mv`'s `FileSystem`, `chmod`'s `FileSystem`):

- `FileSystem` — learn a path's kind, set its owner, and read a
  directory's entries (for `-R`).
- `Output` — write the usage banner to the terminal (`chown` is silent on
  success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, owner-spec, and recursion
decision is testable without a kernel.

### Fail closed

- An unknown option or a missing operand is a `ChownError::Usage` that
  changes nothing.
- An owner operand that is not a valid `OWNER`/`OWNER:GROUP`/`:GROUP`
  spec is a `ChownError::BadOwner`.
- An operand that cannot be inspected surfaces the underlying `Errno` as
  `ChownError::Stat`, and the run stops before any later operand.
- An owner that cannot be applied is `ChownError::Apply`; a directory
  whose entries cannot be read during a recursive descent is
  `ChownError::Read`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

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

## `getcap` — report a file's capability gate (`userland/apps/getcap`)

`rustos-getcap` reports the **optional capability requirement** an inode
may carry: a capability the caller must hold to reach the node at all, on
top of the mode/ACL checks (`AGENTS.md` §5.3). For each file operand it
prints one line — `path CAP_NAME` — when the file carries a gate, and
prints nothing for a file that has none, so a clean tree is silent. With
`-R` a directory operand is reported and then its contents recursively.
It is the read-only companion of [`setcap`](#setcap--set-or-clear-a-files-capability-gate-userlandappssetcap).

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
getcap [-R] [--] file...

  -R, --recursive  report files and directories recursively
  -h, --help       show the usage banner
```

At least one file is required. `--` ends option parsing: every later
argument is an operand. `getcap` spells recursive `-R`; a bare `-r` is
not an option. `-h`/`--help` wins immediately.

### Capability names

A gate renders by its canonical `CAP_*` name (e.g. `CAP_AUDIT_READ`),
resolved through `rustos_abi::CapabilityId::name` — the single,
frozen `abi-v1` source of truth shared with `setcap` (`AGENTS.md` §2.2,
§5.2). A node that stored an in-range identifier the running ABI has not
yet named renders as `CAP_<id>` rather than being silently dropped, so a
gate is never hidden (`AGENTS.md` §2.1).

### A reporter, not a policy point

`run` asks the injected filesystem seam for each operand's kind and
capability gate, renders the gated files, and walks each directory `-R`
must descend (reporting the directory before its contents). The driver
only *reports* the stored gate; `getcap` makes no permission decision
(`AGENTS.md` §5.4 — the VFS is the policy point). The operations that
reach the outside world are injected seams, mirroring the other userland
crates:

- `FileSystem` — learn a path's kind, read its capability gate, and read
  a directory's entries (for `-R`).
- `Output` — write the report and the usage banner to the terminal.

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, rendering, and recursion
decision is testable without a kernel.

### Fail closed

- An unknown option or a missing operand is a `GetcapError::Usage` that
  reports nothing.
- An operand that cannot be inspected surfaces the underlying `Errno` as
  `GetcapError::Stat`, and the run stops before any later operand.
- A gate that cannot be read is `GetcapError::Query`; a directory whose
  entries cannot be read during a recursive descent is `GetcapError::Read`;
  a failed write is `GetcapError::Output`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p rustos-getcap` drives the parser and the engine against an
in-memory tree and a recording output: the command grammar (the recursive
flag, the `-r`-is-not-recursive and unknown-option refusals, `--`, and
the no-operand path), a gated file reported by name, an ungated file
producing no output, an unnamed in-range gate rendered numerically,
several files reporting only the gated ones in order, a non-recursive
directory report leaving its contents alone, a recursive report touching
the directory before its contents, and the missing-operand / stat / query
/ read-during-recursion fail-closed paths.

## `setcap` — set or clear a file's capability gate (`userland/apps/setcap`)

`rustos-setcap` changes the **optional capability requirement** of each
of its file operands (`AGENTS.md` §5.3). The capability operand is either
a canonical `CAP_*` name (e.g. `CAP_AUDIT_READ`), which installs that gate,
or the literal `-`, which clears the gate so the node has none. With `-R`
a directory operand is changed and then its contents recursively. It is
the policy-*writing* companion of
[`getcap`](#getcap--report-a-files-capability-gate-userlandappsgetcap) and
a building block of the §5.3 filesystem permission model.

`setcap` stores the gate but makes no permission decision itself
(`AGENTS.md` §5.4 — the VFS is the policy point). Setting a gate is itself
a privileged operation; the filesystem seam refuses an attempt the caller
is not authorised to make (it surfaces as `SetcapError::Apply`).

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
setcap [-R] [--] CAP file...

  -R, --recursive  change files and directories recursively
  -h, --help       show the usage banner
```

A capability spec and at least one file are required. `--` ends option
parsing: every later argument is an operand. `setcap` spells recursive
`-R`; a bare `-r` is not an option. `-h`/`--help` wins immediately.

### The capability grammar

The capability spec is one of:

- a canonical `CAP_*` name (`CAP_FS_MOUNT`, `CAP_AUDIT_READ`, …) — install
  that gate; the name is resolved through
  `rustos_abi::CapabilityId::from_name`, the same frozen `abi-v1` table
  `getcap` renders with (`AGENTS.md` §2.2);
- the literal `-` — clear the gate.

The name match is exact and case-sensitive (`AGENTS.md` §2.1 — no
guessing): an unknown, mis-cased, or bare-numeric value is rejected as a
`SetcapError::BadCapability` rather than coerced.

### A gate-setting machine, not a data source

`run` asks the injected filesystem seam for each operand's kind, applies
the new gate, and walks each directory `-R` must descend (changing the
directory before its contents, and reusing the kind carried in each
directory entry so it re-inspects nothing). The operations that reach the
outside world are injected seams, mirroring `chmod`'s and `chown`'s
`FileSystem`:

- `FileSystem` — learn a path's kind, set its capability gate, and read a
  directory's entries (for `-R`).
- `Output` — write the usage banner to the terminal (`setcap` is silent
  on success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, cap-spec, and recursion
decision is testable without a kernel.

### Fail closed

- An unknown option or a missing operand is a `SetcapError::Usage` that
  changes nothing.
- A capability operand that is neither a known `CAP_*` name nor `-` is a
  `SetcapError::BadCapability`.
- An operand that cannot be inspected surfaces the underlying `Errno` as
  `SetcapError::Stat`, and the run stops before any later operand.
- A gate that cannot be applied is `SetcapError::Apply`; a directory whose
  entries cannot be read during a recursive descent is `SetcapError::Read`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p rustos-setcap` drives the parser and the engine against an
in-memory tree and a recording output: the command grammar (a named
capability and the clearing `-`, the recursive flag, the
`-r`-is-not-recursive and unknown-option refusals, `--`, the
too-few-operands and bad-capability paths), the cap-spec parser (the
named and `-` forms, and the unknown / mis-cased / numeric refusals), a
named-capability install, a `-` clear, several files, a non-recursive
directory change leaving its contents alone, a recursive change touching
the directory before its contents, and the missing-operand / stat / apply
/ read-during-recursion fail-closed paths.

## `useradd` — create a user account (`userland/apps/useradd`)

`rustos-useradd` adds a single account to the user database that persists
under `/System/Security/Users` (`AGENTS.md` §5.1, §16). It names the new
account and its numeric identity — a login name, an optional user id
(auto-allocated by the database when omitted), a **required** primary
group id, an optional supplementary-group set, and the textual comment
and home directory — and hands that record to the database through an
injected seam. Group and user references are **decimal** ids, the same
choice `chown` makes.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
useradd [-u UID] -g GID [-G GID[,GID...]] [-c COMMENT] [-d HOME] [--] NAME

  -u, --uid UID       numeric user id (auto-allocated if omitted)
  -g, --gid GID       numeric primary group id (required)
  -G, --groups LIST   comma-separated numeric supplementary group ids
  -c, --comment TEXT  account comment / full name
  -d, --home PATH     home directory
  -h, --help          show the usage banner
```

Exactly one name operand is required, and `-g` is mandatory. Each
value-taking option accepts its value attached (`-u0`, `--uid=0`) or as
the following argument (`-u 0`, `--uid 0`). `--` ends option parsing:
every later argument is an operand. `-h`/`--help` wins immediately.

### The account grammar

`UID`, `GID`, and the `-G` list entries are decimal ids. A group name
(rather than a numeric id) is not accepted: RustOS has no name-to-id seam
in this tool, so resolving names would be interface creep (`AGENTS.md`
§2.4). The login name must match `[a-z_][a-z0-9_-]*` within the length
bound — the portable Unix shape, which admits no name that could be
confused for a numeric id or an option.

`-g` is required rather than defaulted: there is no default-group policy
to invent (`AGENTS.md` §2.1). Likewise a missing `-u` and a missing `-d`
are left to the database's documented defaults (the §16 `/Users/<name>`
home layout), not guessed here.

### An account-spec parser, not a policy point

`run` asks the injected database whether the name is already taken, then
writes the new record. Creating an account is privileged — it needs
`CAP_USER_ADMIN` (`AGENTS.md` §5.2) — but the **database** makes that
decision, not this tool (`AGENTS.md` §5.4): an unauthorised attempt is
refused by the seam and surfaced as `UseraddError::Create`. The database
is likewise the authority on uid collisions, group existence, and the
supplementary-group bound. The operations that reach the outside world
are injected seams, mirroring `setcap`'s `FileSystem`, `login`'s
`Authenticator`, and `init`'s `Spawner`/`Reaper`:

- `UserDb` — learn whether a login name is in use and create the account
  record.
- `Output` — write the usage banner to the terminal (`useradd` is silent
  on success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing and validation decision is
testable without a kernel.

### Fail closed

- An unknown option, a missing `-g`, or anything other than exactly one
  name operand is a `UseraddError::Usage` that creates nothing.
- A login name outside `[a-z_][a-z0-9_-]*` is a `UseraddError::BadName`; a
  `-u`/`-g`/`-G` value that is not a decimal id is a `UseraddError::BadId`.
- A name already present is a `UseraddError::Exists`; a database that
  cannot be consulted surfaces the underlying `Errno` as
  `UseraddError::Lookup`, and a refused or failed creation as
  `UseraddError::Create`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p rustos-useradd` drives the parser and the engine against an
in-memory database and a recording output: the command grammar (the
minimal name+group form, every option, long `--opt value`/`--opt=value`
and attached short `-u0` spellings, `-h`/`--help`, the missing-group,
wrong-operand-count, unknown-option, and missing-value usage refusals,
`--`, and the bad-id / bad-name refusals), the login-name validator
(accepted and rejected shapes, including the length bound), and the
creation engine (a minimal account, every field reaching the database,
the already-exists refusal, and the lookup / create / unknown-group /
help-write fail-closed paths).

## `groupadd` — create a group (`userland/apps/groupadd`)

`rustos-groupadd` adds a single group to the group database that persists
under `/System/Security/Groups` (`AGENTS.md` §5.1, §16). It names the new
group and an optional numeric id (auto-allocated by the database when
omitted), and hands that record to the database through an injected seam.
The group id is a **decimal** value, the same choice `chown` and
`useradd` make. It is the natural sibling of `useradd`: the same
parser/seam/error discipline, narrowed to the two fields a group record
carries.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

### Grammar

```
groupadd [-g GID] [--] NAME

  -g, --gid GID   numeric group id (auto-allocated if omitted)
  -h, --help      show the usage banner
```

Exactly one name operand is required. `-g` accepts its value attached
(`-g0`, `--gid=0`) or as the following argument (`-g 0`). `--` ends
option parsing: every later argument is an operand. `-h`/`--help` wins
immediately.

### The group grammar

`GID` is a decimal id. A name (rather than a numeric id) is not accepted:
RustOS has no name-to-id seam in this tool, so resolving names would be
interface creep (`AGENTS.md` §2.4). The group name must match
`[a-z_][a-z0-9_-]*` within the length bound — the portable Unix shape,
which admits no name that could be confused for a numeric id or an
option.

A missing `-g` is left to the database to allocate rather than guessed
here (`AGENTS.md` §2.1).

### A group-spec parser, not a policy point

`run` asks the injected database whether the name is already taken, then
writes the new record. Creating a group is privileged — it needs
`CAP_USER_ADMIN` (`AGENTS.md` §5.2) — but the **database** makes that
decision, not this tool (`AGENTS.md` §5.4): an unauthorised attempt is
refused by the seam and surfaced as `GroupaddError::Create`. The database
is likewise the authority on gid collisions. The operations that reach
the outside world are injected seams, mirroring `useradd`'s `UserDb`,
`setcap`'s `FileSystem`, `login`'s `Authenticator`, and `init`'s
`Spawner`/`Reaper`:

- `GroupDb` — learn whether a group name is in use and create the group
  record.
- `Output` — write the usage banner to the terminal (`groupadd` is silent
  on success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing and validation decision is
testable without a kernel.

### Fail closed

- An unknown option or anything other than exactly one name operand is a
  `GroupaddError::Usage` that creates nothing.
- A group name outside `[a-z_][a-z0-9_-]*` is a `GroupaddError::BadName`;
  a `-g` value that is not a decimal id is a `GroupaddError::BadId`.
- A name already present is a `GroupaddError::Exists`; a database that
  cannot be consulted surfaces the underlying `Errno` as
  `GroupaddError::Lookup`, and a refused or failed creation as
  `GroupaddError::Create`.

There is no partial-guess path and no panic (`AGENTS.md` §2.9).

### Tests

`cargo test -p rustos-groupadd` drives the parser and the engine against
an in-memory database and a recording output: the command grammar (the
bare-name and name+gid forms, long `--gid value`/`--gid=value` and
attached short `-g0` spellings, `-h`/`--help`, the wrong-operand-count,
unknown-option, and missing-value usage refusals, `--`, and the bad-id /
bad-name refusals), the group-name validator (accepted and rejected
shapes, including the length bound), and the creation engine (a minimal
group, a requested gid reaching the database, the already-exists refusal,
and the lookup / create / taken-gid / help-write fail-closed paths).

## `users` — interactive account administration (`userland/shell/users`)

`rustos-users-cli` (`/System/Apps/users.app/Run`) is the first holder of the
`CAP_USER_ADMIN`-gated `users_admin` syscall
(`plans/CAPABILITY_USE.md` CU4): an interactive session that lists,
creates, modifies, locks/unlocks, and deletes accounts, edits their
capability ceilings, replaces passwords, and manages groups. It is
interactive (a `users>` prompt over the inherited standard streams);
now that the `spawn` ABI carries an argument vector, the staged
`useradd`/`groupadd` argv grammars above can become thin frontends over
the same syscall — the operation authority already lives in exactly one
place, the kernel engine.

Every rule is enforced kernel-side under the caller's attested identity:
the dispatch gate, never-widen grant editing, the last-administrator
guard, the `users-v1` format validation, crash-safe persistence, and the
next-spawn/next-login binding (`docs/src/security/capabilities.md`).
Passwords are read echo-off and hashed client-side into salted PBKDF2
records (salt from `sys:random`); the listing responses are secret-free.

The tool's manifest requests only the console pair plus
`CAP_USER_ADMIN` — deliberately above the session baseline, so the
`manifest ∩ ceiling` intersection arms it only for an administrator
account and leaves it inert for everyone else. No `CAP_FS_ACCESS`.

### Tests

`cargo test -p rustos-users-cli` drives scripted sessions through the
`ToolIo`/`AdminChannel`/`SaltSource` seams: the command grammar and its
usage refusals, the exact typed requests submitted (decoded and asserted
field by field), the password-record round trip and the
mismatched-password refusal, the grant merge/removal flow against a
served listing, the listing renderers, and the terse errno reporting.
