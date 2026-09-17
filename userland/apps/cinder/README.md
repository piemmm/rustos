# `cinder` — the desktop companion

Cinder, the TAIRiX mascot, as a virtual pet. He lives in a playpen window and
can be let out onto the desktop, where he wanders, chases the pointer, climbs
onto windows, burrows under them, or walks around them.

## What is here

The crate's `[lib]` is the whole model, host-tested without a screen:

| Module | What it is |
|---|---|
| `project` | The elevated camera: one `GROUND_DEPTH` constant everything projects through, and the depth sort that turns the creature round for free. |
| `fur` | The soft fuzzy disc he is built from: a shared falloff table, an integer pixel loop, and a deterministic per-blob rim ripple. |
| `cinder` | The mascot palette, the 44-blob skeleton, and the pose that places it. |
| `gait` | Locomotion: the leg cycle, the body bob, the tail, the ears, the jump arc, the burrow crouch. |
| `mind` | Three needs and a closed set of intents, drawn from an injected generator so every decision is reproducible. |
| `world` | The desktop as terrain, and the over/under/around route state machine. |
| `pen` | The playpen: whereabouts, petting, dragging, the toy. |
| `layout` | The pen's geometry and the companion surface's, in logical pixels. |
| `paint` | The two painters — the pen window, and the transparent companion surface. |
| `state` | What survives a restart: the mood, and whether he was out. |

`src/run.rs` is the bundle's `Run` binary; it only composes the above over the
window channel.

## Stability

`experimental`.

## The capability

Being present on the desktop outside a window of its own requires
`CAP_DESKTOP_LAYER`, which the signed `AppInfo` requests. Everything that makes
that safe is the desktop session's, not this app's: the surface is bounded well
below the narrowest trusted prompt, is never in the focus rotation, catches the
pointer only where its own content is opaque, sits below the icon bar at its
highest depth, and is hidden with both its feeds stopped whenever a trusted
surface is up.

An account whose ceiling does not carry the capability simply cannot let Cinder
out: the refusal is stated on `stderr` and in the pen, and the pen keeps
working.

## Where the design lives

`plans/CINDER.md`, and `docs/src/desktop/cinder.md`.
