# `tairix-login` — TAIRiX text login

Stage 6 deliverable (`AGENTS.md` §3 `userland/session/`, §10). It
authenticates a user against `kernel/sec` and launches a session on their
behalf. Which session runs is **system policy, never a per-login
prompt**: the authenticated account's text shell by default, or the
graphical desktop when the administrator configured
`os.loginType graphical` (`lib/sysconfig`) *and* a graphical session is
available. When it is not, the configured graphical default degrades to
text — never crashed, never errored (`AGENTS.md` §10) — and a shell user
starts the desktop on demand with the `desktop` command. The installed
binary lives at `/System/Services/login.app/Run`.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). It
depends only on the audited `lib/*` crates `tairix-abi`, `tairix-caps`,
and `tairix-log`, so it never links a kernel or driver crate
(`AGENTS.md` §17.4).

## A policy machine, not a credential store

`Login` decides *what* to prompt for, *when* to retry, and *which*
session to start. It never reads the credential store, hashes a password,
or speaks to a terminal device. The operations that reach the
outside world are injected seams, mirroring `init`'s `Spawner`/`Reaper`
design:

- `LoginView` — presents the full-screen login page (machine name, OS
  version, and clock in the top bar; the bordered login box; the red
  running failed-attempt count; memory/tasks/users/load in the bottom
  bar) and reads the username (echoed in the box) and password (never
  rendered).
- `Authenticator` — verifies a `Credentials` pair against `kernel/sec`
  and the credential store, returning the user's identity and capability
  ceiling on success. Failures come back as the stable `Errno`, so login
  invents no parallel error vocabulary (`AGENTS.md` §2.2).
- `SessionLauncher` — starts the chosen `SessionKind` under the user's
  identity and blocks until it ends.
- `ElevateLauncher` (`elevate.rs`) — runs one re-authenticated
  `elevate <user> <program>` command as the target account and returns its
  exit code (`plans/CAPABILITY_USE.md` CU5). The decision logic
  (`handle_elevate_request`) placement-checks the caller's attested
  console, decodes fail-closed, re-authenticates through the same
  `Authenticator` as the prompt, and audits every grant and refusal; the
  `Run` binary serves it over the console's reserved elevation endpoint
  while the session runs.

On a running kernel these are syscall- and `kernel/sec`-backed; in tests
they are in-memory fixtures, so every control-flow decision is testable
without a kernel.

## Fail closed

`Login::run` bounds the number of attempts: a rejected credential is
audited and consumes one try, and an exhausted budget launches nothing
(`LoginError::TooManyAttempts`). A terminal that cannot be read aborts
(`LoginError::Console`), and an authenticated user whose session will not
start returns `LoginError::SessionLaunch` (`AGENTS.md` §5.4.5). The
`Authenticator` returns the same error whether the account is unknown or
the password is wrong, and login never discloses the cause — the prompt
cannot be used to probe for valid usernames (`AGENTS.md` §5).

## Capability handoff

A successful authentication yields an `AuthenticatedUser` carrying the
`(uid, primary gid, supplementary gids, capability grants)` tuple
(`AGENTS.md` §5.1). The capability set is the user's grant ceiling; login
hands it verbatim to the `SessionLauncher` and never widens it. The loader
intersects it with the launched binary's signed manifest request at exec
time (`AGENTS.md` §5.2). There is no ambient authority (`AGENTS.md` §4).

## Audit events

Login owns the reserved `EventId` range `10000..11000` (`AGENTS.md` §2.5,
§19.4): `SESSION_STARTED`, `AUTH_FAILED`, `LOCKED_OUT`, `SESSION_ENDED`,
`SESSION_LAUNCH_FAILED`, `CONSOLE_ERROR`, `ELEVATE_GRANTED`,
`ELEVATE_REFUSED`, `ELEVATE_UNAVAILABLE`.

## Tests

`cargo test -p tairix-login` drives the state machine against in-memory
`Prompt`/`Authenticator`/`SessionLauncher` fixtures and a recording log
sink, covering a successful text login, the configured graphical default
starting the desktop (and degrading to text when no graphical session is
available), wrong-password retry then success, the lockout and
zero-budget paths, a dead console, and a refused session launch — plus
the `EventId` invariants, the numeric audit-field formatter, and
the elevation broker's decision table (grant + audit, foreign console
refused before parsing, malformed refused without authentication,
indistinguishable refusals, spawn refusal reported verbatim).

See [`docs/src/userland/login.md`](../../../docs/src/userland/login.md)
for the full subsystem documentation.
