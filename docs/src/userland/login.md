# Text login (`userland/session/login`)

`rustos-login` authenticates a user against `kernel/sec` and launches a
session on their behalf. It **always starts in text mode** and offers a
graphical session only when a display driver and the window manager are
present; when they are not, the graphical option is simply hidden — never
crashed, never errored (`AGENTS.md` §10). The installed binary lives at
`/System/Services/login`.

The crate is `no_std` (with `alloc`), has no `unsafe`, and depends only on
the audited `lib/*` crates `rustos-abi`, `rustos-caps`, `rustos-log`, and
`rustos-users`, so a userland service never links a kernel or driver crate
(`AGENTS.md` §17.4).

## The policy machine, not a credential store

`login` decides *what* to prompt for, *when* to retry, and *which* session
to start. It deliberately does **not** read the credential store, hash a
password, or speak to a terminal device. Verifying the offered password
against the stored hash (with `lib/crypto`'s constant-time primitives —
`AGENTS.md` §16.4) is the `Authenticator` seam's job; `login` never sees
the stored hash.

## Login pipeline

`Login::run` repeats a bounded loop that **fails closed** (`AGENTS.md`
§5.4.5):

1. **Prompt** for a username (echoed) and a password (un-echoed, via the
   `Prompt::read_secret` seam — `AGENTS.md` §5).
2. **Authenticate** the `Credentials` through the `Authenticator`. A
   rejected attempt is audited and consumes one try; the bounded budget
   means a stuck or hostile console can never spin forever.
3. On success, **choose the session** — text by default, graphical offered
   only when `graphical_available` (`AGENTS.md` §10) — and hand the
   authenticated identity to the `SessionLauncher`.

If the attempt budget is exhausted, login launches nothing and returns
`LoginError::TooManyAttempts`. A terminal that cannot be read aborts with
`LoginError::Console`, and an authenticated user whose session will not
start returns `LoginError::SessionLaunch` — every terminal outcome is
fail-closed.

## No information leak

The `Authenticator` returns the **same** error whether the account is
unknown or the password is wrong, and `login` never inspects the cause: a
failed attempt is always reported to the user as `Login incorrect`, so the
prompt cannot be used to probe for valid usernames (`AGENTS.md` §5). The
password is held only for as long as the credential check takes and is
never logged; zeroing its freed allocation is the kernel allocator's job
(`AGENTS.md` §4).

## Capability handoff (`AGENTS.md` §5.2)

A successful authentication resolves to an `AuthenticatedUser` carrying the
`(uid, primary gid, supplementary gids, capability grants)` tuple
(`AGENTS.md` §5.1). The `capabilities` field is the user's grant
**ceiling**; `login` passes it verbatim to the `SessionLauncher`, which
drops to that identity and execs the shell or window manager. The loader
intersects the ceiling with the launched binary's signed manifest request
at exec time (`AGENTS.md` §5.2). `login` never widens it (`AGENTS.md` §4 —
no ambient authority).

## The seams

The three operations that touch the outside world are injected, mirroring
[`init`](init.md)'s `Spawner`/`Reaper` split:

- `Prompt` — reads the username and (un-echoed) password and writes
  prompts to the controlling terminal.
- `Authenticator::authenticate(&Credentials) -> Result<AuthenticatedUser, Errno>`
  — verifies credentials against `kernel/sec` and the credential store.
- `SessionLauncher::launch(&AuthenticatedUser, SessionKind) -> Result<SessionOutcome, Errno>`
  — starts the chosen session under the user's identity and blocks until
  it ends.

On a running kernel these are syscall- and `kernel/sec`-backed; in tests
they are in-memory fixtures. Splitting the seams from the state machine
keeps the security-relevant policy independent of kernel plumbing and
exhaustively testable.

## The production authenticator (`UsersAuthenticator`)

`auth::UsersAuthenticator` is the shipped `Authenticator`: it wraps a
parsed [`rustos-users`](../lib/users.md) database — the
`/System/Security/Users` text — and delegates the whole verification to
`UsersDb::authenticate` (PBKDF2-HMAC-SHA256 through `lib/crypto`,
constant-time hash comparison, and a timing-equalised refusal for unknown
or locked accounts, `AGENTS.md` §19.1). Every refusal is mapped to the
same `Errno::PermissionDenied`, and a success is mapped to the
`AuthenticatedUser` identity tuple straight from the matched record —
including the user's **shell of choice**, which the `SessionLauncher`
launches as the text session. Reading the database text off the root
volume (and wiring the launcher to the real `spawn` path) is the login
*binary*'s job, staged with the per-console session work in
`plans/PI.md`.

## Audit events

`login` owns the reserved `EventId` range `10000..11000`
(`AGENTS.md` §2.5, §19.4):

| Id    | Constant                | Level | Meaning                                          |
|-------|-------------------------|-------|--------------------------------------------------|
| 10001 | `SESSION_STARTED`       | Info  | a user authenticated and a session was launched  |
| 10002 | `AUTH_FAILED`           | Warn  | an authentication attempt was rejected           |
| 10003 | `LOCKED_OUT`            | Error | the attempt budget was exhausted; nothing started |
| 10004 | `SESSION_ENDED`         | Info  | a launched session returned                       |
| 10005 | `SESSION_LAUNCH_FAILED` | Error | a user authenticated but their session would not start |
| 10006 | `CONSOLE_ERROR`         | Error | the controlling terminal could not be read/written |

## Tests

`cargo test -p rustos-login` drives the state machine against an in-memory
`Prompt`/`Authenticator`/`SessionLauncher` and a recording log sink,
covering a successful text login, the graphical option hidden when
unavailable, an offered graphical session selected and defaulted to text,
wrong-password retry then success, the fail-closed lockout and zero-budget
paths, a dead console, and a refused session launch — plus the
session-choice parser, the `EventId` range and uniqueness invariants, the
numeric audit-field formatter, and the `UsersAuthenticator` (full identity
mapping on success; one uniform refusal for a wrong password, an unknown
user, a locked account, and empty credentials).
