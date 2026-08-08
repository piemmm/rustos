# `tairix-greeter` — the authentication-surface engine

`lib/greeter` is the shared engine for every place TAIRiX asks a user to prove
who they are *at the screen*. It owns what such a surface is — one centred
column carrying the clock, the date, and the machine name, and under them
either the account tiles or the chosen account's disc, name, and masked field —
together with the wording, the geometry, and the state machine that turns a
keystroke into a verdict. It knows nothing of a compositor, a window manager, a
seat, or IPC.

Two consumers share it, and they may not depend on one another: the desktop
session's screen lock (`userland/gui/session/src/lock.rs`), which is this
surface with the account fixed to the session's own, and the graphical login
screen (`userland/session/greeter`), which starts at a chooser of the
machine's accounts. Two implementations of "prove who you are, at the screen"
is exactly the duplication the charter forbids, so there is one.

## The surface

`AuthSurface::new(account)` names whose secret is wanted and goes straight to
the field; an empty name is shown as `UNNAMED_ACCOUNT`, because failing to
resolve a name must never be a reason to let anybody through. That is the
lock's constructor: a lock has exactly one account to ask about, so it has no
chooser and `Escape` does nothing.

`AuthSurface::with_accounts(tiles)` is the login screen's constructor: it opens
on the chooser instead.

`on_event(event, ctx)` applies one pointer or key event, in the surface's own
coordinate space, and answers an `Outcome`: whether the frame on screen is now
stale (`redraw`), and whether the account was verified (`verified`). Keys edit
the secret and `Enter` offers it; the pointer places the caret within the field
and reaches nothing else. `EventContext` carries the monotonic clock the surface
times motion from.

On the chooser, moving focus cross-fades the selection mark over the theme's
`SelectionChange` duration (instant under reduced motion). `advance(now_ns)`
steps a running fade and reports the union of the two tiles it touches;
`motion_due(now_ns)` is the nanoseconds until the next fade frame, or `None`
when nothing is animating so an idle embedder arms no timer.

`render(screen, scale, theme, backdrop)` paints the whole frame and yields
`None` — never a partial or empty frame — when a screen has no pixels or a
surface could not be allocated, so an embedder that must cover the display
fails closed on it.

## The verdict seam

The surface holds no credential store, no key, and no authority. It asks
through `Verifier`, which answers one of three things:

| Verdict | Meaning |
|---------|---------|
| `Verified` | the secret belongs to the account — the only answer that lets anybody through |
| `Refused` | a real answer about a real secret: it is not the account's |
| `Unreachable` | no answer could be obtained — nothing listening, a transport fault, a reply that is not this protocol |

A refusal and an unreachable authority read differently on screen ("wrong
password" and "I could not ask" call for different reactions from the person
at the keyboard) but conclude identically: still asking.

`verify` is told the login name the surface is currently asking about. The
login screen forwards it to the session authority; the desktop's lock ignores
it, because its broker attests the caller's identity from the kernel and checks
the password against *that* uid — a name on the wire could not weaken it, and
is not consulted. A test injects an answer directly, which is what lets the
whole surface be exercised on the host without a kernel.

## The chooser

A surface built `with_accounts` opens on a centred row of account tiles — a
monogram disc, the display name, and a badge on any account that already has a
live session — and moves to the secret field for whichever is picked. The row
wraps into a grid only when the screen cannot hold it, so a grid where a row
would do never reads as a list. `Escape` returns to the chooser and wipes
whatever had been typed.

A tile's height comes from `IconTile::label_lines`, sized so the band under
the disc holds three whole lines at the reference density and at a doubled
one. That is capacity rather than layout — a one-word name still draws a
single line — but "System Administrator" wraps instead of being cut, and a
face wider than the reference one has a line to fall onto rather than being
broken mid-word.

An `Other…` tile is always present and always last, even when the list is
empty, and leads to a typed login name. It wears a disc bearing an ellipsis in
the quiet plate colours, so it reads as a peer of the accounts rather than as a
leftover while staying plainly not one of the listed people. An account the
chooser could not be told about therefore stays reachable, and a machine whose
account list could not be read is still a machine somebody can log in to.

The whole surface is operable from the keyboard alone: `Tab`/`Shift-Tab` and
the arrow keys move focus with wrap-around, `Return` activates. The tiles are
`lib/controls` `IconTile`s, so the login screen looks and behaves like the rest
of the desktop rather than being a second visual vocabulary.

## The attempt cooldown

`set_cooldown(remaining)` shows the authority's per-account lockout and makes
the surface refuse to offer a secret while it is running — a submit re-states
the wait rather than reaching the verifier, and erases what was typed, since a
lockout can outlast the user's patience. The engine invents no budget and reads
no clock: the remaining time is a `Duration64` the embedder supplies from the
authority's own answer, so there is exactly one attempt policy on the machine
and it lives behind the verifier.

## Chrome

