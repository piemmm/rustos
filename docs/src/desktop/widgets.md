# Widget gallery (`widgets.app`)

The widget gallery is a first-class demo desktop app that showcases every
shared Reactive Alloy control (`lib/controls`, `plans/GUI-CONTROLS-DESIGN.md`)
on its own tab, each with several role/state/value variations. It is the
worked reference for how an application composes the shared controls: every
widget it shows is drawn and driven by `lib/controls`, and the app adds no
second control implementation.

## Layout

The window's furniture (frame, title bar, command buttons) is drawn
server-side by the compositor, so the app presents only client content: a tab
strip selecting one control family, and a panel of captioned demo widgets for
the selected family. The families are:

| Tab | Controls |
|---|---|
| Buttons | `Button` (primary / recommended / destructive / disabled / denied), `IconButton`, `SplitButton` |
| Selectors | `Toggle`, `Checkbox` (checked / mixed), `Radio` (a single-selection group) |
| Values | `Slider` (plain / capped / disabled), `Progress` (fraction / busy / failed) |
| Text | `TextField` (editable / placeholder / read-only / invalid), `SearchField` |
| Choice | `ComboBox`, `Menu` |
| Collections | `ListRow`, `TableRow`, `Card`, `Panel` |
| Bars | `Toolbar`, `ScrollBar` (vertical and horizontal) |
| Feedback | `Dialog`, `Tooltip`, `HelpTip` |
| Window | the `WindowControl` command buttons (close, minimize, put-to-back, size toggle) |

## Interaction

Click a tab, or use `Left`/`Right`, `Home`/`End`, and `Enter` on the tab
strip, to switch panels. Click a widget to interact with it (a toggle flips, a
slider moves, a combo box opens); a clicked widget keeps the keyboard focus, so
arrow keys, `Enter`, `Space`, and typed characters then drive it. `Tab` and
`Shift+Tab` move focus between the tab strip and the panel's interactive
widgets. Each control emits its typed action, which the gallery — the control's
owner — reflects straight back into the control; nothing here performs
privileged work.

## Presenting what changed

The gallery is the worked example of an app that presents the rectangle it
repainted instead of its window. Three whole-window passes used to run on every
pointer sample: a window-sized surface allocated and zeroed, the gallery drawn
into all of it, and every pixel unpremultiplied into the shared frame under a
full-window damage rectangle — after which the session diffed the whole window
again.

Now the `Run` binary holds one surface for the life of the window, and each
round of input carries a damage region (`tairix_controls::damage::sink()`) that
the controls and the gallery report into. `tairix_window::present_damage` turns
that into the rectangle to present: what was reported, clipped to the window;
the whole window for a first frame or an adopted desktop change, which re-themes
and re-densifies every pixel; and nothing at all when nothing changed. The draw
is clipped to that same rectangle, which is sound precisely because the surface
is retained — every pixel outside it is the one already on screen.

A round that changed the view but reported nothing presents the whole window.
Over-covering costs pixels; under-covering would leave a stale frame, since the
session copies only what a present declares. That safety net is not a substitute
for reporting: a host test renders the gallery before and after every event of a
scripted walk over all nine panels — hovering, pressing and releasing every
widget, then actuating the whole focus ring from the keyboard — and asserts that
every pixel which changed lies inside what that round reported.

## Structure

The app is `userland/apps/widgets`. Everything with behaviour worth testing
lives in the crate's host-tested gallery-model `[lib]` (`tairix_widgets`): the
`GalleryTab` families, the per-family panels of `DemoItem`s, the `DemoWidget`
enum that gives every control a uniform render/pointer/key/focus surface, and
the `Gallery` that lays a panel out and routes input. The freestanding `Run`
binary is a thin shell that composes the gallery over the window channel
(`lib/window`), exactly as the file manager composes `lib/browse`.

It requires a running graphical session; without one the window channel is
unreachable and the app reports the refusal on the standard error stream and
exits. It needs only `CAP_CONSOLE_WRITE` (fail-loud diagnostics) and `CAP_SHM`
(the window frame region).

## Container pointer routing

A container (`Toolbar`, `ActionRail`, `Panel`, `Dialog`, and a `Card`'s
footer) hit-tests one pointer sample **once** and delivers it to at most
three children: the one the pointer left, the one it entered, and any child
holding a press. Every other child is already at rest and would only be
written back the state it has, so a motion sample over a crowded strip costs
one rect test rather than one per child.

The pressed child stays in the stream wherever the pointer travels — the
pointer grab. Its own latch resolves against the position it last saw, so
dropping it would leave that position stale and a press dragged off the child
would fire on release instead of cancelling.

Routing is an optimisation, not a behaviour change: the state a scripted
pointer path leaves behind is identical to feeding every child every event,
and `lib/controls` pins that with a differential test against the fan-to-all
delivery it replaced.
