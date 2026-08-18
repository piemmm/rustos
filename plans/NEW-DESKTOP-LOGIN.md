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
degrading to the text prompt otherwise. The screen is animated throughout —
the chooser's selection cross-fade, the stage transition to a chosen
account's prompt, the shake on a refusal, and the veil that both uncovers the
screen on arrival and covers it again to hand the seat to a desktop revealing
from black over it — which the desktop reverses when it leaves (G2.1, G4.1).
The docs and
README matrix are current.

**Remaining: the G7 QEMU verticals only.** No integration test yet
authenticates at the graphical login screen or switches accounts — see G7
for what remains. A real boot now proves the machine reaches the
login screen unprompted (G7.1 vertical 1); what a screen does not yet prove
is the authenticate/logout/switch path, which is why the README marks the
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
compiled default happened to equal what losing the race produced. An
unreachable store therefore withholds a *default*: this round runs the text
prompt, which is always available and contradicts no stored choice, the next
round re-reads once the volume is up, and the operator's one-boot choice
still wins on the very first round.

**The two are told apart by the refusal, never by probing for a
directory.** One read of the document answers both questions, classified
once in `ConfigStore::from_read`. A mount whose backing volume is not
registered fails closed with `NotImplemented` and never falls back to
another volume, so that refusal — and only that one — means "ask again
later"; every other refusal came from a live volume that simply teaches
nothing. Inferring absence from the store's *directory* is forbidden and
was the original defect here: `configure` creates that directory on its
first write, so a machine nobody has configured has none, and the probe
read every fresh installation as an offline volume — pinning the
`Reachable(None) ⇒ graphical` rule above out of reach and silently
delivering a text prompt on every machine that had never been configured,
however capable of a graphical login it was.

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
  screen is not a second visual vocabulary: the chosen account **frosts the
  wallpaper behind it** — the shared region frost the compositor blurs a
  window's backdrop with, at the theme's `selection_backdrop_blur` — and the
  shared accent, at three tenths opacity, is laid over that with a **crisp**,
  rounded edge. The fill is that light because the frost is what marks the
  tile; the accent only tints it.
  The blur belongs behind the mark, never on it: softening the fill itself
  leaves a smear with no shape of its own. It is also **short**, which matters
  most here of anywhere: a box blur of radius `r` averages `2r + 1` samples, so
  a radius any appreciable fraction of a 132 × 154 tile averages the wallpaper
  behind it to a single colour, and the hovered account reads as an orange
  smudge rather than as glass laid over the picture. A selected tile draws no
  outline
  of any kind on top — neither the focus ring nor the pointer wash, both
  suppressed by the selection rather than by the mark's strength, so nothing
  flickers while the mark arrives. A long
  display name **wraps** rather than being cut — `System Administrator` reads
  as itself, not as `System Admini`. The mark **cross-fades** as focus moves —
  the tile being left decays while the tile arrived at grows, frost and colour
  together, so a backdrop never snaps into focus ahead of the accent leaving
  it. The tile's height is derived from the
  control's own `label_lines`, not guessed: 154 logical pixels is the first
  height that holds three whole label lines at the reference density *and* at
  a doubled one, so a face wider than the test face still has somewhere for a
  long single word to fall.
  An `AccountTile` carries only what is drawn — display name, login name,
  live flag — never credential material, a uid, a capability set, or a home
  path. A tile draws a **monogram**, not an avatar: the system has no
  per-user avatar store, and an identifier nothing can resolve would be
  speculative surface.
