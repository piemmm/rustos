# NEW-DESKTOP-LOGIN.md — the graphical login screen and session switching

Binding under `AGENTS.md`. This is the normative specification for how a
TAIRiX machine reaches a *logged-in* state: the boot-time choice between a
text and a graphical login, the first-class **graphical login screen**
(the *greeter*), the **session authority** that owns authentication and
session lifetime, and macOS-style **fast user switching** between several
concurrently live desktop sessions on one seat.

Read first, in order: `AGENTS.md` (all of it, especially §2, §4, §5, §10,
§16, §17.3, §19, §23, §26, §27), `plans/DISPLAY.md` (the seat/display
ownership model this builds on), `plans/GUI-CONTROLS-DESIGN.md` (the
Reactive Alloy `lib/controls` vocabulary every surface here composes — **no
second control implementation**), `plans/PINBOARD.md` (the wallpaper store,
catalog, fit geometry and sandboxed decode this reuses), `plans/APPS.md`
(the bundle model), `plans/CAPABILITY_USE.md` (CU5 elevation, capability
sizing), `plans/USERS.md` (service accounts and id ranges),
`plans/NEW-SUPERVISOR.md` (the pre-boot REPL), `plans/ICONS.md` (asset
tiers), and `plans/FONT-SERVICE.md` (`fontd`). Every rule in all of them
applies here without exception.

**Note:** `abi-v1` is *not* frozen before the first release, so the
`lib/abi` additions below are in-place evolution (`AGENTS.md` §2.13, §9).
Every `lib/abi` change regenerates the C header
(`cargo xtask c-header --write`); the drift guard enforces it.

## Status

