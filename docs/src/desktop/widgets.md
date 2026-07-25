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
