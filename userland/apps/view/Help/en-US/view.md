## NAME

view — graphical picture and document viewer

## SYNOPSIS

`view`

## DESCRIPTION

Opens a desktop window showing a picture or a document. Launched with a
document — from the file manager, or by opening a picture — it shows that
file; launched on its own it asks the desktop session's trusted file picker
to choose one.

The viewer holds no filesystem capability: it cannot open, list, or read
anything on its own. The session browses on the viewer's behalf under its
own identity, and only the file the user chooses is delegated to the viewer,
one-shot and read-only. The file is never decoded inside the viewer: its
bytes are streamed to a separate worker process that holds no filesystem
reach at all, so a malformed or hostile file cannot reach anything the
viewer can.

Supported formats are JPEG, PNG, SVG, GIF, TIFF, WEBP, BMP, ICO and RISC OS
Sprite. A file the decoder refuses states its reason in the window and on the
standard error stream; the window is never left blank and no image is ever
fabricated.

Several documents at once are separate viewers, so comparing two pictures
side by side is opening the second one; closing a window ends its viewer.

The toolbar across the top carries, in order: zoom out, zoom in, fit in
window, actual size, previous entry, next entry, rotate left, rotate right,
mirror, play or pause an animation, and the information panel. A continuous
zoom slider sits at its trailing edge. The status line along the bottom
states the document's name, format, pixel size, which entry of it is shown,
its length, and the magnification.

Drag the picture to move about inside it when it is larger than the window;
scrollbars appear along the canvas edges while it is. Turn the wheel over the
picture to pan. A secondary press on the picture opens the viewer's menu,
which the desktop session draws.

Transparency is shown against a checkerboard, so a transparent picture reads
as transparent rather than as the colour behind it.

* `+` — magnify to the next step
* `-` — reduce to the previous step
* `0` — fit the whole picture in the window
* `1` — actual size, one picture pixel per screen pixel
* `2` — fit the picture's width
* `[` / `]` — rotate a quarter turn left or right
* `M` — mirror left to right
* `I` — show or hide the information panel
* `Space` — play or pause an animation
* `O` — choose another document
* `Page Up` / `Page Down` — the previous or next entry
* `Home` / `End` — the first or last entry
* arrow keys — move about inside the picture
* `Escape` — close the window

## OPTIONS

`-h`, `-?`, `--help`
: Write this help to the standard output and exit.

## EXIT STATUS

Zero after a clean close. Non-zero when the window channel, the shared frame
region, or the desktop session was refused; the reason is stated on the
standard error stream.
