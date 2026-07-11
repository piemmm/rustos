# Text login (`userland/session/login`)

`rustos-login` authenticates a user against `kernel/sec` and launches a
session on their behalf. It **always starts in text mode** and offers a
graphical session only when the desktop-session bundle is installed
**and** a display service is live (both re-probed each round, see the
`Run` binary below); when they are not, the graphical option is simply
hidden — never crashed, never errored (`AGENTS.md` §10). The installed
binary lives at `/System/Services/login.app/Run`.

The crate is `no_std` (with `alloc`), has no `unsafe`, and depends only on
the audited `lib/*` crates `rustos-abi`, `rustos-caps`, `rustos-log`,
`rustos-users`, and `rustos-vt` (the shared terminal-control vocabulary the
read line discipline keys off), so a userland service never links a kernel
or driver crate (`AGENTS.md` §17.4).

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

1. **Prompt** for a username (echoed in the login box) and a password
   (never rendered, via the `LoginView::read_password` seam —
   `AGENTS.md` §5).
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
failed attempt is always reported to the user as the running
`N failed attempts` count under the login box (rendered in red), so the
prompt cannot be used to probe for valid usernames (`AGENTS.md` §5).

The credential buffers are **caller-provided stack arrays**
(`INPUT_LINE_MAX`, 512 bytes — a §24.4 validation bound) that
`Credentials` borrows `&str` slices of: the view writes each keystroke
straight into them, so no copy of a credential ever lives on the heap.
`Login` validates each filled line as UTF-8 itself — line noise fails
closed as a console error, never reaching the authenticator — and zeroes
the password buffer after every attempt, success or failure (`AGENTS.md`
§4); the password is never logged.

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

The operations that touch the outside world are injected, mirroring
[`init`](init.md)'s `Spawner`/`Reaper` split:

- `LoginView` — presents the login screen and reads the username, the
  (never-rendered) password, and the session choice; the machine drives
  it through semantic calls (`round_begin`, `note_failure`,
  `session_handoff`), never raw terminal writes.
- `Authenticator::authenticate(&Credentials) -> Result<AuthenticatedUser, Errno>`
  — verifies credentials against `kernel/sec` and the credential store.
- `SessionLauncher::launch(&AuthenticatedUser, SessionKind) -> Result<SessionOutcome, Errno>`
  — starts the chosen session under the user's identity and blocks until
  it ends.
- `ElevateLauncher::run_as(program, uid) -> Result<i32, Errno>` — runs one
  re-authenticated elevated command as the target account and returns its
  exit code; `handle_elevate_request` drives it from the elevation broker
  (`plans/CAPABILITY_USE.md` CU5).

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
launches as the text session.

## The `Run` binary (`/System/Services/login.app/Run`)

`src/run.rs` is the shipped login service — the pure-Rust (`rustos-rt`,
`AGENTS.md` §1) program PID 1 `init`'s `session` directive launches and
supervises (`plans/PI.md` P11). It wires the real seams:

