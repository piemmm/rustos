# tairix-greeter

Stability tier: **experimental**.

The shared **authentication surface** every place TAIRiX asks a user to prove
who they are at the screen (`lib/greeter` — see `plans/NEW-DESKTOP-LOGIN.md`).

One crate owns what such a surface *is*: one centred column carrying the clock,
the date, and the machine name, and under them either the account tiles or the
chosen account's disc, name, and masked field — together with the wording, the
geometry, and the state machine that turns a keystroke into a verdict. It knows
nothing of a compositor, a window manager, a seat, or IPC — an embedder gives
it events and a verifier, takes back a painted surface and an outcome, and owns
everything about *where* those pixels and events come from.

## Consumers

- **The desktop session's screen lock** (`userland/gui/session/src/lock.rs`) —
  this surface with the account fixed to the session's own and no chooser
  behind it, verified through the per-console elevation broker.
- **The login greeter service** (`plans/NEW-DESKTOP-LOGIN.md` G3, staged) —
  the same surface with the account chooser, the wallpaper backdrop, and the
  authority's cooldown, verified through the session authority.

They may not depend on one another, so the surface lives in `lib/*`. Two
implementations of "prove who you are, at the screen" is the duplication the
charter forbids.

## What lives here

- `AuthSurface` — the surface. `new` names one account and goes straight to
  its secret; `with_accounts` starts on the chooser. `on_event` applies one
  pointer or key event and reports an `Outcome` (repaint needed? verified?
  what changed?), `render` paints the whole frame, `notice` is the line
  currently shown under the field, `selected_account` is the login name a
  secret is being asked for, and `field_rect` is the one placement both the
  paint and the pointer hit test read.
- `AccountTile` — one account on the chooser: display name, login name, a
  live-session flag, and the monogram drawn on its disc. Only what is drawn;
  never credential material, a capability set, or a home path.
- `Verifier` / `Verdict` — the seam the embedder drives. The surface holds no
  credential store and no authority: it asks about `(account, secret)` and
  reacts to `Verified`, `Refused`, or `Unreachable`. An embedder that
  authenticates its own kernel-attested caller ignores the account.
- `Chrome` — the clock, date, and host name drawn on the backdrop. Display
  text, bounded on the way in, never read back for authority.
- `EventContext` — the screen rectangle, scale, theme, and verifier one event
  is answered against.
- `Backdrop` — what is painted behind the column: the theme's flat desktop
  colour, or a wallpaper the embedder has already decoded and fitted, under a
  scrim and a soft vertical wash of the desktop colour at each end, where the
  chrome and the prompt sit. This crate never learns to decode or fit an image.
- `scrim_alpha` — how much of the theme's desktop colour that wallpaper needs
  behind the prompt block for its text to stay legible. Pure, sample-bounded,
  and computed once per (wallpaper, screen size, theme) by the embedder.
- `panel_rect`, `MAX_PASSWORD`, `MAX_LOGIN_NAME`, `MAX_CHROME`,
  `UNNAMED_ACCOUNT` — the prompt block's placement, the bounds the fields and
  the backdrop text reserve their buffers at, and the name shown when the
  embedder could not name the account.

## Guarantees

- **Only a verified secret concludes it.** No cancel, no timeout, no error
  state that falls through: a refusal, an unreachable authority, a reply that
  cannot be parsed, an empty account list, and a cooldown running out are all
  "still asking".
- **The secret lives in one place and is erased on every path out** — the
  masked field's bounded, pre-reserved buffer, wiped as soon as a verdict
  comes back, when a lockout refuses the attempt, on every step between
  accounts, and zeroised again when the surface is dropped.
- **It is operable with no pointer at all.** `Tab`/`Shift-Tab` and the arrow
  keys move between tiles and wrap at both ends, `Return` picks one, and
  `Escape` steps back to the chooser — and takes what was typed with it. A
  surface built with `new` has no chooser to step back to, so `Escape` there
  does nothing at all.
- **`Other…` is always there.** Always the last tile, present even with no
  accounts at all, leading to a typed login name so an unlisted account stays
  reachable.
- **A tile is tall enough for the name on it.** Its height is sized from
  `IconTile::label_lines` so the band under the disc holds three whole lines
  at the reference density and at a doubled one. That is capacity, not layout:
  a one-word name still draws one line, but "System Administrator" wraps
  instead of being cut, and a face wider than the reference one has a line to
  fall onto rather than being broken mid-word.
- **The paint and the hit test cannot disagree.** The prompt block, the field,
  and the tile grid each have one definition, read by the paint, the pointer
  hit test, and the damage report alike.
- **Every length is authored once, in logical pixels**, and converted through
  the one shared `Scale`, so the column is the same composition at any DPI.
- **The top of the screen does not move.** The chrome's presence and placement
  depend on the screen and the density alone, never on which body is up, so
  picking an account cannot make the clock appear or vanish.
- **Every change reports what it changed.** An `Outcome` carries the
  rectangle the next paint touches — the field for a keystroke, the block for
  a verdict, the grid for a focus move, the chrome band for a clock tick — or
  `None` for "the whole screen" on a mode change and before the surface has
  been placed.
- **Nothing here rate-limits or counts attempts**, deliberately. The authority
  behind the verifier owns that policy and audits every attempt against the
  account. `set_cooldown` *presents* what the authority reports and refuses to
  submit while it stands; it invents no budget and reads no clock.
