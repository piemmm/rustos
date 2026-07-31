## NAME

widgets — Reactive Alloy widget gallery

## SYNOPSIS

`widgets`

## DESCRIPTION

Opens a desktop window that demonstrates every shared TAIRiX GUI control on
its own tab: buttons, selectors, value controls, text fields, choice controls,
collections, bars, feedback surfaces, and window controls. Each tab shows
several variations of its family — different roles, states, and values — so the
full behaviour of each control is visible and interactive in one place.

Switch tabs by clicking the tab strip or with the `Left`, `Right`, `Home`, and
`End` keys and `Enter`. Click a widget to interact with it: a toggle flips, a
slider moves, a text field takes the caret, a combo box opens. A clicked widget
keeps the keyboard focus, so the arrow keys, `Enter`, `Space`, and typed
characters then drive it; `Tab` and `Shift+Tab` move focus between the tab
strip and the widgets.

The gallery is launched from the desktop's Program Library (the taskbar's
Library button) or by name from a shell. It
requires a running graphical session: without one the window channel is
unreachable and the gallery reports the refusal on the standard error stream
and exits.

## EXIT STATUS

Zero after a clean close; non-zero when the window channel or the shared frame
region was refused (the reason is stated on the standard error stream).
