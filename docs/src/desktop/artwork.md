# Design artwork and storyboards

The top-level `artwork/` directory (`AGENTS.md` §3) holds the **design source**
for the TAIRiX desktop and brand: concept art, an iconset reference sheet, and
UI storyboards that inform the look of the window manager, taskbar, and default
apps.

It is **reference material, not a shipped or build-time asset**. Nothing under
`artwork/` is read by the kernel, a driver, the compositor, or any build step;
removing the directory cannot break a build or a boot. The canonical, scalable
assets the OS actually renders are authored as SVG and live under
`/System/Graphics`, decoded through the curated image-decoding library in a
parser sandbox on the desktop's single rasterisation path (`AGENTS.md` §10,
§16.2; see [SVG asset decoding](./svg-assets.md), [Desktop icons](./icons.md),
and [Pointer cursors](./cursors.md)). Treat `artwork/` as the human-facing
brief these production assets are derived from, kept in sync by hand when the
look changes.

## Contents

- **`cinder-mascot1.png`, `cinder-mascot2.png`** — the project mascot, **Cinder**
  ("little guardian, core by nature"): a small red-panda character in a charcoal
  scarf. The first sheet is a concept board (about-text, palette, personality,
  pose explorations); the second is a fuller character sheet (hero pose,
  turnaround, expression set, action poses, size/silhouette study, material and
  costume notes, and the hex palette). Cinder is a steadfast, curious, reliable,
  resourceful guardian of the system core, drawn approachable and readable at
  small sizes — never aggressive or overtly heroic.

- **`tairix_iconset_sheet.svg`** — the system pictogram reference, *"Secure core.
  Fearless systems."* A 24px master-grid iconset grouped by usage: **Brand /
  System** (OS mark, Cinder, launcher, core, kernel, boot, user, lock, power),
  **Window decorations** (close, minimise, maximise, restore, fullscreen, pin,
  snap, tile, shade), **Iconbar / Apps** (search, files, terminal, browser,
  settings, editor, monitor, trash), **Menus / Navigation** (the directional and
  selection glyphs), **Task switcher / Workspaces**, and **Status /
  Connectivity** (notify, network, Wi-Fi, volume, battery, sync, warning, info).
  Its stated principles — silhouette first, familiar metaphors, low-detail 24px
  masters, orange reserved for identity/confirmation/critical attention, and
  transparent fills marking active zones — are the intent behind the production
  glyphs in `lib/icon` and `lib/cursor`.

- **`switchboard1.png`, `switchboard2.png`** (plus the `-light` variants) —
  storyboards for **Switchboard**, the system-overview panel pinned to the
  far right of the taskbar. The boards explore a glanceable overview (CPU /
  memory / disk / network), running tasks, background jobs, a plain-language
  "system pressure" view with per-task actions, activity grouping, a recovery
  view for unresponsive apps, a full system detail view, and the taskbar icon's
  states and micro-interactions. Each scene is shown in both the default dark
  theme and a light theme, matching the runtime-switchable dark/light theming
  the desktop already requires (`AGENTS.md` §10).

The `-light` / default pairing exists to validate that every storyboard reads
correctly under both themes; it is a design check, not two separate designs.

## Status

The mascot and iconset are exploratory concepts feeding the brand and the
`/System/Graphics` SVG sources. The Switchboard boards graduated from proposal
to design brief: the monitor service and its panel are implemented
(`userland/gui/switchboard`, staged by `plans/NEW-TASKBAR.md`; see
[the Switchboard page](./switchboard.md)), with the boards remaining the
visual reference their measured, honest implementation is judged against.
This page is documentation of the artwork, not a commitment to ship every
idea it depicts — where a board sketches an interface the OS does not have
(a disk/network throughput gauge, app "sleep"), the shipped panel shows the
measured truth instead.