- **The screen's motion.** Four animations, every duration read from the
  theme's `MotionInteraction` table and every one of them driven by the shared
  `Timeline` — one definition of how a duration becomes frames, so no surface
  keeps its own clock arithmetic and no two animations can ease differently by
  accident. A *travelling* element takes the eased (smoothstep) progress; a
  pure strength fade takes the linear one, through the shared
  `tairix_theme::Fade` (a `Timeline` plus a `from`/`to` strength pair).
  - The chooser's selection **cross-fade** (`SelectionChange`), above.
  - The **stage transition** (`StageTransition`) between the chooser and the
    chosen account's prompt, in **both** directions: the picked account's
    monogram disc travels and scales from its tile to the prompt's disc, the
    other tiles fade out, and the prompt's name, field and notice fade in. It
    interpolates the *layout* — the disc's centre and radius, and a strength
    per element — and draws one pass into the surface it already owns; a
    cross-fade of two full renders would cost a second screen's worth of
    memory for a quarter of a second and buy nothing.
  - The **rejection shake** (`AttemptRejected`): a refused secret displaces the
    prompt horizontally on a decaying oscillation that ends at exactly zero,
    so the refusal is legible before the notice is read. It is *additional* to
    the notice and the cooldown, never a replacement for either.
  - The **veil** (`SessionFade`), which is one animation run in **both**
    directions rather than two: `AuthSurface::begin_entry_fade` opens it off
    full black as the screen arrives, and `begin_session_fade` closes it to
    opaque as the screen leaves. There is deliberately no second fade type.
    - *Arriving.* The service calls `begin_entry_fade` before its opening
      present, so the very first frame the display ever receives is full
      black and the chooser appears out of it. That is what makes the whole
      cycle symmetric (chooser in → login → chooser to black → desktop from
      black → log out → desktop to black → chooser in again), and at first
      boot it covers the kernel text console's pixels in one step instead of
      replacing them with a chooser. Only the *leaving* direction is modal:
      `session_fade_begun` is direction-aware, so neither input handling nor
      the pointer stops for an arriving screen and a user may pick an account
      and type while it is still appearing. The arriving veil is **dropped**
      the frame it uncovers, so no fully-transparent fill is blended
      thereafter.
    - *Leaving.* On a verified secret, before the process exits. The
      authority starts the session on that exit, so a screen that has already
      gone black is what lets the desktop reveal from black over it without a
      seam — and it is why the fade must complete first. A secret accepted
      mid-arrival leaves from the strength the veil had reached, so the
      screen never brightens before it darkens. The leaving veil is *held*
      after it finishes, because its owner keeps the screen black until it
      exits. It is total: a lost display or a failed present still exits `0`
      promptly, because a cosmetic fade may never strand a successful login.
      Input is ignored once it begins; the decision is already made.

  Each rides the screen's existing park deadline — one one-shot wake per frame
  while something runs, none once everything settles — so an **idle** login
  screen still arms no timer at all. A reduced-motion theme reports every
  duration as zero, which the timeline reads as *settled*: each change lands
  at once, with no second code path and no frame asked for.
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
  backdrop. Scrim and wash are **one** pass per pixel — the ends carry the
  alpha the two compose to — because a darkened picture holds fewer output
  levels than input levels, and rounding it twice, every pixel alike, banded
  a smooth sky into plateaus tens of rows deep on a 1080-row screen. The one
  dithered wash in `lib/raster` spends that missing resolution across the
  area; the entry/exit veil is the same shape and goes through it too. The
  picture is never blurred *wholesale*: frosting a whole
  wallpaper to make text sit on it hides the picture the user chose, so the
  scrim and the wash do the legibility work honestly. The shared frost stays
  where it belongs — the compositor's window backdrops, and the wallpaper
  behind one selected tile, which is a mark on that tile rather than a
  treatment of the picture.
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

That ordering is also what makes the hand-over seamless, and it is seamless
in **both** directions. The greeter fades its screen to black and only then
exits; the authority starts the desktop on that exit; the desktop **reveals
from black** over the same `SessionFade` duration, applied by the compositor
at the one point composed pixels become the scan-out frame. Going the other
way, the desktop **dissolves back into that black** before it releases the
seat — on log out, and on the fast-user-switch step-aside — and the greeter
the authority brings back appears out of it. The two halves never negotiate:
each simply animates its own second of the same continuous transition, which
is why neither needs to know about the other. The reveal is why the
compositor's hardware-layer path declines while it runs: a layer the display
scans out directly never passes through the dimming, so an accelerated frame
would appear at full brightness and lose the fade.

