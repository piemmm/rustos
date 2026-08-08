# Login: the session authority (`userland/session/login`)

`tairix-login` is the machine's **session authority**: the only component
that reads the user database, verifies a credential, and starts a session
under another account. It runs one round after another for the lifetime of
the machine, and each round collects a credential either at its own text
prompt or through the [graphical login screen](greeter.md), which is a
separate, unprivileged process that draws and types but decides nothing.
The installed binary lives at `/System/Services/login.app/Run`.

Which session runs is **system policy, never a per-login prompt**: the
graphical desktop by default, when the effective session type is graphical
**and** a graphical session is available this round, otherwise the
authenticated account's text shell. A graphical choice the round cannot
honour degrades to text — never crashed, never errored — and a shell user
starts the desktop on demand with the `desktop` command.

The **effective session type** comes from `effective_session_kind`, the
one definition of the precedence (`plans/NEW-DESKTOP-LOGIN.md` G1):

| Input | Effect |
| --- | --- |
| the operator's Supervisor choice (`continue text` / `continue gui`), read from the kernel through the ungated `boot_session_get` syscall | wins for this boot only; never persisted |
| the administrator's stored `os.loginType` (`text` \| `graphical`) | decides when the operator made no choice |
| neither, on a machine that carries no configuration | `graphical` |
| neither, and the store could not be reached | `text` for this round |

The last row is a distinction, not a detail. A round that runs before the
root unlock mounts `/System/Settings` can read nothing, and treating that
as "no configuration" would boot the compiled default over an
administrator who had configured the opposite. So an unreachable store
decides nothing: the round runs the text prompt, which is always
available and contradicts no stored choice, and the next round re-reads
once the volume is up. A *reachable* store holding no document is a
genuine "no configuration" and does take the default. An unreachable
store withholds only a default — the operator's `continue gui` still
wins on the very first round.

The crate is `no_std` (with `alloc`), has no `unsafe`, and depends only on
the audited `lib/*` crates `tairix-abi`, `tairix-caps`, `tairix-log`,
`tairix-users`, and `tairix-vt` (the shared terminal-control vocabulary the
read line discipline keys off), so a userland service never links a kernel
or driver crate — and, in particular, never a graphics crate: the drawing
stack lives in the greeter, not in the process that can mint any user's
identity.

## The policy machine, not a credential store

