# `rustos-useradd` — create a user account

Stage 6 deliverable (`AGENTS.md` §3 `userland/apps/`). `useradd` adds a
single account to the user database that persists under
`/System/Security/Users` (`AGENTS.md` §5.1, §16). It names the new
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

## Usage

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
every later argument is an operand.

## The account grammar

`UID`, `GID`, and the `-G` list entries are decimal ids. A name (rather
than a numeric id) is not accepted for a group: RustOS has no name-to-id
seam in this tool, so resolving names would be interface creep
(`AGENTS.md` §2.4). The login name must match `[a-z_][a-z0-9_-]*` and fit
within the length bound — the portable Unix shape, which admits no name
that could be confused for a numeric id or an option.

`-g` is required rather than defaulted: there is no default-group policy
to invent (`AGENTS.md` §2.1). Likewise a missing `-u` and a missing `-d`
are left to the database's documented defaults (the §16 `/Users/<name>`
home layout), not guessed here.

## An account-spec parser, not a policy point

`run` asks the injected database whether the name is already taken, then
writes the new record. Creating an account is privileged — it needs
`CAP_USER_ADMIN` (`AGENTS.md` §5.2) — but the **database** makes that
decision, not this tool (`AGENTS.md` §5.4). The operations that reach the
outside world are injected seams, mirroring the other userland crates
(`init`'s `Spawner`/`Reaper`, `login`'s `Authenticator`, `setcap`'s
`FileSystem`):

- `UserDb` — learn whether a login name is in use and create the account
  record. It is the authority on permission, uid collisions, group
  existence, and the supplementary-group bound.
- `Output` — write the usage banner to the terminal (`useradd` is silent
  on success).

On a running system these are syscall- and console-backed; in tests they
are in-memory fixtures, so every parsing and validation decision is
testable without a kernel.

## Fail closed

An unknown option, a missing `-g`, or anything other than exactly one
name operand is a `UseraddError::Usage` that creates nothing; a login
name outside `[a-z_][a-z0-9_-]*` is a `UseraddError::BadName`; a
`-u`/`-g`/`-G` value that is not a decimal id is a `UseraddError::BadId`;
a name already present is a `UseraddError::Exists`. A database that cannot
be consulted surfaces the underlying `Errno` as `UseraddError::Lookup`,
and a refused or failed creation as `UseraddError::Create`. There is no
panic (`AGENTS.md` §2.9).

## Tests

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

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
