# `rustos-groupadd` — create a group

Stage 6 deliverable (`AGENTS.md` §3 `userland/apps/`). `groupadd` adds a
single group to the group database that persists under
`/System/Security/Groups` (`AGENTS.md` §5.1, §16). It names the new
group and an optional numeric id (auto-allocated by the database when
omitted), and hands that record to the database through an injected
seam. The group id is a **decimal** value, the same choice `chown` and
`useradd` make.

It is the natural sibling of `useradd`: the same parser/seam/error
discipline, narrowed to the two fields a group record carries.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
only dependency is the audited `rustos-abi` crate, so it never links a
kernel or driver crate (`AGENTS.md` §17.4).

## Usage

```
groupadd [-g GID] [--] NAME

  -g, --gid GID   numeric group id (auto-allocated if omitted)
  -h, --help      show the usage banner
```

Exactly one name operand is required. `-g` accepts its value attached
(`-g0`, `--gid=0`) or as the following argument (`-g 0`). `--` ends
option parsing: every later argument is an operand. `-h`/`--help` wins
immediately.

## The group grammar

`GID` is a decimal id. A name (rather than a numeric id) is not accepted:
RustOS has no name-to-id seam in this tool, so resolving names would be
interface creep (`AGENTS.md` §2.4). The group name must match
`[a-z_][a-z0-9_-]*` and fit within the length bound — the portable Unix
shape, which admits no name that could be confused for a numeric id or an
option.

A missing `-g` is left to the database to allocate rather than guessed
here (`AGENTS.md` §2.1).

## A group-spec parser, not a policy point

`run` asks the injected database whether the name is already taken, then
writes the new record. Creating a group is privileged — it needs
`CAP_USER_ADMIN` (`AGENTS.md` §5.2) — but the **database** makes that
decision, not this tool (`AGENTS.md` §5.4). The operations that reach the
outside world are injected seams, mirroring the other userland crates
(`useradd`'s `UserDb`, `setcap`'s `FileSystem`, `login`'s `Authenticator`,
`init`'s `Spawner`/`Reaper`):

- `GroupDb` — learn whether a group name is in use and create the group
  record. It is the authority on permission and gid collisions.
- `Output` — write the usage banner to the terminal (`groupadd` is silent
  on success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing and validation decision is
testable without a kernel.

## Fail closed

An unknown option or anything other than exactly one name operand is a
`GroupaddError::Usage` that creates nothing; a group name outside
`[a-z_][a-z0-9_-]*` is a `GroupaddError::BadName`; a `-g` value that is
not a decimal id is a `GroupaddError::BadId`; a name already present is a
`GroupaddError::Exists`. A database that cannot be consulted surfaces the
underlying `Errno` as `GroupaddError::Lookup`, and a refused or failed
creation as `GroupaddError::Create`. There is no panic (`AGENTS.md`
§2.9).

## Tests

`cargo test -p rustos-groupadd` drives the parser and the engine against
an in-memory database and a recording output: the command grammar (the
bare-name and name+gid forms, long `--gid value`/`--gid=value` and
attached short `-g0` spellings, `-h`/`--help`, the wrong-operand-count,
unknown-option, and missing-value usage refusals, `--`, and the bad-id /
bad-name refusals), the group-name validator (accepted and rejected
shapes, including the length bound), and the creation engine (a minimal
group, a requested gid reaching the database, the already-exists refusal,
and the lookup / create / taken-gid / help-write fail-closed paths).

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
