## NAME

files — graphical filesystem browser

## SYNOPSIS

`files`

## DESCRIPTION

Opens a desktop window listing the filesystem, starting at the root
view. The top row shows the current directory's path; the rows below it
list the directory's entries, the selected entry highlighted with the
active theme's accent colour. Every directory read is an ordinary
permission-checked listing under the launching user's identity: an
unreadable directory is refused, never guessed at.

The browser is launched from the taskbar's permanent Files button or by
name from a shell. It requires a running graphical
session: without one, the window channel is unreachable and the browser
reports the refusal on the standard error stream and exits.

The window is driven with the keyboard: `Down` and `Up` move the
selection, `Enter` opens the selected directory, and `Backspace` goes
up to the parent directory. Closing the window from the desktop ends
the browser.

## EXIT STATUS

Zero after a clean close; non-zero when the window channel, the shared
frame region, or the initial directory listing was refused (the reason
is stated on the standard error stream).
