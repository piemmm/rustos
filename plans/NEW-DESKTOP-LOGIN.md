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

**In progress.** G1–G6 are done and host-tested: the boot session decision,
the `lib/greeter` surface engine, the `greeter.app` service, the
`session-v1` broker and graphical round in `login`, fast user switching on
both sides, and the desktop's screen lock composing the same engine. A
graphical login is now the **default** on hardware that can run one (G1),
degrading to the text prompt otherwise. The docs and README matrix are
current.

**Remaining: the G7 QEMU verticals only.** No integration test yet boots a
machine to the graphical login screen, authenticates, and switches accounts
— see G7 for what that needs and why it is staged separately. Everything
below is proven on the host and by construction; nothing yet proves it *on a
screen*, which is exactly the gap G7.1 closes and why the README marks the
feature partial. Four defects a first real boot exposed — no wallpaper, no
text, no pointer, and a text-by-default login — are fixed above (G0, G2.1,
G2.2, G3, G1); their common lesson is recorded in G7.1.

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

The decode itself happens in a capability-empty worker, never in the address
space that owns the seat, and the greeter reaches that worker through
`CAP_SANDBOX_SPAWN` — an authority that admits *only* a canonical parser
sandbox (a child the kernel brands capability-empty, with no credential
switch and no console inherit). It is deliberately not `CAP_PROC_SPAWN`:
isolating untrusted input must not cost the authority to start a general
process, or the greeter's "it cannot start the session it authenticates for"
boundary would be a fiction. `spawn`'s gate therefore lives in the handler,
which alone decodes the attach block — the coarse "holds one of the two"
refusal first, then sandbox ⇒ either capability, anything else ⇒
`CAP_PROC_SPAWN`.

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
   `lib/sysconfig` store (`graphical` or `text`).
3. **The compiled default**, `graphical`.

A machine that can run a graphical login gets one without being configured
for it: the default is `graphical`, and there is exactly one definition of
it — `SystemConfig::default().login_type`. A *reachable* store that teaches
nothing (no document, an unreadable one, non-UTF-8 bytes, a parse error)
resolves through that same definition rather than spelling a fallback of
its own, so the default cannot be flipped in one place and silently ignored
in another.

**An unreachable store is a different fact and MUST NOT be collapsed into
the absent one.** A round that runs before the root unlock mounts
`/System/Settings` can read nothing at all, and assuming the compiled
default there would silently boot graphical over an administrator who had
configured the text prompt — a defect that stayed invisible only while the
compiled default happened to equal what losing the race produced. Login
therefore probes the store's own directory first: unreachable ⇒ this round
runs the text prompt, which is always available and contradicts no stored
choice, and the next round re-reads once the volume is up. An unreachable
store withholds a *default*, never the operator's one-boot choice, which
still wins on the very first round.

