# `rustos-groupadd` — create a group

A `plans/APPS.md` command app registered at
`/System/Apps/groupadd.app/Run` so the shell resolves the bare word
`groupadd` to it. `groupadd` adds a single group to the group registry
that persists under `/System/Security/Groups` (`AGENTS.md` §5.1, §16).
It names the new group and an optional numeric id (auto-allocated when
omitted) and hands that record to the registry through an injected seam.
The group id is a **decimal** value, the same choice `chown` and
`useradd` make. `-h`/`-?` render the tool's own short help from its
bundled `Help/` tree through the shared `lib/help` engine
(`plans/APPS.md` §4), in the locale the inherited `LANG` variable names,
falling back to the usage banner when the tree is unavailable.

It is the natural sibling of `useradd`: the same parser/seam/error
discipline, narrowed to the two fields a group record carries.

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
groupadd [-g GID] [--] NAME

  -g, --gid GID   numeric group id (auto-allocated if omitted)
  -h, -?, --help  show this command's own short help
```

Exactly one name operand is required. `-g` accepts its value attached
(`-g0`, `--gid=0`) or as the following argument (`-g 0`). `--` ends
option parsing: every later argument is an operand.

The bundle's thirteen-locale `Help/` tree (the canonical `en-US` plus the
`rustos_help::REQUIRED_LOCALES` translations, `plans/APPS.md` §8.1) is
authored on disk in this crate and
planted at `/System/Apps/groupadd.app/Help/` by the image builder from
that source (`tools/syshelp`) — never embedded in the binary
(`plans/APPS.md` §6.1).

## Production wiring, host-tested

`run` asks the injected registry whether the name is taken, then writes
the new record. The production `GroupDb` is `db::GroupsAdminDb`, the
`users_admin` client over its injected `db::AdminChannel` transport (the
syscall in production, an in-memory registry in tests), so the whole
client policy is host-tested:

- an omitted gid is allocated by the shared `rustos_users::next_id`
  policy (interactive-user range, `1000..`: one above the highest taken
  id in the band, fail closed on band exhaustion);
- the registry — not this tool — is the policy point: it enforces
  `CAP_USER_ADMIN` and gid uniqueness (`AGENTS.md` §5.4).

## Fail closed

An unknown option or anything other than exactly one name operand is a
`GroupaddError::Usage` that creates nothing; a group name outside
`[a-z_][a-z0-9_-]*` is a `GroupaddError::BadName`; a `-g` value that is
not a decimal id is a `GroupaddError::BadId`; a name already present is a
`GroupaddError::Exists`. A registry that cannot be consulted surfaces the
underlying `Errno` as `GroupaddError::Lookup`, and a refused or failed
creation as `GroupaddError::Create`. There is no panic (`AGENTS.md`
§2.9).

Exit codes: `0` on success, `1` on a registry or output failure, `2` on
a usage error.

## Tests

`cargo test -p rustos-groupadd` drives the parser, the engine, and the
production client against in-memory fixtures: the command grammar (the
name-only and name+gid forms, long `--gid value`/`--gid=value` and
attached short `-g0` spellings, `-h`/`-?`/`--help`, the
wrong-operand-count, unknown-option, and missing-value usage refusals,
`--`, and the bad-id / bad-name refusals), the group-name validator, the
creation engine (a minimal group, a requested gid reaching the registry,
the already-exists refusal, and the lookup / create / taken-gid /
help-write fail-closed paths), the short-help render from a Help document
with its usage-banner fallback, the `users_admin` client (gid allocation
and pass-through, hostile and overlong replies failing closed), and the
switch-drift pin that every locale's `OPTIONS` section documents exactly
the parser's switches (`plans/APPS.md` §3.1).

See [`docs/src/userland/utilities.md`](../../../docs/src/userland/utilities.md)
for the full subsystem documentation.
