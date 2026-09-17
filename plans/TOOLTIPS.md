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

The desktop declares tips for its own surfaces too — its menu rows — where it
is both sides of that table. The split still holds: what is declared is a
region and a line, and the seat owns everything else.

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
- **The deadline is measured from the rest, not from an event.** The seat
  records where the pointer stopped and when (`AtRest`), and a tip is due
  `TOOLTIP_DWELL_NS` after *that*. Two things follow, and both are load-bearing
  rather than incidental:
  - **A declaration that lands under a pointer already at rest arms its own
    dwell.** Nothing else can: the pointer has stopped and will send no further
    sample. This is the menu chain's ordinary case — the shell sees the motion
    sample first, because it owns the tracked pointer the chain is asked
    against, and only *then* does the chain move its highlight and declare the
    row's explanation. Without this a row's tip appeared only if the hand
    happened to jiggle. The declaring caller supplies where that source's
    regions begin, so `declare` can resolve one without the seam its other
    calls take; `None` is a source the seat cannot place, and fails closed.
  - **A declaration equal to the one held is not a new one**, and does nothing
    at all. A surface re-presenting an unchanged row would otherwise take its
    tip down and count again on every frame: a tip that is due would never fall
    due, and one already up would blink.
  - **A dismissal spends the rest.** A press, key, or scroll means the user has
    moved on from asking, so a declaration landing later under the same still
    pointer arms nothing until the pointer moves again — otherwise a changed
    declaration over the region just pressed would pop a tip straight back up
    with no wait at all.
  A stationary hand still produces samples, and they do not move the deadline,
  because the rest they report has not changed.
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
| The session's intake and presentation — `ShellWindowHost::tooltip_declared` feeding `SeatTooltip`, the plate's compositor window, and the seat's lifetime hooks | **done** |
| Menu rows: a row's explanation shown on dwell rather than drawn beside its label | **done** |
| A QEMU vertical that puts a tip on screen | **remaining** |

**Nothing has yet seen a tip on a real screen.** Every part above is covered by
host tests — the dwell and placement over `SeatTooltip`, the chain's
`hovered_tip`, and the declare/dwell/present/withdraw round over
`DesktopShell` — but no enrolled guest dwells a pointer on a refused row and
photographs the plate. That is the same gap `plans/NEW-MENUS.md` D17 closed for
the chain itself, and it is open here for the same reason: the tip needs a
*rest*, so a vertical has to hold the pointer still across a frame tick rather
than click and move on. The honest gate is the one D17 built — a tip is on
screen only once a frame carrying it reached the display — so the vertical
wants a record of its own beside `MENU_SHOWN` before it can assert anything.

## The two consumers

A declaration is keyed on a `TipSource`, not a bare id, because two namespaces
reach the one map and must not be able to collide: an application's
window-channel id and the desktop's own menu chain. A channel id that happened
to equal the chain's key would answer one pointer with the other's line.

- **`TipSource::Window(WindowId)`** — an application, through
  `WindowRequest::SetTooltip`. `ShellWindowHost::tooltip_declared` resolves the
  channel id to the compositor window presenting it *at declaration*, where the
  session's window map is at hand, so placing the tip later needs only the
  compositor; `window_closed` forgets it.
- **`TipSource::Chain`** — the desktop's own menu rows. A row that cannot be
  chosen states why on dwell, never in a caption beside its label: that caption
  made every plate as wide as its longest excuse, so `MenuItem` carries no help
  text at all and `ChainRow::explained` holds the line.
  `DesktopShell::present_menu_chain` declares it from `MenuChain::hovered_tip`
  once the plates are placed, so the region is where the row actually is, and
  withdraws it when no row explains anything — both internal to that
  presentation, since declaring for the chain is not a seam anything outside
  the shell reaches. The region is already screen space, because the chain
  placed its own plates.

`AppMenuItem::reason` therefore stays on the wire. An application cannot
declare a tooltip region on a plate the *desktop* owns, so the declared reason
is what the desktop puts in the tip; only the inline rendering is gone.

## How the session drives it

- `DesktopShell::apply` feeds `tooltip_pointer` on pointer motion and
  `dismiss_tooltip` on every other event, so a tip answers a *rest* and any
  press, key, or scroll ends it.
- `settle` presents once per batch of applied events, so a run of motion
  samples costs one plate update.
- The session's park is shortened by `tooltip_park_deadline_ns` and the frame
  tick calls `tooltip_tick`, so a resting pointer wakes nothing until the tip
  is due — a deadline, not a poll.
- `present_tooltip` runs after the chain is presented, so the plate sits above
  the row it explains, and marks the window input-transparent so it never
  becomes the pointer target it appeared under.