`login` decides *what* to prompt for, *when* to retry, and *which* session
to start. It deliberately does **not** read the credential store, hash a
password, or speak to a terminal device. Verifying the offered password
against the stored hash (with `lib/crypto`'s constant-time primitives) is
the `Authenticator` seam's job; `login` never sees the stored hash.

## Which login a round runs

Before each round `supervise` reloads the user database, and the round
puts the graphical login screen up when **all** of these hold — otherwise
it runs the text prompt:

- the effective session type is graphical (the table above);
- the greeter bundle is installed;
- the desktop bundle is installed;
- a display service answers the reserved `DISPLAY_ENDPOINT`;
- the reserved `session-v1` endpoint was bound at startup.

The middle three are re-probed **every round**, so a display service that
came up after boot arms a configured graphical default at the next round
and one that vanished degrades it again. Every absence selects the text
prompt; none of them is an error. A login that could not bind `session-v1`
has nothing for a login screen to ask, so it audits
`SESSION_ENDPOINT_UNAVAILABLE` once at startup and every round runs the
text prompt rather than claiming a rendezvous nothing answers.

The bundle probes are a read-only `fs_open` of each `Run` path, closed
again at once: the probe wants existence, not bytes, and keeps no
descriptor. The display probe is one `Query` call — login holds no seat
lease, so a live service answers it with a typed refusal, but *any*
well-formed reply proves something is serving the reserved rendezvous,
while an unbound endpoint fails the call itself. The probe learns nothing
about the seat and gains no authority.

## The graphical round

1. **Start one login screen** — `/System/Services/greeter.app/Run`, spawned
   as the `greeter` service account (uid 16) on login's own console.
2. **Serve `session-v1`** while it runs, on the same wait-set that
   supervises it. Anything already queued on the endpoint is answered and
   discarded before the screen starts, so a round never inherits a call
   posted while it was not listening — the endpoint holds one call, so an
   uncollected request from an unauthorised poster would otherwise block
   the next legitimate one.
3. **The screen exits `0`** once a secret was verified, which also frees
   the seat. The authority learns *which* account was accepted from its own
   authenticator, never from anything the login screen said.
4. **Bring that account's session to the foreground** and supervise it
   until it ends or steps aside; then open a fresh round.

A login screen that exits without an accepted verdict — it failed to start,
was dismissed, or died — is audited (`GREETER_FAILED`) and restarted. After
three consecutive failures the round says so on `stderr`, audits
`GREETER_DEGRADED`, and runs the text prompt instead: a broken login screen
can never leave a machine impossible to log in to.

## `session-v1`, the login screen's channel

At startup login binds the reserved `SESSION_ENDPOINT` for the process's
lifetime. A reserved id needs `CAP_IPC_BIND_PRIVILEGED`, so a squatter
cannot impersonate the authority; senders are unrestricted and the capacity
is one, because each exchange is serialised by design and who may *post* is
not the security boundary — placement and identity are enforced per
request.

| Request | Sent by | Reply |
| --- | --- | --- |
| `Accounts { offset }` | the login screen | one `AccountPage`: a display name, a login name, and a live-session flag per account, plus the whole list's `total` |
| `Authenticate { username, password }` | the login screen | a verdict. It starts nothing |
| `Background` | the presenting desktop session | a verdict: the session is now recorded as background |

The list is **paged** because a machine may have more accounts than one
reply could hold; a client walks pages until it has `total` of them. A
record carries only what a tile draws — never a password hash, a uid, a
capability ceiling, or a home path — and only accounts a login could
actually succeed for are offered (active, with both a home and a shell), so
the chooser never invites someone to type a secret at a tile that could
never accept it. "Which accounts exist" and "which are already logged in"
are one question about one set of records, so they are one request with a
flag per record rather than two answers that could disagree.

`Authenticate` deliberately starts nothing: it answers a verdict, and the
authority acts on its own loop, so a compromised login screen can never
choose which program runs as the authenticated user.

### The gate

Every request is checked against the caller's **kernel-attested** identity
(`call_peer_origin`), never a claim in the message. In order, each step
failing closed:

1. **Shape** — the frame must decode. Decoding is bounded, total, and
   touches no state, so it may precede the identity checks; all it decides
   is which *shape* of refusal an unauthorised caller gets back.
2. **Placement** — the attested console must be login's own, exactly as for
   the elevation broker.
3. **Identity** — `Accounts` and `Authenticate` require the attested uid to
   be the `greeter` service account. `Background` requires the attested uid
   to own the entry the session table records as the foreground one: the
   greeter holds no session and is refused it, a background session cannot
   use it to take the screen back, and nothing else can take the screen
   away from the person using it.
4. **Adjudication** — only now is any state read or changed.

### One refusal, and an empty page

`Refused` has no reason field at all. An unknown account, a wrong password,
a locked account, an authority with no database, and a caller that is not
the greeter are indistinguishable on the wire, so a reply can never be used
to probe for accounts; why a login was refused lives in the audit trail
alone. The verify path is timing-equalised in `lib/users`, as the text
prompt and the elevation broker already are.

The one thing a refusal carries is `retry_after`, the remaining per-account
cooldown — a screen that could not say "wait thirty seconds" would leave
the user pressing a key that silently does nothing.

An unauthorised or undecodable request is answered with a well-formed
**empty account page**, never an errno, so a client bug reaches the login
screen as the protocol fault it is rather than being shown to the user as a
wrong password. A refusal is always shaped like the request that was sent,
so a stranger cannot tell a placement refusal from any other.

The request buffer holds an offered secret, so it is zeroed on every path
out of the handler. The secret is never logged, never placed in an
`stdinfo` record, and never carried in a reply.

## The attempt budget

Guessing at a login screen costs nothing but keystrokes, so the authority
meters it. The budget lives here rather than in the greeter deliberately: a
client-side limit would be duplicated in every surface and trivially
bypassed by a caller that simply does not implement it.

- **Three free attempts** per account, matching the text prompt's per-round
  budget — mistyping a password is ordinary human behaviour and must not
  put a delay in front of the person at the machine.
- **The fourth refusal costs five seconds**, and every further one doubles
  it to a **five-minute** cap. Capped rather than unbounded, so an account
  cannot be delayed out of use: a retry is always within a knowable time.
- **Per login name**, so a wrong password for one account never delays
  another.
- **Monotonic**: every instant is the caller's reading of the monotonic
  clock, so a cooldown cannot be shortened by moving the wall clock. The
  engine reads no clock of its own.
- A cooling-down account is refused **before** the authenticator is called
  — the point of a cooldown is that the guess is not adjudicated at all —
  and an attempt made during one does not extend it. A success clears that
  account's entry.

The table tracks **sixteen** accounts. That is a validation bound over
names an untrusted caller chooses, not a capacity that grows on demand: a
table that grew would let a caller cycling invented names allocate without
limit. A newcomer takes a free slot, or the entry whose cooldown expired
longest ago — a heavily-guessed account has the longest cooldown and so the
latest deadline, which makes it the *last* entry such a caller can
displace. When every entry is still cooling down, the newcomer inherits the
table's **shortest** remaining wait instead of evicting anyone: cycling
invented names buys no unmetered guess, and the wait is by construction
bounded by the time until the soonest slot frees.

## Live sessions and fast user switching

The authority keeps the **session table**: one entry per account with a
live desktop session, holding its uid, its task id, and whether it is the
foreground one. At most one entry is foreground, enforced by construction —
promoting one demotes whatever held it. The wake mailbox is *derived* from
the task id (`session_wake_endpoint`) rather than stored, so the id the
authority posts to and the id the session bound cannot drift apart. The
table is also what supplies the login screen's live badge.

**Switching away.** The presenting desktop session sends `Background`. Only
on `Accepted` does it tear down presentation and release the seat —
releasing first would black the screen with nobody drawing. It keeps
running and parks on its wake mailbox; the authority stops supervising it,
keeps its entry, and puts the login screen back up.

**Switching back.** When the login screen accepts an account that already
has a live session, the authority does **not** start a second one: it wakes
that session through its mailbox and promotes it (`SESSION_RESUMED`). A
wake that cannot be delivered means the session may already be gone, so it
is reaped non-blockingly to find out — a reaped session leaves the table
and a fresh desktop starts, while one still running keeps its entry and the
round returns to the login screen rather than duplicating a desktop for one
account. Returning to a background session still requires a successful
`Authenticate` for it; there is no path from the login screen to a live
session without one.

**Logging out** is different, and the round tells the two apart explicitly.
A session that *exited* loses its entry and is audited as ended; one that
*stepped aside* keeps its processes and its entry and stays resumable — the
broker already audited that decision, so nothing is recorded twice.

**When the authority itself exits** (a dead console; PID 1 relaunches it)
it drains the table newest-first, sending every entry `SessionWake::End`. A
relaunched authority starts with an empty table, so a background session it
had not ended would be unreachable for ever: holding memory, owning no
seat, and with nothing left that could wake it. An undeliverable wake is
audited and skipped — never retried, never waited on — so one wedged
session cannot hold the exit open.

## The text round

`Login::run` repeats a bounded loop that **fails closed**:

1. **Prompt** for a username (echoed in the login box) and a password
   (never rendered, via the `LoginView::read_password` seam).
2. **Authenticate** the `Credentials` through the `Authenticator`. A
   rejected attempt is audited and consumes one try; the bounded budget
   means a stuck or hostile console can never spin forever.
3. On success, **decide the session** — pure policy, no prompt — and hand
   the authenticated identity to the `SessionLauncher`: the desktop when
   the effective session type is graphical and a graphical session is
   available this round, the account's own shell otherwise.

If the attempt budget is exhausted, login launches nothing and returns
`LoginError::TooManyAttempts`. A terminal that cannot be read aborts with
`LoginError::Console`, and an authenticated user whose session will not
start returns `LoginError::SessionLaunch` — every terminal outcome is
fail-closed.

The text round keeps no session-table entry, so a session it starts cannot
step aside; fast user switching is a property of the graphical round.

## The terminal a session leaves behind

A text console is shared by everyone who sits at it, so the end of a
session is a boundary, not just a return to the prompt. Whatever the
session left on the terminal is **discarded** before the next round
prompts: the screen it drew, the screen it was *not* showing (a
full-screen program's alternate screen, which no erase sequence written to
the console can reach), a remote emulator's saved scrollback, and the
keystrokes it typed ahead but never read — which could otherwise be
delivered to the next user's prompt as if they had typed them. The read
line discipline returns to cooked, so the next round starts from the
interactive default however the last session left it.

One call does all of it (`terminal_purge`, see
[Syscalls](../architecture/syscalls.md)); `login` holds both halves of the
console authority it requires, and the kernel does the discarding, so the
login never depends on a terminal honouring an escape sequence. It happens
on **every** session boundary — a clean exit, a session refused at load,
one that never started — because in each case the terminal is about to be
offered to whoever is there next. A merely *rejected credential* is not a
session boundary: nothing ran, so nothing is discarded and the failed
attempt keeps the login box it is drawing on. A terminal that refuses the
discard is reported by the console, not by `login`, which re-prompts
regardless.

## No information leak

The `Authenticator` returns the **same** error whether the account is
unknown or the password is wrong, and `login` never inspects the cause: a
failed attempt is always reported to the user as the running
`N failed attempts` count under the login box (rendered in red), so the
prompt cannot be used to probe for valid usernames.

The credential buffers are **caller-provided stack arrays**
(`INPUT_LINE_MAX`, 512 bytes — a validation bound) that `Credentials`
borrows `&str` slices of: the view writes each keystroke straight into
them, so no copy of a credential ever lives on the heap. `Login` validates
each filled line as UTF-8 itself — line noise fails closed as a console
error, never reaching the authenticator — and zeroes the password buffer
after every attempt, success or failure; the password is never logged.

## Capability handoff

A successful authentication resolves to an `AuthenticatedUser` carrying the
`(uid, primary gid, supplementary gids, capability grants)` tuple. The
`capabilities` field is the user's grant **ceiling**; `login` passes it
verbatim to the `SessionLauncher`, which drops to that identity and execs
the shell or window manager. The loader intersects the ceiling with the
launched binary's signed manifest request at exec time. `login` never
widens it: there is no ambient authority to widen it with.

## The seams

The operations that touch the outside world are injected, mirroring
[`init`](init.md)'s `Spawner`/`Reaper` split:

- `LoginView` — presents the login screen and reads the username and the
  (never-rendered) password; the machine drives
  it through semantic calls (`round_begin`, `note_failure`,
  `session_handoff`, `session_ended`), never raw terminal writes. A
  terminal resize is
  never taken for a keystroke: the page is re-laid-out and the box
  re-centred on the new size, and the field under edit is kept whole.
  `session_handoff` gives the terminal to the launched session and
  `session_ended` takes it back, discarding everything the session left on
  it.
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
parsed [`tairix-users`](../lib/users.md) database — the
`/System/Security/Users` text — and delegates the whole verification to
`UsersDb::authenticate` (PBKDF2-HMAC-SHA256 through `lib/crypto`,
constant-time hash comparison, and a timing-equalised refusal for unknown
or locked accounts). Every refusal is mapped to the same
`Errno::PermissionDenied`, and a success is mapped to the
`AuthenticatedUser` identity tuple straight from the matched record —
including the user's **shell of choice**, which the `SessionLauncher`
launches as the text session. The graphical round verifies through the
same seam, so a login screen's `Authenticate` and the text prompt cannot
adjudicate differently.

## The `Run` binary (`/System/Services/login.app/Run`)

`src/run.rs` is the shipped login service — the pure-Rust (`tairix-rt`)
program PID 1 `init`'s `session` directive launches and supervises
(`plans/PI.md` P11). It wires the real seams:

- **`LoginView`** as the full-screen curses view (`view::CursesView`)
  over the inherited standard streams: rendered bytes
  to fd 1, keystrokes from fd 0, drawn through the one `lib/curses`
  screen model. The page shows whoever is at the
  console *which system they are logging in to*: a white-on-blue top bar
  with the machine name, OS version, and the wall-clock time; the
  cyan-bordered `TAIRiX Login` box in the middle carrying the `Username:`
  prompt (which becomes `Password:`); a red, running `N failed attempts`
  line beneath the box that accumulates until a session launches; and a
  white-on-blue bottom bar with the memory in use, task count, logged-in
  users, and the 1/5/15-minute load averages — queried live from
  `sysinfod` (`SYSTEM_IDENTITY`, `KERNEL_MEMORY_STATS` under login's
  `CAP_SYSINFO_KERNEL`, and the ungated `LOAD_AVERAGE`) and the kernel
  wall clock. A refused or unavailable figure renders as a `--`
  placeholder and never blocks a login. While the
  prompt sits idle the bars refresh every five seconds: the field read
  waits with the `stream_read` timeout — a kernel park with a one-shot
  deadline, never a poll — and each elapsed bound re-queries the figures
  and repaints. The view selects the
  raw (echo-off) discipline with `stream_input_mode`
  (`tairix_rt::set_input_mode`) for the whole page: it echoes the
  username into the box itself (bounded at the account format's
  `MAX_USERNAME_LEN`, 32, so the field can never overflow its one-line
  box — an over-long line is refused whole, never truncated), renders
  the shared `[input active...]` marker (`tairix_vt::secret`) in place of
  every hidden field's text — its dots animated by the shared
  `SecretIndicator` timer cadence (one frame per second, freezing after
  the bounded window with no input), never by a keystroke, so the
  operator sees input is live while the marker reveals nothing about how
  much was typed —
  and **refuses** the password read outright if raw mode cannot be
  selected: a credential must never be rendered. On
  session launch it leaves the alternate screen and restores the cooked
  discipline; the next round re-enters it.
- **`UsersAuthenticator`** over the database obtained through the
  capability-gated `users_db_read` syscall (`CAP_USERS_READ`, see
  [`architecture/syscalls.md`](../architecture/syscalls.md)) and re-parsed
  with the fail-closed `tairix-users` parser. `tairix_login::supervise`
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
    busy spin. The instant the unlock
    resolves — a database installed **or** a fail-closed give-up — the
    kernel hands console 0 back to `login`: it opens the console-0 input
    gate, arms the UART receive interrupt, and resolves the pending
    `users_db_read`, so the keyboard and serial `login` both receive input
    again. That release is bound to the unlock resolving on *every* outcome
    (the `on_resolved` callback of `unlock_root_disk_interactively`), so a
    successful unlock can never leave the console latched shut behind a
    mounted root.
  - **Present** — a delivered, valid database wires `UsersAuthenticator`
    for the round; `login` authenticates against it. Because the read
    happens per round, the database the unlock installs *after* `login`
    started is picked up by the next round.
  - **Absent** — the unlock resolved with no database (an installer image,
    or an unlock that gave up). A **deny-all** authenticator is wired: the
    prompt stays up and every attempt is refused, never an invented
    account. A graphical round then offers no accounts at all, which is the
    honest answer — nothing could authenticate either.
- **`SessionLauncher`** over the `spawn`/`wait` syscalls: the chosen
  session's program — the record's shell of choice for a text session,
  the OS `desktop` application
  (`tairix_login::DESKTOP_SESSION_PATH`,
  `/System/Applications/desktop.app/Run` — the same bundle the shell's
  `desktop` command word resolves to) for a graphical one
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
  account ceiling — the desktop session's
  `CAP_DISPLAY`/`CAP_INPUT_READ`/`CAP_SHM` are its manifest's, admitted
  because the session baseline carries the graphical class; the
  spawn-as-user switch sets the child's *identity*, not its capabilities.
- **The graphical-availability probe** (`plans/DISPLAY.md` D7d),
  described above. Its bundle reads go through the secured VFS — login's
  one filesystem code path, and the reason its manifest carries
  `CAP_FS_ACCESS`; credentials still flow only through the gated
  `users_db_read` syscall.
- **The `session-v1` broker**, bound at startup and served on the same
  wait-set that supervises the round's child. `handle_session_request` is
  a pure function over injected seams, exactly like the elevation broker,
  so every branch of the gate is host-tested and the `Run` binary owns
  only the receive-decide-reply loop and the syscall-backed seams. The
  receive is non-blocking: the wait-set also supervises a child, so a wake
  whose queued call was cancelled must not park the loop.
- **The elevation broker** (`plans/CAPABILITY_USE.md` CU5): at startup
  login binds its console's reserved elevation call endpoint
  (`tairix_abi::elevate::elevate_endpoint` over its own kernel-attested
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

`login` owns the reserved `EventId` range `10000..11000`. The numeric
values are stable: once shipped they are never re-used or renumbered,
because external audit-log consumers key off them.

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
| 10010 | `FONTD_STARTED`         | Info  | the sandboxed font service was started for a display-capable machine |
| 10011 | `FONTD_UNAVAILABLE`     | Warn  | the font service would not start; the session launches without it |
| 10012 | `VERIFY_GRANTED`        | Info  | a `Verify` request re-authenticated its own attested account; nothing ran |
| 10013 | `VERIFY_REFUSED`        | Warn  | a `Verify` request was refused (cause audited, never disclosed) |
| 10014 | `SESSION_AUTH_GRANTED`  | Info  | a `session-v1` authentication succeeded; nothing started yet |
| 10015 | `SESSION_AUTH_REFUSED`  | Warn  | a `session-v1` authentication was refused, or the account was cooling down |
| 10016 | `SESSION_REQUEST_REFUSED` | Warn | a `session-v1` request was not served: wrong caller, wrong console, or a frame that would not decode |
| 10017 | `SESSION_ACCOUNTS_SENT` | Info  | an account page was disclosed to the login screen |
| 10018 | `SESSION_ENDPOINT_UNAVAILABLE` | Warn | `session-v1` could not be bound; every round uses the text login |
| 10019 | `GREETER_FAILED`        | Warn  | a login screen exited with no accepted verdict; a fresh one is started |
| 10020 | `GREETER_DEGRADED`      | Warn  | consecutive login-screen failures spent the round's budget; the text login runs |
| 10021 | `SESSION_RESUMED`       | Info  | an existing desktop session was woken to the foreground instead of a second one being started |
| 10022 | `SESSION_BACKGROUNDED`  | Info  | the presenting session stepped aside and stays resumable |
| 10023 | `SESSION_ENDED_ON_EXIT` | Info/Warn | a live session was told to end because the authority is exiting (`Warn` when the wake could not be delivered) |

A refusal record names the account offered and the attested uid, never the
offered secret, and never *which* credential fault it was: refusals stay
indistinguishable even to a reader comparing audit entries. Only the
`SESSION_AUTH_REFUSED` record distinguishes a live cooldown from an
adjudicated failure.

## Tests

`cargo test -p tairix-login` drives the state machine against an in-memory
`LoginView`/`Authenticator`/`SessionLauncher` and a recording log sink,
covering a successful text login, a configured text session launching the
shell even when a graphical session is available, the graphical default
starting the desktop (and degrading to text when headless),
wrong-password retry then success, the fail-closed lockout and zero-budget
paths, a dead console, and a refused session launch — plus the
`EventId` range and uniqueness invariants, the
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

The graphical half is covered to the same depth. `handle_session_request`
is driven over every branch of its gate — a foreign console, a caller that
is not the greeter, an unattested caller, an undecodable frame, a page
walk, a wrong password, a cooling-down account, and each `Background`
refusal — asserting both the reply bytes and the audited event id. The
budget's schedule, its eviction rule and its full-table behaviour are
pinned by their own tests, as are the session table's invariant (at most
one foreground entry, re-asserted after every mutation), switch-away,
switch-back, logout, a background session that dies, two accounts
alternating, and the newest-first exit drain including an undeliverable
wake. The wire itself round-trips and fail-closed-decodes in `lib/abi` and
is hammered by its fuzz harness (`cargo xtask fuzz`, target
`tairix-abi/fuzz_decode`), and the greeter's own suite wires its client
seam straight to this broker, so the two halves of the protocol are proven
against each other rather than each against its own mock.