**In progress.** Done: **G1** (the boot session decision) and **G2**/**G6**
(the `lib/greeter` surface engine, with the desktop's screen lock composing
it). Remaining, in order: **G3** (the `greeter.app` service bundle), **G4**
(the `session-v1` broker in `login` and the graphical round), **G5** (fast
user switching), **G7** (docs, README matrix, QEMU verticals). Each
deliverable's section states what it guarantees now and what remains.

Until G3 and G4 land, a graphical login type still runs the **text** prompt
and then starts the desktop: the login screen itself is the work G3/G4 do.
G3 and G4 land **together** — a broker with no caller, or a greeter with
nothing to ask, is speculative surface neither may ship alone.

## Terminology

The keywords **MUST**, **MUST NOT**, **SHOULD**, **SHOULD NOT** and **MAY**
are implementation requirements.

- **Seat** — one display with its keyboard and pointer (`plans/DISPLAY.md`).
  `SEAT_PRIMARY` is the boot seat. Exactly one task holds a seat's lease.
- **Greeter** — the graphical login screen: the program that owns the seat
  whenever no user session is in the foreground.
- **Session authority** — `login` (`/System/Services/login.app`): the only
  component that reads the user database, verifies a credential, and starts
  a session under another account. It is the trust anchor.
- **Desktop session** — one running `desktop.app` process, owned by one
  authenticated account.
- **Foreground session** — the one desktop session (or the greeter) that
  currently holds the seat lease.
- **Background session** — a live desktop session that does not hold the
  lease. Its processes keep running; it presents nothing.

## G0. Why the greeter is a separate process

The greeter draws a full-screen surface over a **decoded wallpaper**, so it
is a consumer of untrusted image bytes (§19.5) and links the whole
`lib/controls`/`lib/raster`/`lib/font` drawing stack. The session authority
holds `CAP_USERS_READ` and `CAP_SPAWN_AS_USER` — the two most dangerous
grants on the machine.

Folding the drawing stack into the authority would put a large parsing and
rendering surface inside the one process that can mint any user's identity.
That is a security regression (§2.7, §19.5) and is forbidden here. The two
are therefore separate processes with a narrow, versioned channel between
them:

- the **greeter** knows how to draw and how to collect a name and a secret;
  it holds no capability that can read the user database or start a session;
- the **authority** knows how to verify and how to start a session; it draws
  nothing and never links a graphics crate.

This also keeps §17.3 clean: `userland/session/*` gains no edge to
`userland/gui/*`. The greeter composes `lib/*` crates only, exactly as the
text view composes `lib/curses`.

## G1. The boot session decision

**Status: done.**

Three inputs decide whether a boot ends at a text login or a graphical one.
They are evaluated in this order, highest first:

1. **The operator's supervisor choice**, if one was made this boot
   (`continue text` / `continue gui`). One boot only; never persisted.
2. **The administrator's stored default**, `os.loginType` in the
   `lib/sysconfig` store (`text` — the default — or `graphical`).
3. **The compiled default**, `text`.

A graphical choice is *offered* only when a graphical login is actually
possible this round (G4's availability probe). When it is not, the boot
degrades to the text login — never an error, never a blank screen (§2.24,
§5.4).

### G1.1 `continue [text|gui]`

`lib/supervisor`'s `continue` command (aliases `boot`) takes an optional
single operand:

| Spelling | Meaning |
| --- | --- |
| `continue` | resume the boot with no override; the stored default decides |
| `continue text` | resume the boot and force a text login this boot |
| `continue gui` | resume the boot and force a graphical login this boot |

`graphical` and `desktop` are accepted as spellings of `gui`; `console` as a
spelling of `text`. Any other operand, or more than one, is refused with the
usage line and the REPL stays open (fail closed — an ambiguous instruction
never resumes the boot). Matching is case-insensitive, as for every other
supervisor command.

The command's outcome is `SupervisorExit::ContinueBoot(BootSession)`, where
`BootSession` is the `lib/abi` enum `Unset` / `Text` / `Graphical`.

### G1.2 Carrying the choice into userland

The supervisor runs in-kernel, before any volume is mounted, so the choice
cannot be written to the configuration store (it is not persistent policy in
any case). The kernel records it once and serves it through a new ungated
syscall, the `boot_facts_get` shape (`AGENTS.md` §16.6 — boot-static, public,
never live state):

- `tairix_abi::BootSession` — `Unset = 0`, `Text = 1`, `Graphical = 2`. An
  unrecognised wire value fails closed to `Unset`.
- `boot_session_get()` — no arguments, returns the recorded `BootSession`
  discriminant in the return register. No capability: the operator's boot
  choice is public machine state, carries no authority, and reveals no
  secret. Not audited (a read of a boot-static public value is not a
  security decision).
- The record is installed **once**, by the root-unlock boot path, from the
  supervisor's exit. A kernel that never entered the supervisor reports
  `Unset`. A second install is refused, so a later userland process cannot
  rewrite the operator's boot choice.
- Wrappers: `tairix_rt::boot_session()`, `tairix_sys_boot_session_get`.

### G1.3 Precedence in `login`

`login` re-reads both inputs each round (as it already does for
`os.loginType`), so the rule is one function with no cached state:

```
effective = match boot_session_get() {
    Text       => Text,
    Graphical  => Graphical,
    Unset      => configured os.loginType,
}
```

and the result is degraded to `Text` when a graphical login is not available
this round.

## G2. `lib/greeter` — the authentication-surface engine

**Status: done for the single-account surface. The chooser, the wallpaper
backdrop and the session actions land with G3, which is what gives each of
them a consumer.**

One `lib/*` crate owns *everything* about what a screen-authentication
surface is and does; an embedder owns only the syscalls, the window, and the
way it actually verifies. `no_std + alloc`, host-tested, no dependency on
`lib/abi`, IPC, a compositor, or a seat.

It is a `lib/*` crate rather than private to one program because there are
two charter-legal consumers (§2.2): the desktop's screen lock (G6, live
now) and the greeter service (G3). There MUST NOT be two implementations of
"prove who you are, at the screen".

### G2.1 As built

```rust
pub struct AuthSurface;      // new(account), on_event, render, notice, field_rect
pub struct Outcome;          // redraw(), verified()
pub struct EventContext<'a>; // screen, scale, theme, verifier
pub trait  Verifier { fn verify(&mut self, secret: &str) -> Verdict; }
pub enum   Verdict { Verified, Refused, Unreachable }
pub enum   Backdrop { Desktop }
pub fn panel_rect(screen: Rect, scale: Scale) -> Rect;
pub const MAX_PASSWORD: usize = 256;
pub const UNNAMED_ACCOUNT: &str = "Locked";
```

- **The secret** is a `lib/controls` `TextField::secret`, so masking, the
  fixed-width bead rendering that leaks no length, and the volatile wipe on
  discard are the one shared implementation. The surface erases the field on
  every terminal transition — verified, refused, unreachable, and drop.
  `MAX_PASSWORD` is a fail-closed memory bound, not a password policy: the
  buffer is reserved once and never grown, so no copy of a secret is left in
  a freed block.
- **The verdict seam** keeps authentication with the embedder. Three
  answers, never two: `Unreachable` (nothing listening, a transport fault, a
  reply that is not the protocol) is never mistaken for `Refused` and never
  for a pass. Only `Verified` concludes the surface.
- **The geometry** is one definition. `panel_rect` (centred, a third down,
  clamped to a small screen) and `field_rect` are read by both the paint and
  the pointer hit test, so they cannot drift apart; a test asserts every
  pixel a keystroke changes falls inside the hit-tested field.
- **Modal and total.** No key, click, or state dismisses the surface
  without a `Verified` verdict — no guest path, no timeout, no error state
  that falls through. A zero-extent screen yields no frame at all rather
  than a "lock" that covers nothing.
- **No rate limit here.** The budget belongs to the authority that can see
  attempts from every surface at once; a second one in the client would be
  both duplicated and trivially bypassed.

### G2.2 What G3 adds

Each of these arrives with its consumer, not before:

- **The chooser.** Account tiles — monogram or avatar, display name, and a
  badge on any account that already has a live session (the switch-user
  affordance) — plus an `Other…` tile, always last, that accepts a typed
  login name so an unlisted account stays reachable. `Tab`/arrows move
  focus, `Return` submits, `Escape` returns to the chooser; the surface is
  fully operable with no pointer, because a machine without one must still
  log in.
  An `AccountTile` carries only what is drawn — display name, login name,
  avatar id, live flag — never credential material, a capability set, or a
  home path.
- **The wallpaper backdrop.** A second `Backdrop` case carrying an
  already-decoded, already-fitted image plus its contrast scrim. The system
  default for the active theme, from the shipped masters under
  `/System/Graphics/Wallpapers`, through the same catalog, the same
  `lib/wallpaper` fit geometry and the same sandboxed decode the desktop
  pinboard uses (`plans/PINBOARD.md`) — the engine gains no decoder. The
  scrim is computed **once** per (wallpaper, screen size, theme), cached
  under the memory-pressure model, and re-derived only when the scale, theme
  or wallpaper changes — never per frame (§10, §2.16).
- **A per-account attempt budget** displayed as a cooldown. Per account, so
  a wrong password for one cannot lock another out, and monotonic-clock
  driven with `Duration64` (§21) — never wall clock, which the user may be
  able to move. It presents the authority's budget; it does not invent one.
- **Chrome and damage.** The clock, the date, the host name, and the
  session actions the authority offers; and the changed-rectangle report
  that lets the service present a damage rect rather than a full-screen
  blit (§2.16). A frame is produced only in response to an event or an
  animation step; an idle login screen presents nothing and the process
  parks (§2.23).

### G2.3 Failure is visible, never fatal

A missing or undecodable wallpaper degrades to the theme's flat desktop
colour. An unreachable font service degrades to the compiled-in console
atlas (`lib/font`). An empty account list still shows `Other…`. None of
these ends the greeter or blocks a login (§2.24).

## G3. `greeter.app` — the service

**Status: planned.**

`userland/session/greeter`, planted at `/System/Services/greeter.app`,
`kind = "service"`. It runs as its own **`greeter` service account** (a
dedicated uid from the service range, `plans/USERS.md`) — never as the
system user, and never as the account being logged in.

Manifest request (the smallest set that draws and reads one seat):

| Capability | Why |
| --- | --- |
| `CAP_DISPLAY` | acquire the seat lease while the login screen is up |
| `CAP_INPUT_READ` | drain the owned seat's keyboard and pointer |
| `CAP_SHM` | the zero-copy frame region the display service maps |
| `CAP_FS_ACCESS` | read the wallpaper master and the theme/avatar assets |
| `CAP_CONSOLE_WRITE` | fail-loud termination reasons on stderr (§2.24) |
| `CAP_LOG_EMIT` | its own audit records |

It requests **no** `CAP_USERS_READ`, **no** `CAP_SPAWN_AS_USER`, **no**
`CAP_PROC_SPAWN` and **no** `CAP_IPC_BIND_PRIVILEGED`. It cannot read a
credential store, cannot start a process, and cannot bind a reserved
rendezvous. Compromising it yields a screen, not an account.

Lifecycle:

1. Started by the authority when a graphical login is wanted.
2. Acquires `SEAT_PRIMARY`, configures the display service, drains input.
3. Calls the authority's `session-v1` endpoint for the account list.
4. On submit, calls `Authenticate`; the reply is `Accepted` or `Refused`.
5. On `Accepted` it releases the seat and **exits**. The kernel reclaims
   the lease on exit in any case (`plans/DISPLAY.md` D8), so the seat can
   never be left stranded by a greeter that dies at the wrong moment.
6. The authority then brings the target session to the foreground and,
   when that session ends, starts a fresh greeter.

Exiting rather than lingering is deliberate: the seat hand-off then has one
mechanism (the kernel's own reclaim-on-exit), no lease transfer is needed,
and a greeter cannot hold the screen behind a running desktop.

## G4. The session authority

**Status: planned.**

`login` keeps its present role and gains the graphical path. It binds one
new reserved endpoint and serves it for the machine's lifetime.

### G4.1 `session-v1` (`lib/abi/src/session_ipc.rs`)

Reserved `SESSION_ENDPOINT`, bound by the authority (which holds
`CAP_IPC_BIND_PRIVILEGED`). Requests:

| Request | Reply | Notes |
| --- | --- | --- |
| `Accounts` | `AccountList` | display name, login name, avatar id, `live` flag — never a hash, uid ceiling, or home path |
| `Authenticate { username, password }` | `Accepted { .. }` / `Refused(Errno)` | the whole point; the secret never leaves this call |
| `Sessions` | `SessionList` | which accounts have a live session (for the switch affordance) |

Every request is refused unless the caller's **kernel-attested** uid is the
`greeter` service account and its attested console matches the authority's
own (the `handle_elevate_request` placement check, reused — one
implementation). A refusal is indistinguishable between "unknown account"
and "wrong password", and the verify path is timing-equalised in
`lib/users`, exactly as the text prompt and the elevation broker already
are. Every decision is audited with a stable event id (§19.4).

The request buffer holds a secret, so it is wiped on every path out of the
handler, and the secret is never logged, never placed in an `stdinfo`
record, and never carried in a reply.

`Authenticate` does **not** itself start anything: it returns a verdict. The
authority starts the session on its own loop, so the greeter can never
choose *which* program runs as the authenticated user.

### G4.2 The round

The authority's per-console loop becomes:

```
loop {
    reload the user database                     (existing)
    kind = effective login type                  (G1.3)
    if kind == Graphical && graphical available:
        run one graphical round
    else:
        run one text round                       (existing)
}
```

A graphical round: start the greeter → serve `session-v1` until it reports
`Accepted` and exits → bring the target session to the foreground → wait →
loop. A greeter that dies without accepting is restarted; three consecutive
failures degrade the round to the text login with the reason on stderr and
in the log (§2.24), so a broken greeter can never leave the machine
unusable.

### G4.3 Availability

A graphical login is available this round when the greeter bundle is
installed **and** the desktop bundle is installed **and** a display service
answers `DISPLAY_ENDPOINT` — the existing probe, extended by one path. All
three are re-checked per round and every failure degrades to text.

## G5. Fast user switching

**Status: planned.**

The authority keeps the **session table**: one entry per account with a live
desktop session, holding the account's uid, the session process id, its
state (`Foreground` / `Background`), and its wake port. At most one entry is
`Foreground`.

### G5.1 Switching away

`Switch User` in the desktop's session menu, and `Lock`+`Switch` from the
lock surface, both send the session's own request to the authority:

1. The desktop session releases the seat and marks itself background.
2. It keeps running: its apps keep running, its window server keeps
   answering, it simply presents nothing.
3. The authority starts a greeter, which acquires the now-free seat.

**Logging out** is different and unchanged in meaning: the session exits,
its entry leaves the table, and the greeter comes up.

### G5.2 Switching back

When the greeter reports `Accepted` for an account that already has a live
session, the authority does **not** start a second one. It:

1. waits for the greeter to exit (which frees the seat);
2. wakes the background session through its **wake port** — a named port the
   session binds at start-up and holds as a member of its existing wait-set,
   so it is woken by an event and never polls (§2.23);
3. the session re-acquires `SEAT_PRIMARY`, re-configures the display
   service (the mode may have changed), repaints in full, and resumes.

The wake message carries no authority: it says "you are foreground", and the
kernel's own seat exclusivity is what actually decides who may present. A
session that finds the seat busy fails closed and reports rather than
spinning.

### G5.3 Guarantees

- A background session's memory is **not** exposed to the foreground user:
  isolation is the kernel's page tables, unchanged (§4).
- A background session's screen contents are never composited while another
  user holds the seat, and the seat's undrained keystrokes are purged on
  every lease change (the kernel already does this, `plans/DISPLAY.md` D8),
  so a switch cannot leak a partially typed secret into the next owner.
- The account that owns a background session must re-authenticate to return
  to it. There is no path from the greeter to a live session without a
  successful `Authenticate` for that account.
- Shutdown drains the table: every live session is asked to end, in reverse
  order of creation, before the machine goes down.

## G6. One surface, two uses

**Status: done.**

The desktop's `ScreenLock` (`userland/gui/session/src/lock.rs`) is the same
surface as the greeter with the account fixed to the session's own and the
chooser and session actions suppressed. It composes `lib/greeter` and keeps
only its embedder duties: the compositor window, `keep_topmost`,
`LockedDrain`, the window-space mapping, and a `Verifier` over the
per-console elevation broker — which verifies against the caller's
kernel-attested uid, never a supplied name. No panel, field, layout, or
wording remains duplicated in the session crate.

When G3 lands, the greeter service becomes the second embedder of the same
engine — a different `Verifier` (the `session-v1` broker) and a different
`Backdrop`, not a second surface.

## Security review notes

- **The greeter is untrusted by the authority.** Every field of every
  request is bounds- and shape-checked; the caller's identity is the
  kernel-attested origin, never a claim in the message; the reply set
  cannot be widened by the caller.
- **No ambient authority.** The greeter cannot start a process. The
  authority chooses the program (§16.5's desktop bundle path, one spelling)
  from its own constant, never from the request.
- **Secrets.** The secret exists in exactly two buffers — the greeter's
  field and the authority's request buffer — both volatile-wiped on every
  exit path. It never reaches swap unencrypted (§4), a log, or `stdinfo`.
- **Enumeration.** The account list is disclosed only to the attested
  greeter account. A machine that would rather not disclose it uses the
  `Other…` tile only; that is a store setting, not a second code path.
- **Denial of service.** A greeter that cannot start, cannot acquire the
  seat, or repeatedly dies degrades the round to the text login. The
  machine is always loggable-into.

## Testing

Covered now, in host unit tests:

- the `continue` operand grammar, including both rejection shapes (an
  unknown word and more than one operand) and the REPL staying open;
- the `BootSession` wire round-trip and its fail-closed decode, the
  `lib/rt` wrapper's fail-closed mapping, the `abi-sys` stub marshalling;
- the set-once kernel cell (empty reads `Unset`, first install wins, a
  second does not overwrite) and the kernel install path from a supervisor
  `continue gui`;
- the `effective_session_kind` precedence over every
  (override × stored default) pair;
- the surface state machine: editing, submit-once, the secret erased after
  a verified, a refused *and* an unreachable verdict, refusal and
  unreachable reading differently, and no event concluding the surface
  without a verified verdict;
- the surface render: the panel centred, clamped on a small screen, scaled
  from 25 % to 800 %, `None` rather than a panic on a zero-extent screen,
  and every pixel a keystroke paints falling inside the hit-tested field.

Owed by the remaining deliverables:

- the chooser: focus movement, the `Other…` tile, the per-account attempt
  budget and cooldown, the degraded paths (no wallpaper, no font service,
  no accounts);
- `session-v1` encode/decode round-trips plus a fuzz harness (§19.6), and
  the authority's refusal of a caller whose attested uid is not the greeter;
- the session table: switch-away, switch-back, logout, a session that dies
  while background, two accounts alternating;
- QEMU verticals: a graphical boot reaching the greeter, an authentication
  that lands on the desktop, a logout returning to the greeter, and a
  switch between two accounts.

## Deliverables

| # | Deliverable | Status |
| --- | --- | --- |
| G1 | Boot session decision: `continue text\|gui`, `BootSession`, `boot_session_get`, login precedence | done |
| G2 | `lib/greeter` — the authentication-surface engine | done for the single-account surface; chooser, wallpaper backdrop and session actions land with G3 |
| G3 | `greeter.app` service bundle and its service account | planned (with G4) |
| G4 | `session-v1` broker in `login`; the graphical round; availability | planned (with G3) |
| G5 | Fast user switching: session table, wake port, switch away/back | planned |
| G6 | `ScreenLock` composes `lib/greeter` | done |
| G7 | Docs (`docs/src/desktop/greeter.md`), README matrix, QEMU verticals | planned |
