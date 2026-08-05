# `tairix-wallpaper-chooser` — desktop backdrop chooser

`plans/PINBOARD.md` deliverable P9. The windowed chooser for the desktop
pinboard: the wallpaper, its fit, the backdrop colour behind it, and the
`Desktop` folder's icon flow and sort order. Installed as a `.app` bundle
in the system application store, so it is launchable from the desktop and
typeable by name.

## What it is, and what it is not

The chooser **asks**; the desktop session **decides**. It never writes the
settings store, never touches the framebuffer, and never applies a change
itself: Apply renders the settings document and posts it to the session's
pinboard endpoint, which validates it, adopts it, redraws the pinboard, and
persists it. The reply — adopted, refused with the session's reason, or no
session listening — is reported on the status line; a refusal leaves the
window open and never fabricates success.

Wallpaper images are **never decoded in this program's address space**.
The preview panel and every gallery thumbnail are rendered by
`lib/sandbox`'s image-render service running in a capability-empty child
this same binary is re-entered as. A file the worker refuses is marked
`unreadable` and is not asked for again — one attempt per bad file, and a
malformed image cannot take down the chooser.

## Pointer first

The window is driven by the mouse, with the keyboard as a complete
secondary path. Every interactive thing on screen is a shared
`lib/controls` control held for the life of the window — the four settings
drop-downs, the Apply and Close buttons, the gallery's scrollbar — so each
owns its own hover, press and drag state and the app inherits the whole
desktop's interaction vocabulary rather than inventing one. The gallery's
tiles are the exception the design language names: a tile paints state and
never dispatches, so the gallery hit-tests the pointer against the very
grid it painted (`lib/browse`'s shared icon-grid engine, the same one the
file manager and the desktop's own icon field use).

## What this crate is

The host-tested engine plus the `Run` binary that composes it:

- `Chooser` — the model: the candidate gallery (the "no wallpaper" entry,
  the discovered store, and a current wallpaper from outside the store),
  the four settings drop-downs, the live preview, the pointer's hover and
  press state, and the keyboard focus order. No I/O, no authority: the
  preview and every thumbnail arrive already rendered or already refused
  from the caller. The preview is held together with the request that
  produced it, so a stale preview is unrepresentable rather than merely
  avoided;
- `Layout` — the one window geometry every paint and hit-test agrees on,
  derived from the theme's metrics and the text face (never a pixel
  constant), with the footer claimed from the bottom edge so the buttons
  survive any window size;
- `Chooser::render` — the painter over `lib/raster`, `lib/font` and the
  `lib/controls` family, drawing each control in the state that control is
  already in, with the gallery clipped to its own region and the expanded
  drop-down's list painted last;
- `Chooser::settings_document` — the exact `lib/wallpaper` document the
  current UI state means, rendered by that crate's own writer;
- `src/run.rs` — the freestanding program: the sandbox-worker role, the
  store listing, the current-settings read, the window bring-up over
  `lib/window`, the parked event wait, the preview renders, the apply call,
  and the resize path. Every bring-up refusal exits fail-loud with a
  reserved code and a stated reason on `stderr`.

## Capabilities

`AppInfo.toml` requests four, each with a live use:

- `CAP_CONSOLE_WRITE` — the fail-loud `stderr` diagnostics;
- `CAP_FS_ACCESS` — reading the read-only shipped wallpaper store and the
  user's own settings document, under the launching user's identity;
- `CAP_SHM` — the window frame region granted to the session;
- `CAP_PROC_SPAWN` — starting its own sandbox worker for the previews.

Deliberately nothing else: no mount, no network, no driver, no user
administration.

## Limitations

- Only the shipped wallpaper store is offered. Choosing an image elsewhere
  on the system would need the session's trusted picker to report the
  chosen *path* (its conclusion carries only a one-shot, owner-bound
  descriptor handle) and the pinboard apply to accept a delegated read
  handle the session can use later; neither exists, so the capability is
  absent rather than half-built. See `plans/PINBOARD.md` §8.
- The preview shows the chosen image, backdrop and fit at the preview's own
  shape, not at the display's: no unprivileged program can ask how large
  the screen is, so a screen of a different shape crops or letterboxes
  differently from the preview. See `plans/PINBOARD.md` §8.
- The backdrop colour is a fixed named palette plus whatever colour is
  already in effect, not a free-form colour entry.

## Test surface

`cargo test -p tairix-wallpaper-chooser`: the candidate model (the "no
wallpaper" entry, an out-of-store current wallpaper, refused and pending
thumbnails); the pointer — click-to-select, a press released away from
what it started on doing nothing, hover and press changing what is drawn,
a drop-down opening and a row in it taking effect, an open list swallowing
the click beneath it, the wheel and a thumb drag scrolling the gallery,
and a secondary click changing nothing; the preview being re-asked for
exactly when the selection, the fit or the panel size changes and never
showing the previous one; the keyboard's full tab order and every
movement; the rendered document matching the state the controls are in;
the apply-outcome surfaces; and the layout's non-overlap and containment
at degenerate, small and large window sizes.

No pointer test hard-codes a coordinate: each asks the layout where the
thing it is about to click is, so the geometry and the tests cannot drift
apart.
