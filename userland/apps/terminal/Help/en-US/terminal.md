## NAME

terminal — graphical terminal emulator

## SYNOPSIS

`terminal`

## DESCRIPTION

Opens a desktop window hosting the user's default shell on an 80×24
character screen. Keystrokes typed into the focused window are sent to
the shell; everything the shell writes (standard output and standard
error alike) is interpreted through the shared ANSI/VT vocabulary and
drawn with the active theme's palette. The terminal itself never
echoes: echo and line editing belong to the shell, exactly as on a
console.

The terminal is launched from the desktop's Program Library (the
taskbar's Library button) or by name from a shell. It requires a running
graphical session: without one, the window channel is unreachable and the
terminal reports the refusal on the standard error stream and exits.

The session ends when the shell exits (for example with `exit`) or
when the window is closed from the desktop; closing the window ends
the shell with end-of-file on its input.

## EXIT STATUS

Zero after a clean close or the shell's own exit; non-zero when the
shell could not be hosted or the window channel, the shared frame
region, or the event mailbox was refused (the reason is stated on the
standard error stream).
