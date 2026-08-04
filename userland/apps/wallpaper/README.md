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
Every thumbnail is rendered by `lib/sandbox`'s image-render service running
in a capability-empty child this same binary is re-entered as, at the
currently selected fit, so a preview shows what the desktop will actually
do with that image. A file the worker refuses becomes a marked placeholder
tile and is not asked for again — one attempt per bad file, and a malformed
image cannot take down the chooser.

## What this crate is

The host-tested engine plus the `Run` binary that composes it:

- `Chooser` — the model: the candidate list (the "no wallpaper" entry, the
  discovered store, and a current wallpaper from outside the store), the
  thumbnail lifecycle, the fit / backdrop / icon-flow / sort choices, and
  the keyboard focus state machine. No I/O, no authority: every thumbnail
  arrives already rendered or already refused from the caller;
- `Layout` — the one window geometry every render and hit-test agrees on,
  computed bottom-up so a resize can never place a row outside the window;
- `Chooser::render` — the themed painters over `lib/raster`, `lib/font`,
  and the `lib/controls` radio/button family (no new control family), with
  the grid and each option row clipped to their own region;
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
- The window is keyboard-driven; pointer events arrive and select nothing,
  matching the file viewer's scope.
- The backdrop colour is a fixed named palette plus whatever colour is
  already in effect, not a free-form colour entry.

## Test surface

`cargo test -p tairix-wallpaper-chooser`: the candidate model (the "no
wallpaper" entry, an out-of-store current wallpaper, refused and pending
thumbnails), every keyboard movement and the full tab order, each option
group including the backdrop palette, the rendered document matching the
UI state exactly, the apply-outcome surfaces, and the layout's
non-overlap/containment at degenerate and small window sizes.
