# `tairix-getcap` — report a file's capability gate

Stage 6 deliverable (`AGENTS.md` §3 `userland/apps/`). `getcap` reports
the **optional capability requirement** an inode may carry: a capability
the caller must hold to reach the node at all, on top of the mode/ACL
checks (`AGENTS.md` §5.3). For each file operand it prints one line —
`path CAP_NAME` — when the file carries a gate, and prints nothing for a
file that has none, so a clean tree is silent. With `-R` a directory
operand is reported and then its contents recursively. It is the
read-only companion of `tairix-setcap`.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `tairix-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

## Usage

```
getcap [-R] [--] file...

  -R, --recursive  report files and directories recursively
  -h, --help       show the usage banner
```

At least one file is required. `--` ends option parsing: every later
argument is an operand. `getcap` spells recursive `-R`; a bare `-r` is
not an option.

## Capability names

A gate renders by its canonical `CAP_*` name (e.g. `CAP_AUDIT_READ`),
resolved through `tairix_abi::CapabilityId::name` — the single, frozen
`abi-v1` source of truth shared with `setcap` (`AGENTS.md` §2.2, §5.2). A
node that stored an in-range identifier the running ABI has not yet named
renders as `CAP_<id>` rather than being silently dropped, so a gate is
never hidden (`AGENTS.md` §2.1).

## A reporter, not a policy point

`run` asks the injected filesystem seam for each operand's kind and
capability gate, renders the gated files, and walks each directory `-R`
must descend (reporting the directory before its contents). The driver
only *reports* the stored gate; `getcap` makes no permission decision
(`AGENTS.md` §5.4 — the VFS is the policy point). The operations that
reach the outside world are injected seams, mirroring the other userland
crates (`cat`'s `FileSource`, `ls`'s `Listing`, `rm`'s `Removal`, `cp`'s
and `mv`'s `FileSystem`, `chmod`'s and `chown`'s `FileSystem`):

- `FileSystem` — learn a path's kind, read its capability gate, and read
  a directory's entries (for `-R`). A child's kind is carried in its
  directory entry, so the recursion never re-inspects it.
- `Output` — write the report and the usage banner to the terminal.

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing, rendering, and recursion
decision is testable without a kernel.

## Fail closed

An unknown option or a missing operand is a `GetcapError::Usage` that
reports nothing. An operand that cannot be inspected surfaces the
underlying `Errno` as `GetcapError::Stat`; a gate that cannot be read is
`GetcapError::Query`; a directory whose entries cannot be read during a
recursive descent is `GetcapError::Read`; a failed write is
`GetcapError::Output`. The first failure stops the run before any later
operand, and there is no panic (`AGENTS.md` §2.9).

## Tests

`cargo test -p tairix-getcap` drives the parser and the engine against an
in-memory tree and a recording output: the command grammar, a gated file
reported by name, an ungated file producing no output, an unnamed
in-range gate rendered numerically, several files reporting only the
gated ones in order, the non-recursive and recursive descent orders, and
the missing-operand / stat / query / read-during-recursion fail-closed
paths.

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