- **`LoginView`** as the full-screen curses view (`view::CursesView`)
  over the inherited standard streams (`AGENTS.md` §20): rendered bytes
  to fd 1, keystrokes from fd 0, drawn through the one `lib/curses`
  screen model (`AGENTS.md` §2.2). The page shows whoever is at the
  console *which system they are logging in to*: a white-on-blue top bar
  with the machine name, OS version, and the wall-clock time; the
  cyan-bordered `RustOS Login` box in the middle carrying the `Username:`
  prompt (which becomes `Password:`); a red, running `N failed attempts`
  line beneath the box that accumulates until a session launches; and a
  white-on-blue bottom bar with the memory in use, task count, logged-in
  users, and the 1/5/15-minute load averages — queried live from
  `sysinfod` (`SYSTEM_IDENTITY`, `KERNEL_MEMORY_STATS` under login's
  `CAP_SYSINFO_KERNEL`, and the ungated `LOAD_AVERAGE`) and the kernel
  wall clock. A refused or unavailable figure renders as a `--`
  placeholder and never blocks a login (`AGENTS.md` §2.24). While the
  prompt sits idle the bars refresh every five seconds: the field read
  waits with the `stream_read` timeout — a kernel park with a one-shot
  deadline, never a poll — and each elapsed bound re-queries the figures
  and repaints (`AGENTS.md` §2.23, §17.1 tickless). The view selects the
  raw (echo-off) discipline with `stream_input_mode`
  (`rustos_rt::set_input_mode`) for the whole page: it echoes the
  username into the box itself (bounded at the account format's
  `MAX_USERNAME_LEN`, 32, so the field can never overflow its one-line
  box — an over-long line is refused whole, never truncated), renders
  the shared `[input active...]` marker (`rustos_vt::secret`) in place of
  every hidden field's text — its dots animated by the shared
  `SecretIndicator` timer cadence (one frame per second, freezing after
  the bounded window with no input), never by a keystroke, so the
  operator sees input is live while the marker reveals nothing about how
  much was typed —
  and **refuses** the password read outright if raw mode cannot be
  selected (a credential must never be rendered, `AGENTS.md` §5.4). On
  session launch it leaves the alternate screen and restores the cooked
  discipline; the next round re-enters it.
- **`UsersAuthenticator`** over the database obtained through the
  capability-gated `users_db_read` syscall (`CAP_USERS_READ`, see
  [`architecture/syscalls.md`](../architecture/syscalls.md)) and re-parsed
  with the fail-closed `rustos-users` parser. `rustos_login::supervise`
  classifies each read into one of three states and acts on it **before
  every round** (never a once-read cache):
  - **Pending** (`Errno::WouldBlock`) — the encrypted root is still being
    unlocked. Under design B (`plans/PI.md` P11) `init` spawns `login`
    *before* the in-kernel root-unlock kthread mounts the root, and that
    kthread prompts for its passphrase on the **same** console. So while
    the read is pending `login` **parks** in the bounded `users_db_wait`
    syscall and does
    **not** print `Username:` — the unlock owns the console until it
    resolves, so the two prompts never draw over each other and `login`
    cannot steal the passphrase keystrokes (the kernel also gates
    console-0 *input* from `login` until then). The kernel takes the task
    off the run queue and wakes it when the unlock resolves; it is never a
    busy spin (`AGENTS.md` §2.1). The instant the unlock
    resolves — a database installed **or** a fail-closed give-up — the
    kernel hands console 0 back to `login`: it opens the console-0 input
    gate, arms the UART receive interrupt, and resolves the pending
    `users_db_read`, so the keyboard and serial `login` both receive input
    again. That release is bound to the unlock resolving on *every* outcome
    (the `on_resolved` callback of `unlock_root_disk_interactively`), so a
    successful unlock can never leave the console latched shut behind a
    mounted root (`AGENTS.md` §5.4.5).
  - **Present** — a delivered, valid database wires `UsersAuthenticator`
    for the round; `login` authenticates against it. Because the read
    happens per round, the database the unlock installs *after* `login`
    started is picked up by the next round.
  - **Absent** — the unlock resolved with no database (an installer image,
    or an unlock that gave up). A **deny-all** authenticator is wired: the
    prompt stays up and every attempt is refused (`AGENTS.md` §5.4.5, never
    an invented account).
- **`SessionLauncher`** over the `spawn`/`wait` syscalls: the chosen
  session's program — the record's shell of choice for a text session,
  the OS desktop-session bundle
  (`rustos_login::DESKTOP_SESSION_PATH`,
  `/System/Services/desktop.app/Run`) for a graphical one
  (`session_program`, one mapping defined beside `SessionKind`) — is
  spawned **as the authenticated user** and supervised;
  its exit code closes the session. Login holds `CAP_SPAWN_AS_USER` and
  starts the program through the uid-switching spawn with the session
  environment, so the kernel resolves the user's full credential (uid,
  primary gid, supplementary groups) from the authoritative identity
  table and snapshots it onto the child — privilege only ever switches
  user at process creation, never by a running process mutating its own
  identity (there is no setuid-self, `PREREQUISITES.md` P-C). The child
  still receives only its own manifest request intersected with the
  account ceiling (`AGENTS.md` §5.2) — the desktop session's
  `CAP_DISPLAY`/`CAP_INPUT_READ`/`CAP_SHM` are its manifest's, admitted
  because the session baseline carries the graphical class; the
  spawn-as-user switch sets the child's *identity*, not its capabilities.