`set_chrome` supplies the clock, date, and host name at the top of the column:
the time large and light, the date beneath it, the machine name beneath that,
each centred in its own fixed band. They are bounded display text, never read
back for authority, and any of them may be empty — a machine that cannot tell
the greeter its host name shows no host name rather than a guess.

The chrome's presence is a function of the screen and the density *alone*,
never of which body is up, so a screen cannot appear to gain or lose its clock
when an account is picked. A screen too short to hold the chrome and still show
the prompt keeps the prompt: asking for a secret is what the screen is for.

## The column

One centred vertical stack, the same skeleton in both modes so the screen never
appears to jump:

| Band | Logical size | What sits there |
|------|--------------|-----------------|
| chrome | 106 tall, full width, 40 from the top | clock (64), date (24), host (18) |
| disc | 88 × 88 | the chosen account's monogram, in the accent |
| name | 26 tall, full width | the display name, 14 under the disc |
| block | 420 × 96 | the field and the lines under it, 18 under the name |
| field | 320 × the theme's control height | the pill, at the block's top |
| notice | 20 tall | the hint, refusal, or lockout, 10 under the field |
| step-back | 18 tall | `Escape` returns to the chooser, when there is one |

The body is centred in the space beneath the chrome, so both the tile grid and
the prompt hang off the same block. The block is wider than the pill because
the notice under it is prose; a block only as wide as the field would cut it
short.

Every length above is authored in *logical* pixels at the reference density and
converted through the one shared `Scale::scale_length`, so there is no second
conversion and the composition is the same at any DPI. `panel_rect` returns the
block — the region whose legibility the scrim is chosen for — and `field_rect`
is the one definition of where the pill sits, read by both the paint and the
pointer hit test, so the two can never resolve different rectangles.

The field is drawn as a **pill**: the shared `lib/controls` field is rendered
into a scratch row, confined to a stadium, and laid over a stadium in the edge
colour, which stands in for the plate's rim, its focus gap, and its focus ring
— a square ring inside a round field would give the shape away. The edge takes
the danger colour on a refusal exactly as the plate's own rim would have. A
trailing submit mark is drawn only while the field is empty, because a typed
secret's beads scroll to that edge.

## The backdrop

`Backdrop` is what is painted behind the column, chosen by the caller: either
the active theme's flat desktop colour, or an already-decoded, already-fitted
wallpaper under a scrim. This crate never learns to decode an image — decoding
untrusted bytes is the caller's sandboxed business.

No blur is reachable from here (that lives in the compositor), so a soft
vertical wash of the desktop colour is laid over the top and bottom thirds,
where the chrome and the prompt sit. Because the wash *is* the desktop colour,
it composites to exactly what is already there over the flat backdrop, and only
a picture ever sees it.

`scrim_alpha(image, panel, theme)` derives how much scrim that wallpaper needs
for the block's text to stay legible. It sizes for the *brightest* patch under
the block rather than the average, because text sits over the worst pixel and
not over the mean; it samples on a bounded grid, so the cost is the same for a
thumbnail and for a 4K master; and it is bounded well short of both extremes,
so the prompt is never bare and the wallpaper is never blacked out. It is pure,
so an embedder computes it once per wallpaper, screen size, and theme rather
than per frame.

## Damage

Every `Outcome` reports the rectangle the next paint will change: the field for
a keystroke, the block for a verdict, the two tiles a selection fade is leaving
and arriving at, the chrome band for a clock tick, and the whole screen for a
mode change or a first frame.
An embedder presenting to a compositor or a display service therefore uploads a
small rectangle for a keystroke instead of a screen. The reported rectangle is
always a superset of what actually changes; the crate's tests assert that
pixel by pixel.

## Security properties

- **Only a verified secret concludes the surface.** There is no cancel, no
  timeout, and no error state that falls through to anything. `Escape` steps
  back to the chooser; it does not conclude. Enforcing that
  *nothing else on the machine* sees the events while it is up is the
  embedder's half of the contract; a surface cannot do that from the inside.
- **The secret lives in exactly one place** — the masked field's bounded,
  pre-reserved `MAX_PASSWORD` buffer, which reserves once so typing can never
  reallocate and strand a copy in a freed block, draws beads rather than
  characters, and redacts itself in `Debug`.
- **It is erased on every path out.** The buffer is wiped as soon as a verdict
  comes back — verified, refused, or unanswerable alike — on a submit a
  cooldown refuses, on every step between accounts, and again when the surface
  is dropped, so an abandoned prompt leaves no plaintext either.
- **A tile discloses only what it draws.** An `AccountTile` carries a display
  name, a login name, and whether that account has a live session. Nothing
  derived from a stored password, and no uid, home path, or capability set,
  reaches this crate at all.
- **Nothing here rate-limits or counts attempts**, deliberately. The authority
  behind the verifier owns that policy and audits every attempt against the
  account; a second policy on this side would be a second place to get it
  wrong, and one that could not slow down an attacker holding the keyboard
  anyway.

## Stability

Experimental.
