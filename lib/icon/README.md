# tairix-icon

Shared desktop-icon library for the TAIRiX desktop (`lib/icon`, `AGENTS.md`
§6 / §10 — `PLAN.md` Stage 7).

The desktop icons here — the taskbar's status/notification glyphs and the file
manager's file-type glyphs — are **scalable vector artwork, not fixed
bitmaps**: each `VectorIcon` is a small ordered stack of filled polygon layers
over a resolution-independent design grid, so the same glyph is

- **vectorised** — authored once as geometry, not a fixed-resolution bitmap;
- **scalable** — rasterised crisply at any pixel size (`rasterise(side)`),
  each pixel taking the exact area the artwork covers of it;
- **themeable** — every glyph is a monochrome silhouette tinted by a single
  colour the caller supplies from the active theme, so re-theming is data, not
  new code (`AGENTS.md` §10);
- **single-path** — every layer blends through `lib/raster`'s one
  `Surface::fill_polygon` path, and the stack through its one
  `Surface::layered` composition, so a shape's stroke meets its fill without a
  pale seam; the icon library owns no scan converter of its own
  (`AGENTS.md` §2.2), exactly like `lib/cursor`.

## Layout

- `vector` — `IconLayer`, `VectorIcon`: the vector representation and
  `rasterise(side) -> Surface`.
- `glyph` — `IconKind` (the closed glyph set: the taskbar's network, volume,
  battery, and bell, its program-library launcher, and the user bust behind
  its account capsule; the file manager's folder, folder-open, generic file, app-bundle,
  text, image, archive, and executable; the file manager's toolbar commands
  nav-back, nav-forward, nav-up, refresh, view-toggle, sort, new-folder,
  trash, and empty-trash; list-menu, for a screen's own section list behind a
  location breadcrumb; and a generic fallback. A fine-grained content-type or
  disk kind names its own asset id and shares its family's built-in glyph),
  `IconKind::for_asset` (theme asset id → kind, falling back to `Generic`,
  `AGENTS.md` §2.9), `IconKind::index` (its stable slot in `ICON_KINDS`),
  `builtin_icon`, and `disk_icon`.
- The drive kinds are `Disk` (the generic drive), `DiskHard`,
  `DiskSolidState`, and `DiskUsb`; they share one built-in disk glyph.
  `disk_icon(medium)` maps the `BlkDeviceClass` a mounted volume reports —
  rotational, solid-state, removable — onto its kind, and resolves a
  paravirtual device and an unknown medium alike to the generic `Disk`
  rather than guessing. The mapping lives here, beside the vocabulary, so
  every desktop consumer draws the same icon for the same medium.
- `account` — `monogram_of(name)` and `monogram_disc(mark, side, font, colours)`:
  an account's **circular identity picture**. One definition, drawn by the
  login screen's account tiles and prompt (`lib/greeter`) and by the desktop's
  own account capsule on the icon bar, so the mark a person signs in as is the
  mark they then live with (`AGENTS.md` §2.2). The disc is produced at exactly
  the side asked for, so nothing scales or crops it, and it is always a circle.
  It is the tier beneath a picture an account carries of its own; nothing sets
  one yet, so today every account resolves to its monogram, and a name that
  yields no character still gets `FALLBACK_MONOGRAM` rather than a blank
  (`AGENTS.md` §2.9).
- `svg` — `VectorIcon::from_svg` and `decode_svg(bytes)`: build an icon from a
  decoded `lib/svg` `SvgImage` (the SVG-first asset rule, `AGENTS.md` §10). A
  malformed or undecodable asset fails closed, so the caller substitutes a
  `builtin_icon` glyph rather than crashing (`AGENTS.md` §2.9).
- `load` — `IconAssetSource` and `IconSet::from_assets(source)`: build a whole
  icon *set* from on-disk SVG assets (one per `IconKind`, served through the
  injected seam so the `/System/Graphics` read and its capability stay in
  userland, `AGENTS.md` §17.4 / §19.5). `IconSet::icon(kind, tint)` is total:
  a kind that loaded an authored SVG asset keeps its own colours, and a kind
  whose asset is missing, malformed, or undecodable falls back to the
  `builtin_icon` glyph tinted with `tint` (`AGENTS.md` §2.9). `ICON_KINDS` is
  the closed kind list a loader iterates, and `IconSet` stores one slot per
  kind (indexed by `IconKind::index`), so adding a kind is a new `ICON_KINDS`
  entry, never a new field (`AGENTS.md` §2.2). `IconSet::builtin()` (also
  `Default`) is the all-fallback set the desktop draws before any asset
  loads, so a complete icon set always exists.
- `artwork` — the shared "resolve an icon to drawable pixels" layer, used by
  both the desktop session and the file manager. It adds the preferred tiers
  over the built-in glyph: the icon a thing carries of its **own**, then its
  class's shipped master — **raster** first, **vector** next. `ArtworkReader` (a
  capability-gated read) and `ArtworkRasteriser` (the parser sandbox) are
  injected seams, so the crate stays `no_std` and the untrusted decode never
  runs here (`AGENTS.md` §19.5). A draw site states what it wants as one
  `IconRequest` — `kind` (the class alone), `asset` (an icon path the caller
  already resolved), or `bundle` (an application bundle whose own signed
  manifest names its icon) — and this layer owns the order they are tried in,
  so no surface re-decides it. `icon_artwork_path`/`icon_vector_path` name a
  kind's `/System/Graphics/Icons/<id>.png` / `.svg` asset;
  `artwork_kind_for_file` validates a shipped-artwork file name in either
  class format;
  `MAX_ARTWORK_BYTES`, `MAX_ARTWORK_SIDE`, and `MIN_ARTWORK_SIDE` are the one
  definition of the artwork bounds (the sandboxed rasteriser decodes within
  them and the image build refuses first-party artwork that fails them,
  `AGENTS.md` §2.2 / §24.4). `ArtworkCache` (built by `artwork_cache`, over
  `lib/reclaim`) retains each decode — success or refusal — keyed by what was
  resolved and the pixel side, returning a **borrow** so a grid draws many
  icons a frame without copying; a bad, absent, oversize, or wrong-shaped
  asset yields a cached `None` (`AGENTS.md` §2.9). `IconArtworkSource` hands a
  renderer a plain `IconArtwork` lookup, and `NoArtwork` is the all-glyph
  lookup a headless build or a test uses.
