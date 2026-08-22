## NAME

terminal — graphical terminal emulator

## SYNOPSIS

`terminal`

## DESCRIPTION

Opens a desktop window hosting the user's default shell on an 80×25
character screen. Keystrokes typed into the focused window are sent to
the shell; everything the shell writes (standard output and standard
error alike) is interpreted through the shared ANSI/VT vocabulary and
drawn in the colour scheme chosen in the settings. The terminal itself
never echoes: echo and line editing belong to the shell, exactly as on a
console.

The window opens at whatever the 80×25 screen measures in the text size
in force, so it fits the display it is shown on; on a screen too small
for that size the text is stepped down rather than the screen narrowed,
because a program that lays itself out for 80 columns must still get
them.

The terminal is launched from the desktop's Program Library (the
taskbar's Library button) or by name from a shell. It requires a running
graphical session: without one, the window channel is unreachable and the
terminal reports the refusal on the standard error stream and exits.

The session ends when the shell exits (for example with `exit`) or when
the window is closed from the desktop; closing the window ends the shell
with end-of-file on its input.

Pressing the secondary (right) mouse button anywhere on the screen opens
the terminal's menu. Every row has a keyboard shortcut that works
whether or not the menu is open, and `Escape` — or a click away from the
menu — dismisses it without choosing.

| Row | Shortcut | What it does |
| --- | --- | --- |
| Settings… | `Ctrl ,` | Open the settings described below. |
| Larger text | `Ctrl +` | Draw the screen one step larger. |
| Smaller text | `Ctrl -` | Draw the screen one step smaller. |
| Actual size | `Ctrl 0` | Return to the default text size. |
| Clear screen | `Ctrl Shift K` | Blank the screen without writing to the shell. |
| Close | `Ctrl Shift W` | Close the window and end the shell. |

The settings open in the window itself and have two tabs. **Appearance**
chooses the colour scheme, sets the text size, and edits the user's own
scheme. The shipped schemes are *System* (which follows the desktop's
dark or light appearance), *Midnight*, *Phosphor*, *Amber*, *Ember*,
*Contrast*, *Paper*, and *Custom*. Choosing *Custom* uses the colours
edited below the chooser: a grid of the twenty colours a screen is drawn
from — the background, foreground, cursor, cursor text, and the sixteen
ANSI colours — with red, green, and blue sliders for whichever one is
selected.

**Effects** sets how the screen is drawn.

| Effect | What it does |
| --- | --- |
| Opacity | How solid the background is. Below full, the desktop shows through behind the text, which stays fully legible. |
| Backdrop blur | How far the desktop behind a see-through window is blurred. Has no effect on a fully opaque window. |
| Scan lines | Dims alternate rows, the flat part of a shadow mask's look. |
| Fuzz | A moving per-pixel noise floor, as an analogue signal has. |
| Phosphor | How long lit pixels persist, so fast-scrolling text leaves a trail. |
| Wobble | A slow travelling horizontal waver, as an out-of-time tube has. |

Every change takes effect immediately and is saved to the user's own
profile, so a later terminal opens the same way. The profile is kept by
the operating system's settings service and is private to the terminal:
no other application can read or change it. Only what the user actually
changed is stored, so *Restore defaults* removes those choices rather
than freezing today's values — a setting the administrator or a later
version of the terminal changes then applies. A setting the terminal
cannot make sense of is left at its default and reported on the standard
error stream, and a settings service that cannot be reached leaves the
terminal running on the values it ships with, again reported.

## EXIT STATUS

Zero after a clean close or the shell's own exit; non-zero when the
shell could not be hosted or the window channel, the shared frame
region, or the event mailbox was refused (the reason is stated on the
standard error stream).

## ENVIRONMENT

`HOME`
: The account's home directory, where the terminal reads and writes its
profile. Without it the terminal runs on the default profile and saves
nothing.

`TERM`
: Exported to the hosted shell as `xterm-256color`, naming the emulator
this terminal presents. Any inherited value is replaced; the rest of the
environment is forwarded to the shell unchanged.

## SEE ALSO

`elsh`, `sysinfo`