- **The graphical-availability probe** (`plans/DISPLAY.md` D7d), run
  before every round and failing closed to "hidden": a read-only
  `fs_open` of the desktop bundle's `Run` path through the secured VFS
  (login's one filesystem code path, the reason its manifest carries
  `CAP_FS_ACCESS`; credentials still flow only through the gated
  `users_db_read` syscall), and one `Query` call to the reserved
  `DISPLAY_ENDPOINT`. Login holds no seat lease, so a live display
  service answers with a typed refusal — but any well-formed reply
  proves a service is serving the reserved rendezvous (only a
  `CAP_IPC_BIND_PRIVILEGED` holder can bind it), while an unbound
  endpoint fails the call itself. The probe learns nothing about the
  seat and gains no authority.
- **The elevation broker** (`plans/CAPABILITY_USE.md` CU5): at startup
  login binds its console's reserved elevation call endpoint
  (`rustos_abi::elevate::elevate_endpoint` over its own kernel-attested
  `Origin::console`; the reserved id needs login's
  `CAP_IPC_BIND_PRIVILEGED`, so a squatter can never claim it). While a
  session runs, the supervision wait is a kernel wait-set multiplexing the
  shell child (`WaitSourceKind::Child`) with the endpoint: an
  `elevate <user> <program>` request from the session's shell is
  placement-checked against the caller's attested console, decoded
  fail-closed, **re-authenticated with the same authenticator as the
  prompt** (refusals indistinguishable), and its program spawned as the
  target account and reaped while the shell blocks in its `ipc_call`; the
  request buffer is zeroed on every path (it carries the offered
  password). Elevation serialises per console (endpoint capacity 1) and a
  login that cannot bind a rendezvous audits `ELEVATE_UNAVAILABLE` and
  runs broker-less sessions — requests then fail closed at the missing
  endpoint.

Each finished session or exhausted attempt budget loops back to a fresh
prompt; a dead console exits fail-closed and `init` relaunches login. The
whole path from launch through prompt and credential collection is
allocation-free (see above) — only parsing a *delivered* user database
allocates, which arrives with the staged `mem_map` producer
(`plans/SPAWN.md` SP5b); login's audit records are emitted as terse lines
on fd 2 until a userland audit transport exists.

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
| 10007 | `ELEVATE_GRANTED`       | Info  | an elevation re-authenticated and its command ran to completion |
| 10008 | `ELEVATE_REFUSED`       | Warn  | an elevation was refused (cause audited, never disclosed) |
| 10009 | `ELEVATE_UNAVAILABLE`   | Warn  | no elevation rendezvous could be bound; sessions run broker-less |

## Tests

`cargo test -p rustos-login` drives the state machine against an in-memory
`LoginView`/`Authenticator`/`SessionLauncher` and a recording log sink,
covering a successful text login, the graphical option hidden when
unavailable, an offered graphical session selected and defaulted to text,
wrong-password retry then success, the fail-closed lockout and zero-budget
paths, a dead console, and a refused session launch — plus the
session-choice parser, the `EventId` range and uniqueness invariants, the
numeric audit-field formatter, and the `UsersAuthenticator` (full identity
mapping on success; one uniform refusal for a wrong password, an unknown
user, a locked account, and empty credentials). The `supervise` loop is
covered too: that `login` **waits without prompting** while the read is
pending and then authenticates once the database is installed
(`login_waits_while_pending_then_authenticates_once_installed`), that an
absent database prompts deny-all without waiting
(`an_absent_database_prompts_deny_all_without_waiting`), that a database
installed after the process started is picked up by a later round
(`the_users_database_is_reloaded_before_each_round`), and that a dead
console returns after one round rather than spinning.