A graphical choice is *offered* only when a graphical login is actually
possible this round (G4's availability probe). When it is not, the boot
degrades to the text login — never an error, never a blank screen (§2.24,
§5.4). That degradation is what makes a graphical default safe on a headless
or driverless machine, so it is load-bearing and its tests are not to be
relaxed.

A test image that needs the text prompt on hardware that *could* draw asks
for it outright — the autoload QEMU disk plants an `os.loginType text`
document rendered by the configuration engine itself — rather than leaning
on whatever the default happens to be.

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

**Status: done.**

One `lib/*` crate owns *everything* about what a screen-authentication
surface is and does; an embedder owns only the syscalls, the window, and the
way it actually verifies. `no_std + alloc`, host-tested, no dependency on
`lib/abi`, IPC, a compositor, or a seat.

It is a `lib/*` crate rather than private to one program because there are
two charter-legal consumers: the desktop's screen lock (G6) and the greeter
service (G3). There MUST NOT be two implementations of "prove who you are,
at the screen".

### G2.1 As built

```rust
pub struct AuthSurface;      // new(account) | with_accounts(tiles); on_event, render,
                             //   notice, field_rect, selected_account,
                             //   set_chrome, set_cooldown
pub struct AccountTile;      // new(display, login), with_live_session, monogram
pub struct Chrome;           // clock, date, host
pub struct Outcome;          // redraw(), verified(), damage()
pub struct EventContext<'a>; // screen, scale, theme, verifier
pub trait  Verifier { fn verify(&mut self, account: &str, secret: &str) -> Verdict; }
pub enum   Verdict { Verified, Refused, Unreachable }
pub enum   Backdrop<'a> { Desktop, Wallpaper { image: &'a Surface, scrim: u8 } }
pub fn panel_rect(screen: Rect, scale: Scale) -> Rect;
pub fn scrim_alpha(image: &Surface, panel: Rect, theme: &Theme) -> u8;
pub const MAX_PASSWORD: usize = 256;
pub const MAX_LOGIN_NAME: usize = 64;
pub const MAX_CHROME: usize = 64;
pub const UNNAMED_ACCOUNT: &str = "Locked";
```

`new(account)` is the screen lock's constructor — one account, straight to
the field, no chooser, and `Escape` does nothing. `with_accounts` is the
login screen's — it opens on the chooser.

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
- **The geometry** is one centred column, defined once in `layout.rs` and
  read by both the paint and the pointer hit test, so the two cannot drift
  apart; a test asserts every pixel a keystroke changes falls inside the
  hit-tested field, and another presses the centre of every tile the grid
  drew and gets exactly that account.

  | Band | Logical size | Where |
  | --- | --- | --- |
  | chrome | full width × 106 | 40 from the top; clock 64 (`Display`), date 24 (`Body`), host 18 (`Caption`) |
  | disc | 88 × 88 | body top, centred |
  | name | full width × 26 | 14 under the disc (`Heading`) |
  | block (`panel_rect`) | 420 × 96 | 18 under the name |
  | field (`field_rect`) | 320 × `control_height` | block top; a pill, edged `rim_active` or `danger` |
  | notice | block width × 20 | 10 under the field |
  | step-back | block width × 18 | 6 under the notice, only when a chooser exists |
  | chooser grid | tiles 132 × 154, gap 12 | as many columns as fit; wraps only when it must |

  Every one of those is a *logical* length converted through `Scale` exactly
  once, so the screen is correct at any density and a second conversion
  cannot creep in. The body is centred in the space beneath the chrome and
  anchored at that space's top when it is taller, so it can never ride up
  over the clock.

  **The chrome's presence is a function of the screen and the density
  alone** (`chrome_band`), never of which body is up: a screen that shows a
  clock while choosing an account still shows it while typing, so the
  surface cannot appear to gain or lose its chrome mid-login. It stands down
  only when the screen cannot hold both it and a whole prompt, and a 640×480
  screen still gets a usable, painted one.

  The secret field is drawn as a **pill** by masking the shared `TextField`
  to a stadium and laying it over an edge of the same shape — the control's
  own square rim and focus ring are *removed*, not covered, so there is no
  second field implementation and no double border. The submit mark is drawn
  at rest only, because the field owns its own text region and an always-on
  trailing mark would sit under a long secret's beads.
- **Modal and total.** No key, click, or state dismisses the surface
  without a `Verified` verdict — no guest path, no timeout, no error state
  that falls through. A zero-extent screen yields no frame at all rather
  than a "lock" that covers nothing.
- **No rate limit here.** The budget belongs to the authority that can see
  attempts from every surface at once; a second one in the client would be
  both duplicated and trivially bypassed.
- **The chooser.** Account tiles — a monogram disc, the display name, and a
  badge on any account that already has a live session (the switch-user
  affordance) — plus an `Other…` tile, always last and always present, that
  accepts a typed login name so an unlisted account stays reachable.
  `Tab`/`Shift-Tab` and the arrow keys move focus with wrap-around, `Return`
  activates, `Escape` returns to the chooser and wipes the typed secret; the
  surface is fully operable with no pointer, because a machine without one
  must still log in. Tiles are `lib/controls` `IconTile`s, so the login
  screen is not a second visual vocabulary: the chosen account takes the
  shared soft accent halo with its name on an accent pill, and a long
  display name **wraps** rather than being cut — `System Administrator` reads
  as itself, not as `System Admini`. The tile's height is derived from the
  control's own `label_lines`, not guessed: 154 logical pixels is the first
  height that holds three whole label lines at the reference density *and* at
  a doubled one, so a face wider than the test face still has somewhere for a
  long single word to fall.
  An `AccountTile` carries only what is drawn — display name, login name,
  live flag — never credential material, a uid, a capability set, or a home
  path. A tile draws a **monogram**, not an avatar: the system has no
  per-user avatar store, and an identifier nothing can resolve would be
  speculative surface.
- **The wallpaper backdrop.** A second `Backdrop` case carrying an
  already-decoded, already-fitted image plus the alpha of its contrast
  scrim — the engine gains no decoder; decoding untrusted bytes is the
  embedder's sandboxed business. `scrim_alpha` derives that alpha from the
  image's **brightest** patch under the panel, not its mean, because text
  sits over the worst pixel; it samples on a bounded grid, so a 4K master
  costs the same as a thumbnail, and it is bounded well short of both
  extremes so the panel is never bare and the wallpaper never blacked out.
  Pure, so the embedder computes it **once** per (wallpaper, screen size,
  theme) and re-derives it only when the scale, theme or wallpaper changes —
  never per frame. Over the picture the surface also lays a two-ended
  vertical wash in the theme's own desktop colour, so the chrome at the top
  and the field in the middle stay legible over a bright photograph; because
  the wash *is* the desktop colour it composites to nothing over a plain
  backdrop. There is no blur — that lives in the compositor, which this crate
  may not reach — so the scrim and the wash do the work honestly.
- **A per-account attempt budget** displayed as a cooldown. Per account, so
  a wrong password for one cannot lock another out, and monotonic-clock
  driven with `Duration64` — never wall clock, which the user may be able to
  move. It presents the authority's budget; it does not invent one, and it
  reads no clock: `set_cooldown` takes the remaining time from the
  authority's own answer. A submit during a cooldown re-states the wait,
  erases the typed secret, and never reaches the verifier.
- **Chrome and damage.** `set_chrome` supplies the clock, the date and the
  host name drawn on the backdrop — bounded display text, any of which may
  be empty rather than guessed. Every `Outcome` reports the rectangle the
  next paint changes (the field for a keystroke, the panel for a verdict,
  the grid for a focus move, the chrome band for a clock tick, the whole
  screen for a mode change), so the service presents a damage rect rather
  than a full-screen blit. Tests assert pixel-by-pixel that the reported
  rectangle is a superset of what actually changes. A frame is produced only
  in response to an event; an idle login screen presents nothing and the
  process parks.

### G2.2 Failure is visible, never fatal

A missing or undecodable wallpaper degrades to the theme's flat desktop
colour. An unreachable font service degrades to the compiled-in console
atlas (`lib/font`). An empty account list still shows `Other…`. A cursor
that will not rasterise leaves a working screen with no pointer drawn. None
of these ends the greeter or blocks a login (§2.24).

But a degradation is only honest when the good path is actually reachable.
The freestanding `Run` build MUST enable every crate feature its render and
IPC paths need at run time — `tairix-font`'s `rt` above all, without which
there is no glyph transport installed at all and *every* string silently
draws nothing. A program that links a client crate with its transport
feature off has not degraded gracefully; it has failed silently, which is
the opposite. The greeter's `Cargo.toml` carries a per-crate justification
for each feature it enables, and its README tabulates them, so the next
reader can tell a deliberate omission from a forgotten one.

## G3. `greeter.app` — the service

**Status: done** (`userland/session/greeter`).

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
| `CAP_FS_ACCESS` | read the wallpaper master and the theme assets |
| `CAP_SANDBOX_SPAWN` | decode those untrusted bytes in a capability-empty worker (G0) |
| `CAP_CONSOLE_WRITE` | fail-loud termination reasons on stderr (§2.24) |
| `CAP_LOG_EMIT` | its own audit records |

It requests **no** `CAP_USERS_READ`, **no** `CAP_SPAWN_AS_USER`, **no**
`CAP_PROC_SPAWN` and **no** `CAP_IPC_BIND_PRIVILEGED`. It cannot read a
credential store, cannot start the session it authenticates for, and cannot
bind a reserved rendezvous. The one child it may create is the canonical
parser sandbox, which the kernel brands capability-empty. Compromising it
yields a screen, not an account.

The same seven are the `greeter` account's ceiling (`GREETER_CEILING`), so
the manifest ∩ ceiling intersection loses nothing and neither list can drift
from the other unnoticed — both are pinned by tests, together with the four
capabilities that must stay off it.

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

**The pointer is drawn by the service, not the engine.** A seat with a mouse
must show one — an invisible-but-working pointer is a defect, not a
minimalism — so the greeter rasterises the built-in arrow once at start-up
for the active `Scale`. The placement (origin = pointer − hotspot, and
sampling the artwork under a screen row) is `lib/cursor`'s `PlacedCursor`,
shared with the compositor: there is one definition of where a cursor sits,
and it lives in `lib/*` precisely because `userland/session/*` may not reach
into the window manager for it. Motion presents the **union of the cursor's
old and new rectangles** clipped to the screen — never the whole screen for
a mouse move, and never a stale pointer left behind — and motion that moves
nothing presents nothing, so an untouched screen still arms no timer.

**Moving the mouse costs no render and no round trip.** Pointer motion
streams: a hand movement is tens of reports a second, and the screen has to
stay ahead of it.

- The service keeps the **clean** rendered surface — the one
  `AuthSurface::render` produced, with no cursor in it — and re-renders only
  when the surface's own state changed. The set of things a render reads is
  closed (the surface's state, the screen, the scale, the backdrop) and each
  of them changes only through a call that returns an `Outcome` or installs a
  wallpaper, which is why the cache provably cannot go stale. A `Repaint` is
  therefore three cases, not two: nothing, cursor-only, or painted — and a
  cursor-only round keeps the surface.
- The cursor is composited **at scan-out**, sampled over the cached surface
  while the damaged rows are copied, so it is always on top and never dirties
  the thing being reused.
- A drain applies every queued report and presents **once**, merging the
  damage (`Nothing` is the identity, whole-screen dominates, two regions
  become the region containing both, re-classified through the shared
  `sub_screen_damage` so a union that has grown to cover the screen is
  presented as the whole screen rather than an over-large region).

A bare move is then a hit test, two rectangle unions and a copy of the
cursor-sized union — no allocation, no glyph, no wallpaper blit, and one
display call per burst instead of one per report.

## G4. The session authority

**Status: done** (`userland/session/login`).

`login` keeps its present role and gains the graphical path. It binds one
new reserved endpoint and serves it for the machine's lifetime.

### G4.1 `session-v1` (`lib/abi/src/session_ipc.rs`)

Reserved `SESSION_ENDPOINT`, bound by the authority (which holds
`CAP_IPC_BIND_PRIVILEGED`). Three requests:

| Request | Sent by | Reply | Notes |
| --- | --- | --- | --- |
| `Accounts { offset }` | the greeter | `AccountPage` | display name, login name, `live` flag — never a hash, uid, ceiling, or home path |
| `Authenticate { username, password }` | the greeter | `SessionVerdict` | the whole point; the secret never leaves this call |
| `Background` | the foreground desktop session | `SessionVerdict` | step aside so the login screen comes back up (G5.1) |

**One question, one answer.** "Which accounts exist" and "which have a live
session" are the same question about the same records, so they are one
request with a `live` flag per record — two requests returning the same fact
would be duplication and could disagree.

**The list is paged**, not sent whole: a machine may have far more accounts
than one reply could hold, and a fixed ceiling a larger machine outgrows is
a defect. A page carries the whole list's `total`, so a client walks pages
until it has them all.

**A tile draws a monogram, not an avatar.** There is no per-user avatar
store on the system, so an avatar id would name nothing.

The two greeter requests are refused unless the caller's **kernel-attested**
uid is the `greeter` service account and its attested console matches the
authority's own — the `handle_elevate_request` placement check. `Background`
carries its own, different rule (G5.1). Every refusal is one answer: unknown
account, wrong password, locked account, no database, and an unattested
caller are byte-identical, so a reply can never be used to probe for
accounts; the reason lives in the audit trail. A refusal carries only
`retry_after`, the remaining per-account cooldown, because a screen that
could not say "wait 30 seconds" would leave the user pressing a key that
silently does nothing. The verify path is timing-equalised in `lib/users`,
exactly as the text prompt and the elevation broker already are, and every
decision is audited with a stable event id.

An unauthorised or undecodable request is answered with a well-formed
**empty page**, never an errno — so a client bug reaches the greeter as a
protocol fault (which it is) rather than being shown to the user as a wrong
password.

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

**Status: done** (`userland/session/login` + `userland/gui/session`).

The authority keeps the **session table**: one entry per account with a live
desktop session, holding the account's uid, the session process id, and its
state (`Foreground` / `Background`). At most one entry is `Foreground`, and
the invariant is asserted after every mutation. The wake mailbox is
*derived* from the process id (`session_wake_endpoint`), never stored, so
the two cannot disagree.

### G5.1 Switching away

`Switch User…` in the desktop's session menu sends `SessionRequest::Background`:

1. The authority honours it **only** from the session it records as the
   foreground one, identified by the caller's kernel-attested uid on its own
   console. The greeter's uid, a background session, and any other local
   process are all refused — nothing else can take the screen away from the
   person using it.
2. Only on `Accepted` does the session tear down presentation and release
   the seat. Releasing first would black the screen with nobody drawing.
3. It keeps running: its apps keep running, its window server keeps
   answering, it simply presents nothing and parks on its wake mailbox with
   no timeout.
4. The authority's round returns to the login screen, leaving the entry in
   the table.

**Logging out** is different and unchanged in meaning: the session exits,
its entry leaves the table, and the greeter comes up. The round tells the
two apart explicitly — a child that exited is removed and audited as ended;
a session that backgrounded itself is kept and stays resumable.

**If the authority itself exits** (a dead console; PID 1 relaunches it) it
first drains the table newest-first and sends each entry `SessionWake::End`.
A relaunched authority starts with an empty table, so a background session
it did not end would be unreachable forever — holding memory, owning no
seat, and with nothing left that could wake it.

### G5.2 Switching back

When the greeter reports `Accepted` for an account that already has a live
session, the authority does **not** start a second one. It:

1. waits for the greeter to exit (which frees the seat);
2. wakes the background session through its **wake mailbox** —
   `session_wake_endpoint(pid)`: the session's own never-reused task id under
   a fixed high tag, so every session binds a distinct, collision-free,
   unreserved id and no new kernel namespace is invented. The session holds
   it as a member of its existing wait-set, so it is woken by an event and
   never polls;
3. the session re-acquires `SEAT_PRIMARY`, re-configures the display
   service (the mode may have changed), repaints in full, and resumes.

A wake that cannot be delivered means the session may already be gone, so
the authority reaps it non-blockingly to find out: a reaped session leaves
the table and a fresh desktop starts, while one still running keeps its
entry and the round returns to the login screen rather than starting a
second desktop for one account.

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
- The authority never leaves a session it can no longer reach: if it exits,
  it drains the table newest-first and ends every entry first.

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

The greeter service is the second embedder of the same engine — a different
`Verifier` (the `session-v1` client) and a different `Backdrop`, not a
second surface.

## G7. Documentation and verticals

**Docs: done.** `docs/src/userland/login.md` (the session authority, the
`session-v1` surface and its gates, the attempt budget, the session table),
`docs/src/userland/greeter.md` (the service), `docs/src/lib/greeter.md` (the
surface engine), and the `README.md` feature and attack-vector matrices.

### G7.1 QEMU verticals — the one thing still outstanding

Every layer of this work is proven on the host, including the two halves of
`session-v1` against each other. What is **not** proven is that a real
machine boots to the login screen and logs in: no integration test drives a
graphical userland under QEMU at all today.

**This gap has already cost real defects, and that is the argument for
closing it.** The first genuine boot to this screen showed no wallpaper, no
text at all, two unlabelled icons, and no pointer — while every crate
involved was green. None of those was a logic error a unit test could have
caught: the wallpaper decode was refused by a capability gate no host test
exercises, the glyph transport was never linked into the freestanding binary
at all, and the cursor was hit-tested but never drawn. A host test renders
into a `Surface` with a test transport already installed; it cannot see a
program that was built without one. Only a vertical that looks at the actual
framebuffer of an actual boot can. Until one exists, "host-green" must not
be reported as "works".

The verticals owed are

1. a graphical boot reaching the greeter and presenting a first frame;
2. an authentication landing on the desktop;
3. a logout returning to the greeter;
4. a switch between two accounts and back.

The first of those MUST assert on *content*, not merely that a frame
arrived: readable text present, the wallpaper drawn rather than a flat
colour, and a pointer visible — the three things a green host suite let
through.

They are staged separately because they need infrastructure that does not
yet exist rather than more of this feature: a guest with a display device
and injected pointer/keyboard input, the whole `devmgr` / display-service /
`login` / `greeter` / desktop service graph brought up inside it, and a way
to assert on what reached the framebuffer. That harness is a deliverable in
its own right and will serve every graphical vertical the desktop needs,
not only this one; building it inside this change would have made it a
graphical-test-harness change with a login screen attached.

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

Covered in host unit tests:

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
- the surface render: the column centred and non-overlapping at 100 % and
  200 %, every band actually painting inside the rectangle the layout claims
  for it, the chrome identical whichever body is up, a usable prompt on a
  640×480 screen, the block never exceeding the screen, scaling from 25 % to
  800 %, `None` rather than a panic on a zero-extent screen, and every pixel
  a keystroke paints falling inside the hit-tested field;
- legibility on both themes: the clock, name, notice and field reach at
  least half the separation the theme itself promises between that ink and
  the desktop colour — a bar derived from the palette, so it cannot be
  quietly lowered, and one that fails outright if no glyphs are drawn;
- the pointer: the placed origin is the pointer minus the hotspot, the drawn
  pixels fall only inside the cursor's rectangle, a move presents exactly
  the clipped union of the old and new rectangles and leaves nothing behind,
  a move that moves nothing presents nothing, and a cursor that will not
  rasterise leaves a working screen;
- the chooser: focus movement in both directions with wrap-around, the
  `Other…` tile with no accounts and with many, `Escape` returning to the
  chooser and wiping the secret, the live badge, and the paint rectangle
  equalling the hit-test rectangle;
- the cooldown: a submit refused without the verifier being called, the
  secret still erased, clearing, and being dropped when stepping between
  accounts;
- `scrim_alpha`: different bounded alphas for a black and a white
  wallpaper, sizing on the brightest patch, and size-independent sampling;
- damage: every pixel a keystroke, a verdict, a focus move, and a clock
  tick change lying inside the reported rectangle;
- `session-v1` encode/decode round-trips and a fail-closed refusal for
  every malformation, plus an arm in the `lib/abi` wire-decoder fuzz
  harness;
- the broker: the placement and per-request identity gates, byte-identical
  refusals across every failure mode, the empty page for an unauthorised or
  undecodable request, paging past the end, the request buffer wiped, and
  no secret in any audit field;
- the attempt budget: free attempts, the doubling cap, per-account
  isolation, success reset, table-full eviction, and the all-cooling-down
  fallback;
- the session table: the single-foreground invariant after every mutation,
  switch-away, switch-back, logout, a session that dies while background,
  two accounts alternating, and the newest-first exit drain;
- the greeter service: bounded multi-page account loading (including a
  lying `total`), each verdict mapping, the request buffer wiped after
  every outcome, an unreachable authority keeping the surface alive, zero
  accounts still logging in by typed name, the park deadline being the
  nearer of the clock tick and the cooldown, and damage-only presentation;
- **the two halves against each other**: `tests/session_v1.rs` wires the
  greeter's transport straight to the authority's handler, so the client
  and server are proven to agree rather than each agreeing with a mock;
- the desktop side: the seat released only *after* an accepted
  step-aside, a refusal leaving the session drawing, a background park with
  no deadline, a foreground wake re-acquiring and re-moding a **changed**
  display mode, an `End` wake exiting cleanly, and an unattested or
  undecodable wake ignored.

## Deliverables

| # | Deliverable | Status |
| --- | --- | --- |
| G1 | Boot session decision: `continue text\|gui`, `BootSession`, `boot_session_get`, login precedence | done |
| G2 | `lib/greeter` — the authentication-surface engine | done |
| G3 | `greeter.app` service bundle and its service account | done |
| G4 | `session-v1` broker in `login`; the graphical round; availability | done |
| G5 | Fast user switching: session table, wake mailbox, switch away/back | done |
| G6 | `ScreenLock` composes `lib/greeter` | done |
| G7 | Docs and README matrix | done |
| G7.1 | QEMU verticals | planned |
