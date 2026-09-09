# TOOLTIPS — the seat's one tooltip

Binding under `AGENTS.md`. How any application, or any part of the desktop,
has a short line shown for a region of its own window — and who owns each
part of that.

## The split

An application says **two things** and no more: *this region of my window*
and *this one short line*. Everything else belongs to the desktop, because
everything else belongs to the seat:

| Owned by | What |
|---|---|
| The application | the region (its own client pixels) and the text |
| The desktop | the dwell, the placement, the pixels, and every reason the tip goes away |

An application is never told where its window sits on screen and never learns
a pointer position inside the seat, so it could not place a plate truthfully
or time a dwell even if it owned them. This is the same division the menu
chain already uses (`plans/NEW-MENUS.md`).

## The wire

`WindowRequest::SetTooltip { window_id, region, text }` (op 16, `lib/abi`):

- **Window-scoped, no capability.** The window the caller owns is the scope,
  exactly as `OpenMenu` — and ownership is the kernel-attested identity of the
  in-flight caller, never the named id.
- **`region` is a `WindowRegion`** — the same 16-byte window-local rectangle a
  menu anchor is. One type, because the two ask the same question of the
  session; a second would be the same bytes under another name (this is the
  rename of the former `MenuAnchor`).
- **`text` is a `TooltipText`** (`BoundedText<0, TOOLTIP_TEXT_MAX>`): bounded
  and control-character-refusing like a `WindowTitle`, validated at
  construction *and* again at decode.
- **Idempotent replace, and empty withdraws.** A window holds at most one
  declaration, so a second replaces the first; empty text retracts it and
  takes a tip already on screen down. One operation, so there is no second
  "hide" to fall out of step with the first.
- The frame is exact-length; a trailing byte is a field smuggled past the
  operation's end and is refused.

`WindowClient::set_tooltip` is the client call. The engine validates the
window and the bounds and relays the declaration through the
`WindowHost::tooltip_declared` callback, whose default **refuses**: a host
with no seat to hover on cannot honour a tip, and saying so is more honest
than accepting one nothing draws.

## The desktop's half

`userland/gui/session/src/tip.rs` — `SeatTooltip`, the seat's one tooltip:

- **One at a time**, for the reason there is one menu at a time: it is the
  *seat's* tip, and two would be two answers to one pointer.
- **The dwell** is `TOOLTIP_DWELL_NS` (600 ms), armed while the pointer rests
  inside a declared region and cleared the moment it leaves. It is a
  *deadline, not a poll*: `park_deadline_ns` shortens the session's own park
  to the moment the tip is due and `tick` resolves it, so a resting pointer
  wakes nothing until then — the taskbar picker's exact mechanism.
  Re-arming is suppressed while a dwell for the same window is running, so
  the delay is a rest rather than a countdown restarted by every sample of a
  stationary hand.
- **The placement** is the one shared plate rule
  (`tairix_controls::plate_rect`), asked for below the region with a scaled
  gap: a tip under the pointer's own arrow is the one place it does not cover
  what the user is looking at, and the shared rule flips it above at the
  screen's bottom edge and slides it along every other. There is no second
  copy of that arithmetic.
- **The pixels** are the existing `tairix_controls::Tooltip`.
- **The lifetime**: any press, key, or scroll (`dismiss`), leaving the region
  (`pointer_moved`), the owner withdrawing (`declare` with empty text, or
  `withdraw`), the owner dying or its window closing (`forget`), and any
  change of scale, theme, or display mode (`dismiss`).
- **A window the seat cannot place** has its region unresolvable, so it is
  never hovered and never placed — fail closed rather than anchoring a plate
  somewhere invented.

## A tooltip must not take the pointer it explains

A tip appears *under* the pointer by construction, so a plate window that
became the `pointer_target` would fight the very hover it exists to explain.
`Compositor::set_input_transparent` is the compositor concept that answers
that: such a window is composited exactly as before but is never resolved to
by `pointer_target` or `window_at`, so it neither takes the pointer nor
shadows the window beneath it. Its pixels do not change, so the change marks
no damage. A non-interactive overlay is a real compositor concept, not a
tooltip special case.

## Deliberate non-goals

- **No rich content.** One short line. An application with more to say has a
  window to say it in, and a plate the width of the screen is not a tooltip.
- **No application-owned placement.** See the split above.
- **No second plate rule, and no second minimise/plate/blend path.**
- **No keyboard-triggered tip.** The dwell is a pointer rest; a keyboard user
  reaches an application's own help (`plans/APPS.md`).

## Status

| Part | State |
|---|---|
| The wire (`SetTooltip`, `TooltipText`, `WindowRegion`), exact-length decode, fuzz seeds | **done** |
| `WindowClient::set_tooltip`, the engine's validation, `WindowHost::tooltip_declared` | **done** |
| `SeatTooltip`: declarations, dwell, placement, lifetime, render | **done** |
| `Compositor::set_input_transparent` and its hit-testing exclusion | **done** |
| The session's intake and presentation — `ShellWindowHost::tooltip_declared` feeding `SeatTooltip`, the plate's compositor window, and the seat's lifetime hooks | **remaining** |

**No consumers, deliberately.** The facility lands with nothing calling it:
that was the owner's decision, and it is a stated deviation from the charter's
"no speculative surface" rule (§2.3/§2.4) rather than an oversight. The
mechanism is covered by host tests instead of by a caller.

The **remaining** row is the one piece that is not merely unconsumed but
unwired: until it lands, a declared tooltip is validated and refused by the
session's default rather than shown. That wiring is: a `SeatTooltip` on the
session's window host, the `tooltip_declared` intake resolving the window to
its live client origin, a compositor window for the plate marked
input-transparent, a present step beside the menu chain's, and `dismiss`
called from the seat's press/key/scroll/mode paths.