The desktop side is `ScreenFade` (`userland/gui/session/src/fade.rs`), one
`tairix_theme::Fade` over the compositor's screen reveal, turned around by
`arrive`/`depart` from whatever strength is on screen. Its arrival rides the
serve loop's existing park like any other animation. Its **departure cannot**:
it has to finish before the seat is given up, and parking on the session
wait-set would spin a core, because the sources the loop is no longer serving
report ready on every re-park. `run.rs`'s `fade_to_black` drives it instead —
present, sleep to the next frame on `tairix_rt::park_ns`, step — bounded by
the fade's own span, and a refused present simply stops the dim (the seat is
handed on cleared regardless). `SeatPresentation` fixes where the two sit in
a switch: `fade_out` runs only after the authority *accepted* the step-aside
and before `suspend`, while the seat and frame ring are still up; `fade_in`
runs after `reconfigure` and before `repaint_all`, so a **resumed** session
appears out of black exactly as a fresh one does rather than snapping back on
a cleared screen.

**The gap between them is the kernel's, and it is black.** Neither half owns
the seat while the greeter is exiting and the desktop is loading, so the
screen in that window belongs to the seat registry. The greeter's clean exit
releases with `ReleaseSurface::Handover`, which clears the scan-out and holds
it cleared rather than repainting the text console's retained screen over it
(`docs/src/desktop/seat.md`). That is what the fade fades *to*: without it a
minutes-old boot/text screen flashes between the two animations, and the
outgoing session's own pixels would linger for the next account. The desktop's
own clean exit and its fast-user-switch step-aside say the same thing, because
the login screen is what comes back. Every failure path releases with
`ReleaseSurface::Text` instead, so the reason lands somewhere a person can read
it — and the blank is self-healing regardless: the console takes the screen
back the moment a *program* writes to it.

A kernel diagnostic is the one thing that does not reclaim it, and that is
load-bearing here. On a shippable image the kernel's diagnostic sink renders
onto the same framebuffer, and the authority audits `SESSION_ENDED` in
exactly this gap; taking the screen for that one routine record replayed the
whole retained boot log between the desktop and the returning chooser. A
diagnostic now advances the retained grid and its log without painting a
cleared surface, while program output — a text login, a shell, a stated
failure — still takes it back whole (`lib/fbcon`, `docs/src/desktop/seat.md`).

The desktop announces itself visible once the reveal settles — one diagnostic
record, emitted after a present that reached the display, and the witness a
test keys on to photograph a screen that is actually showing something. The
first presented frame can no longer serve as that witness: it is deliberately
black, and black is indistinguishable from a blank screen. Emitting it needed
`CAP_LOG_EMIT` in `SESSION_BASELINE`, since no interactive ceiling carried it
and the kernel discarded every record a session wrote — which had also been
losing the session's own cache ledgers, silently, since long before this. The
widening is real and deliberate: any program a logged-in user runs may now
write to the machine-wide **diagnostic** log. The audit log is a separate
capability, stays kernel-only, and is attributed by the kernel rather than the
caller, so none of this reaches it.

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

### G7.1 QEMU verticals

**This gap has already cost real defects, and that is the argument for
closing it.** The first genuine boot to this screen showed no wallpaper, no
text at all, two unlabelled icons, and no pointer — while every crate
involved was green. None of those was a logic error a unit test could have
caught: the wallpaper decode was refused by a capability gate no host test
exercises, the glyph transport was never linked into the freestanding binary
at all, and the cursor was hit-tested but never drawn. A host test renders
into a `Surface` with a test transport already installed; it cannot see a
program that was built without one. The text-by-default defect (G1) was the
same lesson again: the policy was host-green, and what no host test could
see was that a real boot fed it a refusal it misread. Only a vertical that
boots the real graph can. "Host-green" must not be reported as "works".

The verticals owed are

1. **a graphical boot reaching the greeter and presenting a first frame —
   done**, `tests/integration/greeter_default_qemu_aarch64`;
