# `tairix-viewer` — read-only file viewer

Stage 7 deliverable (`PLAN.md` Stage 7, `plans/APPWIN.md` AW5). The
windowed read-only text viewer — and the first consumer of the desktop
session's trusted file picker and the CU6 one-shot file delegation
(`plans/CAPABILITY_USE.md`). Installed as a `.app` bundle in the system
app store (`AGENTS.md` §16.2/§16.5).

## The capability story (why this app exists)

The viewer's manifest requests **no filesystem capability**: on its own
it can open, list, and stat nothing. At startup it asks the session to
run its trusted picker (`WindowRequest::PickFile`); the user browses in
the *session's* UI under the *session's* authority, and the viewer
receives exactly one conclusion on its authenticated event channel — a
`FilePicked` carrying a one-shot `fd_grant` handle, or a
`PickCancelled`. Redeeming the handle (`fd_redeem`) installs a
read-only descriptor whose reads the kernel authorises under the
session's captured identity, so the viewer reads exactly the one file
the user chose and nothing else: the user-mediated file capability of
`AGENTS.md` §16.5, exercised end to end by a shipping app.

## What this crate is

The host-tested view engine plus the `Run` binary that composes it:

- `content_lines` — the pure, bounded byte→line model: at most the
  visible rows/columns, every non-printable byte sanitised to a
  placeholder, so untrusted file content never reaches the renderer
  raw (fail closed);
- `render_status` / `render_lines` — the themed painters over the
  shared `lib/font` face and `lib/raster` surface;
- `src/run.rs` — the freestanding program: window create over
  `lib/window`, an immediate `pick_file`, the parked event wait, the
  delegated bounded read (`fd_redeem` + `fs_read`, capped at
  `CONTENT_MAX`), and the repaint. `Enter` asks for another pick; a
  cancelled pick leaves the viewer open with a notice; every bring-up
  refusal exits fail-loud with a stated reason on `stderr`.

## Capabilities

`CAP_CONSOLE_WRITE` (fail-loud diagnostics) and `CAP_SHM` (the granted
window frame region) — and deliberately nothing else. See
`AppInfo.toml`.

## Test surface

`cargo test -p tairix-viewer`: the line model's row/column bounds and
line-feed splitting, the non-printable sanitiser, the window-sized
status/content renders, and the non-degenerate view geometry.
