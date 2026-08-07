# tairix-greeter

Stability tier: **experimental**.

The shared **authentication surface** every place TAIRiX asks a user to prove
who they are at the screen (`lib/greeter` — see `plans/NEW-DESKTOP-LOGIN.md`).

One crate owns what such a surface *is*: the panel headed with the account,
the masked field, the wording, the geometry, and the state machine that turns
a keystroke into a verdict. It knows nothing of a compositor, a window
manager, a seat, or IPC — an embedder gives it events and a verifier, takes
back a painted surface and an outcome, and owns everything about *where*
those pixels and events come from.

## Consumers

- **The desktop session's screen lock** (`userland/gui/session/src/lock.rs`) —
  this surface with the account fixed to the session's own, verified through
  the per-console elevation broker.
- **The login greeter service** (`plans/NEW-DESKTOP-LOGIN.md` G3, staged) —
  the same surface with an account chooser, verified through the session
  authority.

They may not depend on one another, so the surface lives in `lib/*`. Two
implementations of "prove who you are, at the screen" is the duplication the
charter forbids.

## What lives here

- `AuthSurface` — the surface: `new` names the account, `on_event` applies one
  pointer or key event and reports an `Outcome` (repaint needed? verified?),
  `render` paints the whole frame, `notice` is the line currently shown under
  the field, and `field_rect` is the one placement both the paint and the
  pointer hit test read.
- `Verifier` / `Verdict` — the seam the embedder drives. The surface holds no
  credential store and no authority: it asks, and reacts to `Verified`,
  `Refused`, or `Unreachable`.
- `EventContext` — the screen rectangle, scale, theme, and verifier one event
  is answered against.
- `Backdrop` — what is painted behind the panel. Caller-supplied, so an
  embedder that already holds a decoded wallpaper can hand one in without this
  crate ever learning to decode an image.
- `panel_rect`, `MAX_PASSWORD`, `UNNAMED_ACCOUNT` — the panel's placement, the
  bound the masked field reserves its buffer at, and the heading used when the
  embedder could not name the account.

## Guarantees

- **Only a verified secret concludes it.** No cancel, no timeout, no error
  state that falls through: a refusal, an unreachable authority, and a reply
  that cannot be parsed are all "still asking".
- **The secret lives in one place and is erased on every path out** — the
  masked field's bounded, pre-reserved buffer, wiped as soon as a verdict comes
  back and zeroised again when the surface is dropped.
- **Nothing here rate-limits or counts attempts**, deliberately. The authority
  behind the verifier owns that policy and audits every attempt against the
  account.

## Staged work

The login greeter's own additions — the account chooser, the `Other…` tile,
the per-account attempt budget, and the wallpaper backdrop — land with the
greeter service (`plans/NEW-DESKTOP-LOGIN.md` G2.1/G3), not before a consumer
needs them.