- `ArtworkResolver` is the seam between deciding what a draw needs and
  producing it, because a read plus a sandbox round trip must never happen
  inside a paint. `InlineArtwork` reads and decodes on the calling thread; a
  caller that defers answers `Resolved::Pending` until the pixels land, so the
  draw takes the tier below (ultimately the built-in glyph) and the same lookup
  serves the artwork afterwards. Both go through `render_artwork`, so where the
  decode ran cannot change what it produced. A caller that *stores* the picture
  rather than drawing from the cache asks `owned_artwork` and gets an
  `ArtworkOutcome`, which tells `Pending` (ask again) from `Refused` (do not).
  `ArtworkCache::prefetch` is the other half: a caller that knows what it is
  about to draw starts the decode then, so the frame that needs it never waits
  — the difference between a launcher opening on its applications' own icons and
  opening on glyphs it replaces one round trip at a time.
- `desk` — `ArtworkDesk`, the deferring `ArtworkResolver`: what has been asked
  for, what a producer is running, and what has come back. It holds no lock,
  thread, or syscall, so its whole policy is host-tested, and it lives here
  beside the contract it implements because two processes drive it — the
  desktop session from a worker thread behind the runtime's futex mutex
  (`plans/FIX-DESKTOP.md` DESK-8), the file manager from its own event loop
  (`plans/NEW-FILEMANAGER.md`), and `userland/apps/*` may not depend on
  `userland/gui/*` (`AGENTS.md` §17.4). An answer handed over is forgotten —
  the cache owns it, so a later miss is a genuine one and is produced again.
  A decode the cache *refused* is held as declined instead, which is what stops
  a landing chasing its own tail; `retry_declined` re-offers it on the pressure
  band's own wake.

## Asset model

An icon resolves through the thing's own icon (an application bundle's
`Resources/` master named by its manifest, or an account's own identity disc)
and then two on-disk class tiers
over an always-present built-in floor: raster artwork (`<id>.png`) preferred,
vector SVG (`<id>.svg`) next, and the built-in `builtin_icon` glyph always
last, so resolution is total even on a system that ships no artwork at all
(`AGENTS.md` §2.9). A fine-grained file-class kind (an HTML or Rust text file,
a PNG or SVG image, a specific disk medium) names its own asset id but shares
its broad family's built-in glyph, so a system without the class artwork
still shows a meaningful icon.

A kind ships **one** class master, in whichever format suits the artwork: the
folders are vector (`folder.svg`, `folder-filled.svg`), the illustrative
file-class, application and disk pictures are raster masters. Shipping one id in both formats is a packaging defect the
image build refuses, since the raster tier would always win and the vector
could never be selected.

A bundle's manifest is authored by whoever built the bundle, so the bundle
tier treats it as untrusted input: the manifest is read under the ABI's wire
bound and decoded fail-closed, and the asset name is accepted only as a plain
file name resolved *inside* the bundle's own directory — a bundle cannot aim
the desktop at a file elsewhere, and one that tries simply draws its class
picture.

## Where it sits

Like `lib/geometry`, `lib/theme`, `lib/raster`, `lib/font`, and `lib/cursor`,
this crate lives in `lib/*` so the taskbar consumes it without the taskbar and
the window manager depending on one another (`AGENTS.md` §17.4). It is
`no_std`, `#![forbid(unsafe_code)]`, and owns no colour arithmetic of its own.
It draws text through `lib/font` — an account disc bears its monogram — rather
than a glyph path of its own.

The taskbar's renderer holds an `IconSet` — the built-in set until
`set_icons` installs one decoded from the on-disk `/System/Graphics` assets —
resolves a notification icon's asset id to an `IconKind`, takes that kind's
`VectorIcon` from the set (a loaded asset's own colours, or the built-in glyph
in the theme's foreground colour), rasterises it to the notification slot's
pixel size, and composites it onto the bar.

## Stability

Tier: `experimental`.
