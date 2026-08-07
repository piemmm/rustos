# `tairix-greeter` — the authentication-surface engine

`lib/greeter` is the shared engine for every place TAIRiX asks a user to prove
who they are *at the screen*. It owns what such a surface is — the panel headed
with the account, the masked field, the wording, the geometry, and the state
machine that turns a keystroke into a verdict — and knows nothing of a
compositor, a window manager, a seat, or IPC.

Two consumers share it, and they may not depend on one another: the desktop
session's screen lock (`userland/gui/session/src/lock.rs`), which is this
surface with the account fixed to the session's own, and the login greeter
service (`plans/NEW-DESKTOP-LOGIN.md` G3, staged). Two implementations of
"prove who you are, at the screen" is exactly the duplication the charter
forbids, so there is one.

## The surface

`AuthSurface::new(account)` names whose secret is wanted; an empty name heads
the panel with `UNNAMED_ACCOUNT`, because failing to resolve a name must never
be a reason to let anybody through.

`on_event(event, ctx)` applies one pointer or key event, in the surface's own
coordinate space, and answers an `Outcome`: whether the frame on screen is now
stale (`redraw`), and whether the account was verified (`verified`). Keys edit
the secret and `Enter` offers it; the pointer places the caret within the field
and reaches nothing else.

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

The desktop's lock implements the seam over the per-console elevation broker,
which attests the caller's identity from the kernel and checks the password
against *that* uid. A test injects an answer directly, which is what lets the
whole surface be exercised on the host without a kernel.

## Geometry and scale

The panel is authored in logical pixels at the reference density and converted
through the one shared `Scale::scale_length`, so there is no second conversion.
`panel_rect` centres it horizontally, a third of the way down, and clamps to a
screen smaller than the panel rather than refusing to draw. `field_rect` is the
one definition of where the masked field sits, read by both the paint and the
pointer hit test, so the two can never resolve different rectangles.

`Backdrop` is what is painted behind the panel, chosen by the caller. Today
that is the active theme's flat desktop colour. It is a parameter rather than a
constant so an embedder that already holds a decoded wallpaper can supply one
without this crate ever learning to decode an image — decoding is the caller's
sandboxed business.

## Security properties

- **Only a verified secret concludes the surface.** There is no cancel, no
  timeout, and no error state that falls through to anything. Enforcing that
  *nothing else on the machine* sees the events while it is up is the
  embedder's half of the contract; a surface cannot do that from the inside.
- **The secret lives in exactly one place** — the masked field's bounded,
  pre-reserved `MAX_PASSWORD` buffer, which reserves once so typing can never
  reallocate and strand a copy in a freed block, draws beads rather than
  characters, and redacts itself in `Debug`.
- **It is erased on every path out.** The buffer is wiped as soon as a verdict
  comes back — verified, refused, or unanswerable alike — and zeroised again
  when the surface is dropped, so an abandoned prompt leaves no plaintext
  either.
- **Nothing here rate-limits or counts attempts**, deliberately. The authority
  behind the verifier owns that policy and audits every attempt against the
  account; a second policy on this side would be a second place to get it
  wrong, and one that could not slow down an attacker holding the keyboard
  anyway.

## Stability

Experimental. The account chooser, the per-account attempt budget, and the
wallpaper backdrop land with the greeter service that needs them
(`plans/NEW-DESKTOP-LOGIN.md`), not before.
