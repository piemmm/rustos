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

- `AuthSurface` — the surface. `new(login, shown)` names one account — read
  and marked as `shown`, offered to the authority as `login` — and goes
  straight to its secret; `with_accounts` starts on the chooser. `on_event` applies one
  pointer or key event and reports an `Outcome` (repaint needed? verified?
  what changed?), `render` paints the whole frame, `notice` is the line
  currently shown under the field, `selected_account` is the login name a
  secret is being asked for, and `field_rect` is the one placement both the
  paint and the pointer hit test read.
- `AccountTile` — one account on the chooser: display name, login name, a
  live-session flag, and the monogram drawn on its disc. Only what is drawn;
  never credential material, a capability set, or a home path. The disc itself
  is the shared `tairix_icon::monogram_disc`, so the login screen and the
  desktop's own account capsule draw one picture, not two.
- `Verifier` / `Verdict` — the seam the embedder drives. The surface holds no
  credential store and no authority: it asks about `(account, secret)` and
  reacts to `Verified`, `Refused`, or `Unreachable`. An embedder that
  authenticates its own kernel-attested caller ignores the account.
- `Chrome` — the clock, date, and host name drawn on the backdrop. Display
  text, bounded on the way in, never read back for authority.
- `EventContext` — the screen rectangle, scale, theme, verifier, and monotonic
  clock (`now_ns`) one event is answered against. The clock times every
  animation below; nothing here reads a clock of its own.
- `AuthSurface::advance` / `motion_due` — step whatever is running and ask when
  the next frame is due. Both fold every animation in every mode, and an idle
  surface returns no deadline.
- `AuthSurface::begin_entry_fade` — start the veil the screen arrives out of,
  before the first frame is presented, so a surface appears rather than being
  cut onto the black the display was handed over cleared to.
- `AuthSurface::begin_session_fade` / `session_fade_finished` — start the veil
  that takes a successful login to black, and ask whether it has arrived. The
  embedder drives it, because the embedder is what decides the login is over.
- `AuthSurface::session_fade_begun` — whether the screen has started leaving.
  One definition, so the two things that must stop when it has — accepting
  input, and drawing a pointer over the frame — cannot disagree, and neither
  of them stops for a screen that is merely arriving.
- `Backdrop` — what is painted behind the column: the theme's flat desktop
  colour, or a wallpaper the embedder has already decoded and fitted, drawn
  exactly as authored. Nothing shades the picture; what keeps the text legible
  over it is a shadow behind each line, in the theme's own desktop colour,
  through `lib/font`'s one shadowed draw. This crate never learns to decode or
  fit an image.
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
  a verdict, the two tiles a selection fade is leaving and arriving at, the
  chrome band for a clock tick — or `None` for "the whole screen" on a mode
  change and before the surface has been placed.
- **Nothing here rate-limits or counts attempts**, deliberately. The authority
  behind the verifier owns that policy and audits every attempt against the
  account. `set_cooldown` *presents* what the authority reports and refuses to
  submit while it stands; it invents no budget and reads no clock.

## Motion

Four animations, each one `tairix_theme::Timeline` over a theme duration. No
surface keeps its own start stamp, span, or frame step, and no duration is
written down here. `advance` steps them and `motion_due` reports the soonest
next frame — the minimum over whatever is running, in whatever mode, and `None`
when nothing is, so an idle screen arms no timer.

One still running answers even when its span has already run out: that frame
is its settled end state, and presenting one takes long enough that the span
routinely ends between a step and the question. `advance` draws it and stops
running — settling the cross-fade and the veil the screen leaves through,
dropping the transition, the shake, and the veil it arrived out of — which is
what makes the screen idle again.

- **The selection cross-fade** (`SelectionChange`). Moving focus dissolves the
  mark from the tile being left onto the one arriving.
- **The stage transition** (`StageTransition`). Picking an account travels the
  chosen tile's monogram disc to where the prompt's disc sits, growing as it
  goes, while the other tiles dissolve and the prompt's name, pill, and notice
  come up. It is the *layout* that is interpolated — the disc's rect and glyph
  size, and a strength on every other element's own colour — not two
  screen-sized renders cross-faded, so a transition costs one tile-sized
  scratch per tile rather than a second screen. `Escape` runs the same
  transition the other way. One strength drives both the disc's position and
  its opacity, in both directions, so a travel turned round half-way turns
  round *where it is* instead of jumping to the mirror of it.
- **The rejection shake** (`AttemptRejected`). A refusal swings the prompt
  column — disc, name, and pill — sideways in a decaying oscillation that
  comes to rest at exactly zero. There is no float maths here, so the sine
  comes from a nine-entry quarter-period integer table, interpolated and
  mirrored into the other three quadrants. The damage is the union of the
  extremes the band reaches, never the screen. The notice and the authority's
  cooldown are unchanged; the shake is additional.
- **The veil** (`SessionFade`), which runs both ways over the whole composed
  screen. `begin_entry_fade` opens it off full black, so a screen appears out
  of the black the display was handed over cleared to instead of being cut
  onto it; `begin_session_fade` closes it to opaque, so an embedder can hand
  over to whatever comes next with no cut. One veil, so both halves of a
  handover are the same black at the same weight, and a login accepted while
  the screen is still arriving leaves from the strength it had reached rather
  than brightening first. Only the *leaving* half is modal: input is ignored
  once it starts, because the decision is made and a keystroke may not re-open
  the prompt, and `session_fade_begun` reports that from the first veiled
  frame so an embedder drawing a pointer over this surface stops drawing one —
  the veil is painted into the surface, and a cursor sampled over it would
  stay bright all the way down to black. A screen still arriving answers both,
  because somebody may pick an account while it comes up, and the veil is let
  go the frame it uncovers.

Reduced motion sets each duration to zero, which a `Timeline` starts already
settled — so every one of them becomes instant with no branch here, and the
surface still reports the same notices and the same damage.
