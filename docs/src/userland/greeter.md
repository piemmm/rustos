# Graphical login screen (`userland/session/greeter`)

`greeter.app` is the screen TAIRiX puts up to ask who is at the machine. It
owns the seat, paints the shared authentication surface, and relays what was
typed to the [session authority](login.md) — which is where every decision
is made. The installed binary lives at `/System/Services/greeter.app/Run`,
and the authority spawns it as the dedicated `greeter` service account
(uid 16, primary group `services`).

**The greeter draws and types; the authority decides.** They are two
processes because the greeter renders untrusted image bytes and links the
whole drawing stack, while the authority holds the two most dangerous grants
on the machine — the one that reads the user database and the one that
starts a process as another user. Folding the drawing stack into the
authority would put a large parsing and rendering surface inside the one
process that can mint any user's identity.

The crate is `no_std` (with `alloc`), has no `unsafe` outside the `Run`
binary's one documented frame mapping, and depends only on audited `lib/*`
crates, so it links no kernel, driver, or `userland/gui/*` crate. What a
screen-authentication surface *is* — the panel, the masked field, the
chooser, the geometry, the cooldown display — belongs to
[`tairix-greeter`](../lib/greeter.md), which the desktop's screen lock
composes too; this service is one of that engine's two embedders and adds
only a seat, frames, an account list, and a channel to the authority.

## Capabilities, and why they stop there

The signed `AppInfo` requests exactly seven, and the `greeter` account's
ceiling in [`lib/users`](../lib/users.md) grants exactly the same seven, so
the intersection the loader takes cannot be wider than this list:

| Capability | Why |
|---|---|
| `CAP_DISPLAY` | hold the seat's exclusive revocable lease and configure the display service |
| `CAP_INPUT_READ` | drain the owned seat's keyboard and pointer channels |
| `CAP_SHM` | create the double-buffered frame region and grant it to the display service, so frames never cross the IPC |
| `CAP_FS_ACCESS` | read the shipped wallpaper master under the read-only `/System` |
| `CAP_SANDBOX_SPAWN` | decode those untrusted bytes in a capability-empty worker instead of in the address space that owns the seat |
| `CAP_CONSOLE_WRITE` | state an abnormal exit's reason on `stderr` |
| `CAP_LOG_EMIT` | its own audit records |

What it does **not** hold is the point of the design, and the account
ceiling's own tests pin each absence:

- **No `CAP_USERS_READ`.** It never sees a credential store. Everything it
  knows about the machine's accounts is what the authority chose to publish
  — a display name, a login name, and whether that account already has a
  live session — and it learns nothing at all about a secret beyond one of
  three answers.
- **No `CAP_PROC_SPAWN` and no `CAP_SPAWN_AS_USER`.** The narrow
  `CAP_SANDBOX_SPAWN` it does hold admits only a canonical parser sandbox —
  a child the kernel itself brands capability-empty, with no credential
  switch and no console — so the greeter still cannot start the session it
  is authenticating for, and can never choose *which* program runs as the
  authenticated user. The authority starts that on its own loop after the
  greeter exits.
- **No `CAP_IPC_BIND_PRIVILEGED`.** It serves nothing and binds no reserved
  rendezvous; it is only ever a *client* of the session and display
  endpoints.

Compromising it therefore yields a screen, not an account.

## Bring-up

1. **Acquire the boot seat's exclusive lease.** A seat already held means
   there is no screen to own.
2. **Query the display mode** and size the scan-out from it.
3. **Map a double-buffered frame region** and grant it to the display
   service, so a frame is presented by handing over a region rather than
   copying pixels through IPC.
4. **Page the offerable accounts** off the authority's `session-v1`
   endpoint into chooser tiles. The walk is bounded three ways — by the
   `total` the first page announced, by a ceiling of eight pages' worth of
   tiles, and by the offset having to advance every round — because the
   authority's answer is input like any other; a page that repeats an
   offset, overruns the total, or never says it is last ends the walk
   rather than spinning.
5. **Paint the first frame** and audit `SCREEN_READY`.
6. **Park.**

