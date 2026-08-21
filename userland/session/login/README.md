# `tairix-login` — the TAIRiX session authority

Stage 6 deliverable (`AGENTS.md` §3 `userland/session/`, §10). It
authenticates a user against `kernel/sec` and launches a session on their
behalf. Which session runs is **system policy, never a per-login
prompt**: the graphical desktop by default (`os.loginType graphical`,
`lib/sysconfig`) when a graphical session is available, otherwise the
authenticated account's text shell — never crashed, never errored
(`AGENTS.md` §10) — and a shell user starts the desktop on demand with
the `desktop` command. The installed binary lives at
`/System/Services/login.app/Run`.

A round that *can* be graphical puts the login screen up instead of the
text prompt: login starts `greeter.app` as the unprivileged `greeter`
service account and serves it over the `session-v1` endpoint
(`plans/NEW-DESKTOP-LOGIN.md`). Login remains the only component that
verifies a credential and the only one that starts a process — the login
screen draws and types, nothing more.

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
  rendered). A terminal resize is never taken for a keystroke: the page
  is re-laid-out and the box re-centred on the new screen size, and the
  field under edit survives untouched.
- `Authenticator` — verifies a `Credentials` pair against `kernel/sec`
  and the credential store, returning the user's identity and capability
  ceiling on success. Failures come back as the stable `Errno`, so login
  invents no parallel error vocabulary (`AGENTS.md` §2.2).
- `SessionLauncher` — starts the chosen `SessionKind` under the user's
  identity and blocks until it ends.
- `SessionDirectory` (`broker.rs`) — the authority's session-and-account
  view as the rendezvous may see it. The production `DbAccounts` filters
  the parsed database down to accounts a login could actually succeed for
  (active, with a home and a shell), so no-login service accounts and
  locked accounts are absent from the chooser entirely, and takes each
  account's `live` badge from the session table. Its one mutation is the
  presenting session stepping aside.
- `SessionWaker` (`table.rs`) — posts a wake to a live session's mailbox:
  the resume that brings it back to the screen, and the end sent when the
  authority itself exits.
- `ElevateLauncher` (`elevate.rs`) — runs one re-authenticated
  `elevate <user> <program>` command as the target account and returns its
  exit code, **or** starts it and returns its pid without waiting
  (`launch_as`), which is what a graphical caller needs: a desktop cannot
  wait for a program it must keep serving windows to
  (`plans/CAPABILITY_USE.md` CU5). Both forms take one spawn, so they can
  never start a program under different terms. The decision logic
  (`handle_elevate_request`) placement-checks the caller's attested
  console, decodes fail-closed, re-authenticates through the same
  `Authenticator` as the prompt, and audits every grant and refusal; the
  `Run` binary serves it over the console's reserved elevation endpoint
  while the session runs.

On a running kernel these are syscall- and `kernel/sec`-backed; in tests
they are in-memory fixtures, so every control-flow decision is testable
without a kernel.

## The graphical login screen (`session-v1`)

`broker.rs` holds the whole decision surface as one pure function,
`handle_session_request`, modelled on the elevation broker: no syscalls,
fully host-tested, with the `Run` binary supplying the seams. It answers
three requests, under two layers of check. **Placement** is shared: the
caller's *kernel-attested* console must be login's own, never a claim in
the message. **Identity** is then per request:

- `Accounts` and `Authenticate` are the login screen's, and require the
  attested uid to be the `greeter` service account.
- `Background` is the *desktop session's* — "I am giving up the screen,
  put the login screen back up" — and requires the attested uid to own the
  entry the session table records as presenting. The greeter holds no
  session and is refused it, and a background session cannot use it to
  take the screen back.

Every answer is deliberately uninformative. An unauthorised or
unparseable request gets a well-formed **empty** account page, which
discloses nothing and reaches a broken client as the protocol fault it is
rather than as a wrong password. Every authentication failure — unknown
account, wrong password, locked account, no database, unattested caller —
returns the identical `Refused` frame, as does every refused `Background`;
the cause is audited, never put on the wire. `Authenticate` **starts
nothing**: it answers a verdict, and login acts on its own loop, so a
compromised login screen can never choose which program runs as the
authenticated user. The request buffer carries the offered secret and is
zeroised on every path out.

`budget.rs` meters guessing, in the authority rather than the client. The
`AttemptBudget` is per **login name** (a wrong password for one account
never delays another), driven only by a caller-supplied *monotonic*
reading (so a cooldown cannot be shortened by moving the wall clock), and
bounded: 3 free attempts, then 5 s doubling to a 300 s cap, tracked in a
fixed 16-entry table — a validation bound over attacker-chosen names, not
a capacity that grows. A full table evicts an entry whose cooldown has
already expired; when every entry is still cooling down the newcomer
inherits the table's *minimum* remaining wait instead, so cycling invented
names buys no unmetered guesses and no permanent lockout can result. A
success clears that account.