2. an authentication landing on the desktop;
3. a logout returning to the greeter;
4. a switch between two accounts and back.

**1 (done).** `tairix-test-greeter-default-qemu-aarch64` boots the aarch64
`virt` board with a display and the signed input/display driver bundles on
`FsDisk::GreeterRootDisk` — the autoload driver store with the **standard**
application store, so no `os.loginType` is planted and the machine is in the
state a fresh installation boots in. The host script types the unlock
passphrase and nothing else: no account, no `desktop` command. It passes only
on two **kernel-attested** witnesses — an `APP_LOADED` naming the greeter's
bundle in the system service store, then a reply the display service serves
on `DISPLAY_ENDPOINT` after it — because a userland record reaches the
diagnostic sink alone and could be forged or truncated by user space. So the
login screen is provably login's own choice, reached after the encrypted root
mounted and its settings store answered "no configuration" rather than "not
here". The sibling autoload verticals plant `os.loginType text` precisely
because their scripts drive a shell.

**2–4 (remaining).** These need what 1 deliberately does not: a scripted
authentication at the login screen (a pointer script selecting a tile and
typed credentials reaching the greeter's own field, not the console
type-ahead the unlock prompt drains), and then screendump assertions over
the desktop that follows. Vertical 2 MUST assert on *content*, not merely
that a frame arrived: readable text present, the wallpaper drawn rather than
a flat colour, and a pointer visible — the three things a green host suite
let through. The harness for all of it now exists (`ramfb`, virtio
keyboard/mouse, `ScreendumpPlan`, `pointer_script`), so what remains is the
verticals themselves, not infrastructure.

## G8. The terminal a text session leaves behind

**Status: done.**

A text console is shared, so the end of a session is a boundary: nothing
the session left on the terminal is the next user's to see. `login` takes
the terminal back at every session boundary — a clean exit, a load refusal,
a session that never started — through the `LoginView::session_ended` seam
(the counterpart of `session_handoff`), which the production view drives
with `tairix_rt::purge_terminal`. A merely rejected credential is not a
boundary: nothing ran, so nothing is discarded.

The discarding is the kernel's, not an escape sequence the login hopes a
terminal honours (`terminal_purge`, syscall 108,
`docs/src/architecture/syscalls.md`):

- `ConsoleWrite::purge` — a retained framebuffer console (`lib/fbcon`
  `TextConsole::purge`) blanks **both** cell grids, so the alternate screen
  a full-screen program left behind cannot be revealed by whoever comes
  next; rewrites every pixel including the margins and stride slack; and
  resets the parser so a held escape prefix cannot be completed by the next
  session's first bytes. A byte-stream console owns no display, so the
  default asks the remote emulator instead, with the one shared
  `tairix_vt::control::SESSION_RESET` sequence (leave alt screen, erase
  display, erase saved scrollback, home, plain pen).
- `ConsoleRead::purge` — the type-ahead ring is re-initialised (every slot
  zeroed, not merely unqueued), so keystrokes typed ahead of the reader,
  including a mistyped credential, are neither delivered nor left in kernel
  memory. A device with no queue of its own is drained under a fixed read
  bound; the blocking adapter delegates to its backing rather than parking
  on the keystrokes the purge exists to refuse.
- `ConsoleDevice::purge_session` / `Pty::purge_session` — input first,
  then the discipline back to cooked (which also clears the secret-entry
  marker), then the display, so nothing repaints over the blank screen.

Both halves' authority is required (`CAP_CONSOLE_WRITE` at the dispatcher,
`CAP_CONSOLE_READ` in the handler before any state is touched) and only the
terminal's controlling owner is admitted. The controlling ownership itself
is deliberately untouched: releasing it here would let a task that never
held the terminal take its control.

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
| G7.1 | QEMU verticals | vertical 1 (graphical boot reaches the greeter) done; 2–4 planned |
| G8 | Text session boundary: `terminal_purge` + `session_ended` | done |
