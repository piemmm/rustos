# `rustos-login` — RustOS text login

Stage 6 deliverable (`AGENTS.md` §3 `userland/session/`, §10). It
authenticates a user against `kernel/sec` and launches a session on their
behalf. Login **always starts in text mode** and offers a graphical
session only when a display driver and the window manager are present;
otherwise the graphical option is hidden — never crashed, never errored
(`AGENTS.md` §10). The installed binary lives at `/System/Services/login`.

The crate is `no_std` (with `alloc`), has no `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9). It
depends only on the audited `lib/*` crates `rustos-abi`, `rustos-caps`,
and `rustos-log`, so it never links a kernel or driver crate
(`AGENTS.md` §17.4).

## A policy machine, not a credential store

`Login` decides *what* to prompt for, *when* to retry, and *which*
session to start. It never reads the credential store, hashes a password,
or speaks to a terminal device. The three operations that reach the
outside world are injected seams, mirroring `init`'s `Spawner`/`Reaper`
design:

- `Prompt` — reads the username (echoed) and password (un-echoed) and
  writes prompts to the controlling terminal.
- `Authenticator` — verifies a `Credentials` pair against `kernel/sec`
  and the credential store, returning the user's identity and capability
  ceiling on success. Failures come back as the stable `Errno`, so login
  invents no parallel error vocabulary (`AGENTS.md` §2.2).
- `SessionLauncher` — starts the chosen `SessionKind` under the user's
  identity and blocks until it ends.

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
`SESSION_LAUNCH_FAILED`, `CONSOLE_ERROR`.

## Tests

`cargo test -p rustos-login` drives the state machine against in-memory
`Prompt`/`Authenticator`/`SessionLauncher` fixtures and a recording log
sink, covering a successful text login, the graphical option hidden when
unavailable, an offered graphical session selected and defaulted to text,
wrong-password retry then success, the lockout and zero-budget paths, a
dead console, and a refused session launch — plus the session-choice
parser, the `EventId` invariants, and the numeric audit-field formatter.

See [`docs/src/userland/login.md`](../../../docs/src/userland/login.md)
for the full subsystem documentation.
