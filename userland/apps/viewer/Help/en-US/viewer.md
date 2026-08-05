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

The window is driven by the mouse. Click the "Open…" button in the
header to ask for another file. Drag the scrollbar's thumb up or down
to move through a long file, click its track above or below the thumb
to page, click its end buttons to step a line, or turn the wheel over
the window to scroll. Cancelling the picker leaves the viewer open
with a notice; closing the window from the desktop ends the viewer.

The keyboard is a secondary path for the same actions: `Enter` asks
for another file, the arrow keys step a line, Page Up/Page Down step a
page, and Home/End jump to the top or bottom.

## EXIT STATUS

Zero after a clean close; non-zero when the window channel or the
shared frame region was refused (the reason is stated on the standard
error stream).
