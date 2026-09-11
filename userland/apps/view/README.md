# `view` — the picture and document viewer

`View.app` is the windowed viewer for pictures and documents: the app the
file manager hands a picture to, and a standalone application that asks the
session's trusted picker when launched with no document.

**It is a viewer.** It holds no write capability and has no editing, saving,
export, annotation, or printing. That is what the app is, and it is why it
needs no filesystem authority of its own.

Stability tier: **experimental** — `view.app` is new, and the page-source seam
behind it gains PDF (`lib/pdf`) before the ABI it speaks freezes.

## What it opens

JPEG, PNG, SVG, GIF, TIFF, WEBP, BMP, ICO and RISC OS Sprite. Every format is
supported *completely* — every bit depth, compression, colour space and
structural variant the format defines — because a format that can only be
half-decoded is not claimed at all. A file the decoder refuses fails closed to
a **stated reason drawn in the window** and written to `stderr`; never a blank
surface, and never a fabricated image.

PDF is deliberately **not** claimed yet: `lib/pdf` lands behind the same page
source, and claiming a format with no decoder behind it would offer the viewer
for a file it must always refuse (`plans/VIEW.md`).

## Capabilities

`CAP_CONSOLE_WRITE`, `CAP_SHM`, `CAP_PROC_SPAWN` — and **no filesystem
capability at all**. A document reaches the viewer only as the user's own act:

* a read-only descriptor the file manager had the kernel clone in at spawn, or
* a one-shot `fd_grant` the session's trusted picker delegated, which
  `fd_redeem` installs.

`CAP_PROC_SPAWN` is what lets the viewer re-enter its own binary as a
capability-empty decoder. A document is untrusted input and is never decoded
in the viewer's address space: the worker holds nothing but its two wired
pipes, so it has strictly less authority than the viewer, and a malformed or
hostile file crashes it and nothing else.

## Where the work lives

| Piece | Home |
|---|---|
| The document model, the viewport, the layout, the commands, the renderers | `src/lib.rs`, `src/layout.rs`, `src/paint.rs`, `src/view.rs` |
| The window shell, the sandbox session, the worker desk | `src/run.rs` |
| Decoding | `lib/sandbox::imagerender`, in the worker |
| Scaling and turning | `lib/raster` |
| Controls | `lib/controls` |

The engine performs **no I/O**: it records what it wants drawn and collects
what came back, so a paint draws only from state it already holds. `Run`
carries a request out on the shared worker desk (`tairix_rt::work::Worker`),
which is what keeps the window answering while a slow store or a large decode
is in progress.

## Driving it

The toolbar carries zoom out/in, fit, actual size, previous/next entry, rotate
left/right, mirror, play/pause and information, with a continuous zoom slider
at its trailing edge. Drag the canvas to pan when the picture overflows;
the wheel pans; a secondary press opens the app's menu (drawn by the session,
never by the app).

The keyboard reaches every command: `+`/`-` zoom, `0` fits, `1` is actual
size, `2` fits the width, `[`/`]` rotate, `M` mirrors, `I` toggles the
information panel, `Space` plays an animation, `O` asks for another document,
`Page Up`/`Page Down` and `Home`/`End` move through a document's entries, the
arrow keys pan, and `Escape` closes the window.

A thumbnail sidebar is **not** part of this app yet, and is deliberately
absent rather than reserved: see `plans/VIEW.md` for the design question it
turns on (the worker holds one decoded page, so a thumbnail render would
displace the page on screen).

## Tests

`cargo test -p tairix-view` covers the viewport and the zoom ladder, the
display-to-page space mapping against `Reorient`'s own position map, the
layout at several scales, the request/answer desk (including a superseded
answer being dropped and the pixel buffer being recycled), the command set,
the input routing, the animation deadlines, and that a paint is a function of
state alone.
