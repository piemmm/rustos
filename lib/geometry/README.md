# rustos-geometry

The single shared **integer screen geometry** for the RustOS desktop
(`AGENTS.md` §6, §17.4 — `PLAN.md` Stage 7): the `Point` and `Rect` types
used by the compositing window manager (`userland/gui/wm`), the taskbar
(`userland/gui/taskbar`), and the default graphical apps.

- `Point` — a signed (`i32`) screen coordinate; a window may sit partly off
  the top or left edge.
- `Rect` — an axis-aligned rectangle (a `Point` origin plus unsigned `u32`
  size) with checked `intersection`, `union`, and half-open `contains`. A
  zero-width or zero-height rectangle is *empty*, the canonical "covers
  nothing" value used by damage tracking and clipping.

All edge arithmetic widens through `i64`/`u32` so a pathological coordinate
saturates rather than wrapping — it fails closed (`AGENTS.md` §2.9).

## Why it lives in `lib/`

The GUI crates may not depend on one another (`AGENTS.md` §17.4), and code
shared by more than one crate lives in `lib/*` (§6). These coordinate types
are needed by the window manager, the taskbar, and every graphical app, so
they belong here rather than being defined once in the window manager and
duplicated elsewhere (§2.2). The crate has no dependencies and sits at the
bottom of the §17.4 layering: it is depended on, never depends. The window
manager re-exports `Point` and `Rect` from this crate, so there is exactly
one definition.

There is no rendering or compositing arithmetic here — that is the window
manager's job — keeping this crate a pure, dependency-free coordinate
vocabulary.

## Stability tier

`experimental` — the Stage 7 desktop geometry seam. It is `no_std`, performs
no allocation, and has no dependencies. No `unsafe`, and no
`unwrap`/`expect`/`panic!` in production paths (`AGENTS.md` §2.9).
