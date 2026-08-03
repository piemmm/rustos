## NAME

files — graphical filesystem browser

## SYNOPSIS

`files [directory] [-h | -?]`

## DESCRIPTION

Opens a desktop window listing the filesystem, starting at the
`directory` named on the command line, or at the launching user's home
directory when none is named. The top row shows the current
directory's path; the rows below it
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

The `directory` operand is treated as untrusted input: it must be an
absolute path within the system's path length limit, and each of its
components must be a real directory name — `.` and `..` are not, so a
spelling can never mean somewhere other than it reads as. A directory
that fails any of those rules, or that the launching user cannot list,
is refused with the reason on the standard error stream and the window
opens at the home directory instead, so a bad argument never leaves the
user with no window. A second operand is refused outright rather than
ignored.

## OPTIONS

- `-h, -?` — show this command's own short help and exit.

## EXIT STATUS

Zero after a clean close, or after the short help was shown; `2` when
the command line was not understood; otherwise non-zero when the window
channel, the shared frame region, or the initial directory listing was
refused (the reason is stated on the standard error stream).
