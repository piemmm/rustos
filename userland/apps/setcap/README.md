# `tairix-setcap` — set or clear a file's capability gate

Stage 6 deliverable (`AGENTS.md` §3 `userland/apps/`). `setcap` changes
the **optional capability requirement** of each of its file operands: a
capability the caller must hold to reach the node at all, on top of the
mode/ACL checks (`AGENTS.md` §5.3). The capability operand is either a
canonical `CAP_*` name (e.g. `CAP_AUDIT_READ`), which installs that gate,
or the literal `-`, which clears the gate so the node has none. With `-R`
a directory operand is changed and then its contents recursively. It is
the policy-*writing* companion of `tairix-getcap`.

`setcap` stores the gate but makes no permission decision itself
(`AGENTS.md` §5.4 — the VFS is the policy point). Setting a gate is itself
a privileged operation; the filesystem seam refuses an attempt the caller
is not authorised to make (it surfaces as `SetcapError::Apply`).

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `tairix-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

## Usage

```
setcap [-R] [--] CAP file...

  -R, --recursive  change files and directories recursively
  -h, --help       show the usage banner
```

A capability spec and at least one file are required. `--` ends option
parsing: every later argument is an operand. `setcap` spells recursive
`-R`; a bare `-r` is not an option.

## The capability grammar

The capability spec is one of:

- a canonical `CAP_*` name (`CAP_FS_MOUNT`, `CAP_AUDIT_READ`, …) — install
  that gate; the name is resolved through
  `tairix_abi::CapabilityId::from_name`, the same frozen `abi-v1` table
  `getcap` renders with (`AGENTS.md` §2.2);
- the literal `-` — clear the gate.

The name match is exact and case-sensitive (`AGENTS.md` §2.1 — no
guessing): an unknown, mis-cased, or bare-numeric value is rejected as a
`SetcapError::BadCapability` rather than coerced.

## A gate-setting machine, not a data source

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

## Fail closed

An unknown option or a missing operand is a `SetcapError::Usage` that
changes nothing; a capability operand that is neither a known `CAP_*` name
nor `-` is a `SetcapError::BadCapability`. An operand that cannot be
inspected surfaces the underlying `Errno` as `SetcapError::Stat`; a gate
that cannot be applied is `SetcapError::Apply`; a directory whose entries
cannot be read during a recursive descent is `SetcapError::Read`. The
first failure stops the run before any later operand, and there is no
panic (`AGENTS.md` §2.9).

## Tests

`cargo test -p tairix-setcap` drives the parser and the engine against an
in-memory tree and a recording output: the command grammar, the cap-spec
parser (the named and `-` forms, and the unknown / mis-cased / numeric
refusals), a named-capability install, a `-` clear, several files, the
non-recursive and recursive descent orders, and the missing-operand /
stat / apply / read-during-recursion fail-closed paths.

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
