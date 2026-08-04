## NAME

wallpaper — graphical desktop backdrop chooser

## SYNOPSIS

`wallpaper`

## DESCRIPTION

Opens a desktop window offering the wallpapers the system ships, the
backdrop colour behind them, and how the desktop arranges the icons on
its pinboard. Nothing changes on screen until the settings are applied.

The grid lists every shipped wallpaper as a thumbnail, plus a **No
wallpaper** entry that shows the chosen backdrop colour alone. Each
thumbnail is rendered under the currently chosen fit, so a preview
shows what the desktop will actually do with that image. A file that
cannot be decoded shows a marked placeholder tile with its name and is
not attempted again.

Wallpaper images are never decoded by this program. Each one is
rendered by a separate sandboxed process that holds no filesystem,
network, or spawn authority, so a malformed image cannot compromise the
chooser or the desktop.

The option rows below the grid are:

- **Fit** — how the image is placed: `fill` (cover the screen, cropping
  the overflow), `fit` (contain it whole, backdrop colour in the bars),
  `stretch` (distort to the exact screen size), `centre` (native size,
  centred), and `tile` (repeat from the top-left).
- **Backdrop** — the flat colour shown wherever the wallpaper does not
  reach: `Theme` follows the active desktop theme, and the named
  colours are fixed. A colour already in effect that is not one of the
  named ones is offered under its own `rrggbb` spelling.
- **Icons** — which side of the pinboard the desktop icon grid grows
  from.
- **Sort** — the order the desktop folder's icons are listed in.

The window is driven by the keyboard. `Tab` and `Shift-Tab` move focus
forward and back through the grid, the option rows, and the buttons.
The arrow keys move within the thumbnail grid, or change the focused
option. `Enter` activates the focused button, and `Escape` closes the
window without applying.

Applying sends the chosen settings to the desktop session, which
decides whether to adopt them, redraws the pinboard, and saves them for
the next login. This program never writes the settings itself. The
result is reported on the status line under the option rows: applied,
refused with the session's reason, or no desktop session listening. A
refusal leaves the window open with the choices intact.

Only the shipped wallpaper store is offered; an image elsewhere on the
system cannot be chosen from this window. Pointer clicks select
nothing.

## EXIT STATUS

Zero after a clean close, including when the settings were refused.
Non-zero when the window could not be opened, the shared frame region
was refused, or the window channel was lost; the reason is stated on
the standard error stream.

## ENVIRONMENT

`HOME` names the user's own home directory, under which
`Settings/Pinboard/pinboard.conf` is read at start-up so the window
opens on the settings that are in effect. That document is written by
the desktop session, never by this program. With no `HOME`, the window
opens on the defaults.

## SEE ALSO

`files`, `viewer`
