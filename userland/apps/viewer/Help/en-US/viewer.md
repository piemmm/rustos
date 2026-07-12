## NAME

viewer — graphical read-only file viewer

## SYNOPSIS

`viewer`

## DESCRIPTION

Opens a desktop window and immediately asks the desktop session's
trusted file picker to choose a file. The viewer itself holds no
filesystem capability: it cannot open, list, or read anything on its
own. The session browses on the viewer's behalf under its own
identity, and only the one file the user chooses is delegated to the
viewer — one-shot and read-only.

The chosen file's content is shown as plain text from the top of the
window. Printable characters are shown as they are; every other byte
is shown as a dot, so binary content reads as obviously sanitised. The
shown content is bounded to the first part of the file.

Press `Enter` to ask for another file. Cancelling the picker leaves
the viewer open with a notice. Closing the window from the desktop
ends the viewer.

## EXIT STATUS

Zero after a clean close; non-zero when the window channel or the
shared frame region was refused (the reason is stated on the standard
error stream).