A verified secret exits `0` immediately, which also releases the seat: the
authority is watching for that exit and starts the session itself. Exiting
rather than lingering is deliberate — the seat hand-off then has one
mechanism (the kernel's reclaim on exit), no lease transfer is needed, and a
login screen cannot hold the screen behind a running desktop. The release is
owner-checked on every exit path, and a lease already lost refuses with a
typed error that is ignored rather than escalated.

## How it parks

An idle login screen must consume nothing. There is one wait set, holding
the seat's input, and its timeout is the *next* thing that actually needs a
repaint: the next clock-minute boundary, or the next one-second tick of a
lockout while one is counting down, whichever is nearer. When neither
applies the wait has no timeout at all, so an untouched screen arms no timer
and takes no interrupt. There is no poll loop and no yield.

A wake that finds nothing changed presents nothing, and every repaint
presents only the damage rectangle the surface reported — the field for a
keystroke, the panel for a verdict, the chrome band for a clock tick — so a
keystroke uploads a small rectangle rather than a screen.

A wake also drains a whole burst before it presents. Every waiting keyboard
and pointer record is applied, what each changed is merged into one
rectangle, and the display is called once with it, because a present is a
round trip to the display service and a moving mouse delivers reports far
faster than a screen refreshes. A drain that changed nothing calls nothing;
a verified secret stops the drain there and then, having presented what the
records before it changed.

## The pointer

The seat reports *relative* motion, so the screen keeps the running position
and holds it inside the frame, and hands the surface the absolute position it
hit-tests. One seat report expands to at most two surface events (a button is
a move **and** a transition) and they present as one frame, not two.

The pointer is also drawn, which is what makes it usable: the built-in
`Arrow` from [`lib/cursor`](../desktop/cursors.md) is rasterised **once** at
start-up for the active UI scale, and it is **sampled over the surface as
the frame is composed** rather than painted into it, so it is on top of
everything the surface drew and the pixels beneath it are still there when
it moves off them. Motion presents the union of the cursor's old and new
rectangles clipped to the screen — never a whole-screen present for a mouse
move, and never a cursor left painted where it no longer is — and motion the
screen edge swallows presents nothing at all. Placement is
`tairix_cursor::PlacedCursor`, the same type the compositor draws its
pointer with; a login screen may not depend on the window manager, so the
placement lives in `lib/cursor` and there is one definition of it.

Keeping the cursor out of the surface is what makes a moving mouse cheap.
The rendered surface is kept between frames and rebuilt only when its own
content changes — a keystroke, a verdict, a countdown, a clock tick, a
chooser tile taking the focus, an arriving wallpaper — so a report that
slides the pointer over an unchanged screen re-composes a cursor-sized patch
of pixels that already exist and renders nothing. Motion that does change
something merges the two rectangles and pays exactly one render for the
report, never one per surface event it expanded into.

## The wallpaper is untrusted input

The shipped wallpaper is attacker-shaped data like any other image, so it is
never decoded in the address space that owns the seat. The greeter
re-enters its own binary as a capability-empty sandbox worker and decodes
and screen-fits the image there, under a fixed input-byte bound; the worker
role is checked before anything else in `main`, ahead of any seat work. The
contrast scrim the panel's text needs is then derived once per wallpaper,
screen size, and theme — never per frame.

## Degradation

A login screen that refuses to appear locks a user out of their own machine,
so every absence short of "there is no screen" is presented rather than
fatal:

| Absent | What happens |
|---|---|
| No account list — the authority is unreachable, or answered with something that is not a page | the chooser stands with its typed-name tile alone, audited `ACCOUNTS_UNAVAILABLE` |
| The authority unreachable when a secret is offered | the surface says so and keeps asking. It does **not** exit, so a transient fault cannot spend the authority's restart budget |
| No wallpaper, or one that will not decode | the theme's flat desktop colour |
| A pointer that will not rasterise | no cursor is drawn, audited `POINTER_UNAVAILABLE`. The pointer still moves, still hit-tests, and the keyboard alone logs in regardless |
| No trusted clock, or no host name | that line of chrome is empty. Never invented |
| A refused, unqueryable, or zero-extent display mode; a refused frame region; a wait set that could not be built | **fatal** |

The fatal cases all mean the same thing: there are no pixels, or the only
way to keep going would be to busy-poll. Each states its reason on `stderr`,
audits `SCREEN_UNAVAILABLE`, and exits with the bring-up code that names
what was missing — after three such exits the authority runs the text login
instead, so the machine is always loggable into. No exit reason ever names
an account or anything about a secret.

A refusal and an unreachable authority read differently on screen, because
"wrong password" and "I could not ask" call for different reactions from the
person at the keyboard, but they conclude identically: still asking. Only a
verified secret finishes the screen — there is no cancel, no timeout, and no
guest path.

## Audit events

The service owns the reserved `EventId` range `19000..20000`; the
authority's own records for the same login are in its `10000..11000` range.

| Id    | Constant                | Meaning |
|-------|-------------------------|---------|
| 19001 | `SCREEN_READY`          | the seat is held, the mode is known, and the first frame is up |
| 19002 | `SCREEN_UNAVAILABLE`    | no screen could be brought up; the authority falls back to a text login |
| 19003 | `ACCOUNTS_UNAVAILABLE`  | the account directory could not be read; the chooser stands with its typed-name tile alone |
| 19004 | `VERDICT_RECEIVED`      | the authority answered an offered secret |
| 19005 | `AUTHORITY_UNREACHABLE` | a secret was offered and no verdict came back |
| 19006 | `WALLPAPER_UNAVAILABLE` | the wallpaper could not be read or decoded; the flat desktop colour is drawn |
| 19007 | `POINTER_UNAVAILABLE`   | the pointer artwork would not rasterise; the pointer works but is not drawn |

## Tests

`cargo test -p tairix-greeter-service` exercises everything about *what the
screen does* behind the injected seams: a keystroke reaching a verdict, a
refusal becoming a countdown, an empty account list, the bounded and
malformed paging walks, the park deadline (including an untouched screen
arming no timer), the drawn pointer and its damage, the kept surface (a move
that changes nothing renders once in total, and the frame it leaves is what
a full repaint would have drawn), and the surface-to-scan-out composition.
`tests/session_v1.rs`
additionally wires the transport seam straight to the authority's own
request handler — a test-only edge — so the two halves of one protocol are
proven against each other rather than each against its own mock.
