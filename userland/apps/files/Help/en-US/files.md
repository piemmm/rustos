## NAME

files — graphical filesystem browser

## SYNOPSIS

`files [--desktop] [directory] [-h | -?]`

## DESCRIPTION

Opens a desktop window listing the filesystem, starting at the
`directory` named on the command line, or at the launching user's home
directory when none is named. The top row shows the current
directory's path; the rows below it
list the directory's entries, the selected entry highlighted with the
active theme's accent colour. Every directory read is an ordinary
permission-checked listing under the launching user's identity: an
unreadable directory is refused, never guessed at.

The desktop starts the browser for you and keeps it on the icon bar: its
slot's menu lists your own places and whatever is mounted, and choosing one
opens a window there. A click on the slot opens one at your home directory.
That copy has no *Quit* row — it is part of the desktop, and closing its
windows simply puts it away.

Run by name from a shell (or opened on a folder from the desktop) it is an
ordinary application instead: one window, and it ends when you close it.
Either way it requires a running graphical session: without one, the window
channel is unreachable and the browser reports the refusal on the standard
error stream and exits.

The window is driven with the keyboard: `Down` and `Up` move the
selection, `Enter` opens the selected directory, and `Backspace` goes
up to the parent directory. `F5` re-reads both the listing and the places
rail, which is how a newly attached volume appears.

`Alt+Enter` opens a *Properties* window on the selected item, as does the
right-click menu's *Properties* row. It is a window of its own, so several can
be open at once and the listing stays usable while they are: it shows what the
item is, its size, its timestamps, where an alias points, its permissions and
owner, and the extended attributes the volume stores for it. Permissions,
owner, and attributes can be changed there, each as an ordinary
permission-checked write under your own identity — a refusal says why and
changes nothing. Reassigning an owner needs the `CAP_FS_CHOWN` capability; a
session without it sees the owner and group marked with a lock, and a line
saying why.

`Left` and `Right` move between the window's sections. On *Permissions*,
`Down` or `Tab` moves into its controls: the arrow keys move between them,
`Space` toggles a permission or opens the owner or group for editing, and
`Tab` or `Escape` returns to the sections.

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

- `--desktop` — run as the desktop's own file-manager component: a
  permanent icon-bar slot offering your places and the mounted volumes,
  no window until one is asked for, and no way to quit. The desktop
  session passes this at bring-up; naming a `directory` alongside it is
  refused, because a component opens no window to put one in.
- `-h, -?` — show this command's own short help and exit.

## EXIT STATUS

Zero after a clean close, or after the short help was shown; `2` when
the command line was not understood; otherwise non-zero when the window
channel, the shared frame region, or the initial directory listing was
refused (the reason is stated on the standard error stream).
