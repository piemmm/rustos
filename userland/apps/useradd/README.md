# `rustos-useradd` — create a user account

A `plans/APPS.md` command app registered at `/System/Apps/useradd.app/Run`
so the shell resolves the bare word `useradd` to it. `useradd` adds a
single account to the user database that persists under
`/System/Security/Users` (`AGENTS.md` §5.1, §16). It names the new
account and its numeric identity — a login name, an optional user id
(auto-allocated when omitted), a **required** primary group id, an
optional supplementary-group set, and the textual comment and home
directory — and hands that record to the database through an injected
seam. Group and user references are **decimal** ids, the same choice
`chown` makes. `-h`/`-?` render the tool's own short help from its
bundled `Help/` tree through the shared `lib/help` engine
(`plans/APPS.md` §4), in the locale the inherited `LANG` variable names,
falling back to the usage banner when the tree is unavailable.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). Its
dependencies are the audited `rustos-abi` vocabulary, the shared
`rustos-help` engine, and the `rustos-users` account policy, so it never
links a kernel or driver crate (`AGENTS.md` §17.4). Its manifest
(`AppInfo.toml`) requests `CAP_CONSOLE_WRITE`, `CAP_USER_ADMIN`, and
`CAP_FS_ACCESS` — the administrative gate sits above the session
baseline, so the tool works only for an administrator account and is
inert for everyone else.

## Usage

```
useradd [-u UID] -g GID [-G GID[,GID...]] [-c COMMENT] [-d HOME] [--] NAME

  -u, --uid UID       numeric user id (auto-allocated if omitted)
  -g, --gid GID       numeric primary group id (required)
  -G, --groups LIST   comma-separated numeric supplementary group ids
  -c, --comment TEXT  account comment / full name
  -d, --home PATH     home directory
  -h, -?, --help      show this command's own short help
```

Exactly one name operand is required, and `-g` is mandatory. Each
value-taking option accepts its value attached (`-u0`, `--uid=0`) or as
the following argument (`-u 0`, `--uid 0`). `--` ends option parsing:
every later argument is an operand.

The bundle's six-locale `Help/` tree (the canonical `en-US` plus `fr-FR`,
`de-DE`, `es-ES`, `uk-UA`, `it-IT`) is authored on disk in this crate and
planted at `/System/Apps/useradd.app/Help/` by the image builder from
that source (`tools/syshelp`) — never embedded in the binary
(`plans/APPS.md` §6.1).

## The account grammar

`UID`, `GID`, and the `-G` list entries are decimal ids. A name (rather
than a numeric id) is not accepted for a group: RustOS has no name-to-id
seam in this tool, so resolving names would be interface creep
(`AGENTS.md` §2.4). The login name must match `[a-z_][a-z0-9_-]*` and fit
within the length bound — the portable Unix shape, which admits no name
that could be confused for a numeric id or an option.

`-g` is required rather than defaulted: there is no default-group policy
to invent (`AGENTS.md` §2.1).

## The created account has no usable password

GNU `useradd` creates an account that cannot authenticate until an
administrator sets a password. The RustOS database requires a well-formed
password record on creation, so the production client submits one derived
from a throwaway 256-bit random secret it immediately discards: no
password matches it, the honest equivalent of the `!` field. The
administrator then sets a real password with the `users` tool's `passwd`
command. The record is built at the minimum PBKDF2 cost — the iteration
count defends guessable passwords, and a discarded random secret is
unguessable at any cost.

## Production wiring, host-tested

`run` asks the injected database whether the name is taken, then writes
the new record. The production `UserDb` is `db::UsersAdminDb`, the
`users_admin` client, itself seam-injected (`db::AdminChannel` — the
syscall; `db::Entropy` — the kernel CSPRNG through `sys:random`), so the
whole client policy is host-tested against an in-memory endpoint:

- an omitted uid is allocated by the shared `rustos_users::next_id`
  policy (one above the highest existing id, fail closed on exhaustion);
- an omitted home is the shared `rustos_users::default_home` layout
  (`/Users/NAME`), the shell is `rustos_users::DEFAULT_SHELL`, and the
  created ceiling is `rustos_users::SESSION_BASELINE` — an administrator
  widens it afterwards with the `users` tool's `grant`, bounded by their
  own effective set (the kernel enforces never-widen);
- the database — not this tool — is the policy point: it enforces
  `CAP_USER_ADMIN`, uid uniqueness, group existence, and the
  supplementary-group bound (`AGENTS.md` §5.4).

## Fail closed

An unknown option, a missing `-g`, or anything other than exactly one
name operand is a `UseraddError::Usage` that creates nothing; a login
name outside `[a-z_][a-z0-9_-]*` is a `UseraddError::BadName`; a
`-u`/`-g`/`-G` value that is not a decimal id is a `UseraddError::BadId`;
a name already present is a `UseraddError::Exists`. A database that cannot
be consulted surfaces the underlying `Errno` as `UseraddError::Lookup`,
and a refused or failed creation as `UseraddError::Create`. A refused
entropy draw creates nothing. There is no panic (`AGENTS.md` §2.9).

Exit codes: `0` on success, `1` on a database, entropy, or output
failure, `2` on a usage error.

## Tests

`cargo test -p rustos-useradd` drives the parser, the engine, and the
production client against in-memory fixtures: the command grammar (the
minimal name+group form, every option, long `--opt value`/`--opt=value`
and attached short `-u0` spellings, `-h`/`-?`/`--help`, the
missing-group, wrong-operand-count, unknown-option, and missing-value
usage refusals, `--`, and the bad-id / bad-name refusals), the login-name
validator, the creation engine (a minimal account, every field reaching
the database, the already-exists refusal, and the lookup / create /
unknown-group / help-write fail-closed paths), the short-help render from
a Help document with its usage-banner fallback, the `users_admin` client
(uid allocation and pass-through, the shared defaults, the unusable
password record verifying against no candidate, hostile and overlong
replies failing closed, a refused entropy draw creating nothing), and the
switch-drift pin that every locale's `OPTIONS` section documents exactly
the parser's switches (`plans/APPS.md` §3.1).

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