`table.rs` records which accounts have a live desktop session — login
name, uid, session pid, foreground/background, and the wake mailbox id —
for fast user switching. **At most one entry presents**, enforced by
construction: promoting one demotes whatever held the seat. An account
that authenticates while it already has a live session is *resumed*
through its wake mailbox; a second desktop is never started for one
account. It is a growable list, not a fixed-ceiling array.

Switching *away* is the other half. A served `Background` moves the
presenting entry to the background and hands the round back its
supervision with the session still running: the round leaves the entry
alone — no removal, no "ended" record — and puts the login screen back up,
so the account still shows as live and can be resumed. Only a session that
**exited** loses its entry.

`end_live_sessions` closes the other end. When a round reports the console
dead the process exits and PID 1 relaunches `login` with an empty table, so
a session left recorded as background would hold no seat and have nothing
that could ever wake it again. Before returning, the authority therefore
drains the table newest first and tells each session to end, auditing every
one; an undeliverable wake is recorded and skipped, never retried, so a
wedged session cannot hold the exit open.

The `Run` binary multiplexes the elevation endpoint, the `session-v1`
endpoint, the running child, and every child a non-blocking elevated launch
started on **one** reusable wait-set, so every wait parks on an event and
nothing polls. A launched child is *login's*, not the requester's, so login
owns collecting it: each is joined to that wait-set under its own token, and
its exit wakes whichever supervision is parked, which then reaps
non-blockingly and audits an exit that was not clean. The sweep also runs
whenever supervision begins, so an exit that landed between watches is not
missed, and it drops any entry that is no longer login's to reap — keeping
one would wake supervision for ever.

A login screen that exits without an accepted verdict is restarted; three
consecutive failures
degrade the round to the text login, with the reason on `stderr` and in
the audit trail, so a broken login screen can never leave the machine
impossible to log in to.

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
`ELEVATE_REFUSED`, `ELEVATE_UNAVAILABLE`, `FONTD_STARTED`,
`FONTD_UNAVAILABLE`, `VERIFY_GRANTED`, `VERIFY_REFUSED`,
`SESSION_AUTH_GRANTED`, `SESSION_AUTH_REFUSED`, `SESSION_REQUEST_REFUSED`,
`SESSION_ACCOUNTS_SENT`, `SESSION_ENDPOINT_UNAVAILABLE`,
`GREETER_FAILED`, `GREETER_DEGRADED`, `SESSION_RESUMED`,
`SESSION_BACKGROUNDED`, `SESSION_ENDED_ON_EXIT`, `LAUNCH_GRANTED`,
`LAUNCH_REFUSED`, `LAUNCH_ENDED_ABNORMALLY`.

The last of those is the reaper's: a program started by a non-blocking
elevated launch writes its `stderr` to login's console, which behind a
desktop is the framebuffer text console nobody sees, so login states an
abnormal exit where a user can find it — naming a reserved load-failure
status in words through the one shared map, and otherwise stating the code.
A clean exit records nothing.

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
indistinguishable refusals, spawn refusal reported verbatim, and the
non-blocking launch taking the launch seam rather than the blocking one,
refused indistinguishably on a wrong password and audited apart, and its
child's abnormal exit audited once — silently for a clean one, named in
words for a reserved load-failure status and by code alone for any other).

The `session-v1` surface is covered the same way: the broker refusing a
caller whose attested uid is not the greeter and one on another console
(separately), an unauthorised accounts request yielding an empty page,
every authentication failure mode producing a byte-identical refusal, the
secret reaching neither a reply nor an audit field, the request buffer
wiped, paging across more accounts than one page, and the directory
filtering out no-login and locked accounts; the step-aside accepted from
the presenting session and refused — byte-identically — from the greeter,
from a background session, from an unattested caller, from another
console, and when nothing presents, with the entry surviving and
resumable afterwards; the budget's free attempts, doubling and capped
cooldown, per-account isolation, success reset, table-full eviction and
all-cooling-down fallback; the session table's single-foreground invariant
across switch-away/back, logout, a background session that dies and two
accounts alternating; and the exit drain ending every session newest
first, auditing each, and carrying on past an undeliverable wake.

See [`docs/src/userland/login.md`](../../../docs/src/userland/login.md)
for the full subsystem documentation.
