## NAME

wallpaper — graphical desktop backdrop chooser

## SYNOPSIS

`wallpaper`

## DESCRIPTION

Opens a desktop window offering the wallpapers the system ships, the
backdrop colour behind them, and how the desktop arranges the icons on
its pinboard. Nothing changes on screen until the settings are applied.

The window is driven by the mouse. A large preview at the top shows the
selected wallpaper as the desktop will draw it, with the chosen backdrop
colour wherever the image does not reach. Beneath it, the gallery lists
every shipped wallpaper as a tile: click one to select it and the
preview follows immediately. The **No wallpaper** tile, always first,
shows the chosen backdrop colour alone.

The gallery scrolls when it holds more tiles than the window shows. Turn
the wheel anywhere over the window, drag the scrollbar's thumb at the
trailing edge, or click the track above or below the thumb to move a
page at a time.

Beside the preview are four settings, each a drop-down list. Click one
to open it and click a choice to take it:

- **Fit** — how the image is placed: `fill` (cover the screen, cropping
  the overflow), `fit` (contain it whole, backdrop colour in the bars),
  `stretch` (distort to the exact screen size), `centre` (native size,
  centred), and `tile` (repeat from the top-left).
- **Backdrop** — the flat colour shown wherever the wallpaper does not
  reach: `Theme` follows the active desktop theme, and the named
  colours are fixed. A colour already in effect that is not one of the
  named ones is offered under its own `rrggbb` spelling.
- **Icons** — the corner of the pinboard the desktop icon grid grows
  from.
- **Sort** — the order the desktop folder's icons are listed in.

The preview shows the selected image, backdrop and fit at the preview's
own shape. A screen of a different shape crops or letterboxes
differently, so the preview is a faithful view of the picture and of the
fit rule, not a scale model of the display.

Wallpaper images are never decoded by this program. Each one is rendered
by a separate sandboxed process that holds no filesystem, network, or
spawn authority, so a malformed image cannot compromise the chooser or
the desktop. A file that cannot be decoded is marked `unreadable` in its
tile and is not attempted again.

The keyboard reaches everything the mouse does. `Tab` and `Shift-Tab`
move focus forward and back through the gallery, the four settings, and
the two buttons. The arrow keys move within the gallery, or open the
focused setting's list and move within it. `Enter` applies, or activates
the focused button, and `Escape` closes the window without applying.

Applying sends the chosen settings to the desktop session, which decides
whether to adopt them, redraws the pinboard, and saves them for the next
login. This program never writes the settings itself. The result is
reported beside the buttons: applied, refused with the session's reason,
or no desktop session listening. A refusal leaves the window open with
the choices intact.

Only the shipped wallpaper store is offered; an image elsewhere on the
system cannot be chosen from this window.

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
