# tairix-icon

The shared desktop-icon library (`lib/icon`, `AGENTS.md` §6 / §10 — `PLAN.md`
Stage 7). It lives in `lib/*` so the taskbar, the desktop session, and the
file manager all draw icons without depending on one another (`AGENTS.md`
§17.4). The crate is `no_std`, `#![forbid(unsafe_code)]`, and owns no scan
converter or colour arithmetic of its own — it draws through `lib/raster`'s
one supersampled `Surface::fill_polygon` path, exactly like `lib/cursor`.

The built-in glyph representation (`VectorIcon`, `IconLayer`, `IconKind`,
`builtin_icon`, the SVG-first `IconSet`/`IconAssetSource` loader) is described
under [Desktop icons](../desktop/icons.md). This page covers the crate's
**two-tier asset model** and the `artwork` layer that resolves it.

## The two-tier asset model

Every `IconKind` resolves to drawable pixels through two on-disk asset tiers
over an always-present built-in floor, tried in order, so resolution is
**total** — a draw site always gets something:

1. **Raster artwork (preferred).** A pre-rasterised master shipped by the OS
   at `/System/Graphics/Icons/<asset-id>.png` (`icon_artwork_path(kind)`).
   Raster masters are a canonical icon source: they carry richer detail than a
   monochrome silhouette can.
2. **Vector SVG (next).** The scalable `<asset-id>.svg` source under the same
   directory (`icon_vector_path(kind)`), decoded into a `VectorIcon` through
   the SVG-first loader.
3. **Built-in glyph (always last).** `builtin_icon(kind, colour)` — the
   monochrome vector silhouette compiled into the crate. This tier can never
   be absent, so the desktop always shows a meaningful icon even with no
   on-disk assets at all (a headless or freshly-installed system), which is
   why it is the **required** fail-closed fallback for every kind
   (`AGENTS.md` §2.9).

A fine-grained file-class kind (`TextHtml`, `TextRust`, `ImagePng`,
`ImageSvg`, `DiskHard`, `DiskUsb`, …) names its own distinct raster/vector
asset id but deliberately shares its broad family's built-in glyph — text
kinds fall back to the text glyph, image kinds to the image glyph, every disk
medium to the one disk glyph. So a system that ships the artwork shows the
precise icon, and one that does not still shows a meaningful family glyph
rather than a bare placeholder.

## The `artwork` layer

The `artwork` module turns tier 1 into drawable pixels for the two processes
that need it — the desktop session and the file manager — through injected
seams, so the crate stays `no_std` and the untrusted decode never runs in this
library or in the renderer that consumes it (`AGENTS.md` §19.5):

- `ArtworkReader` reads an asset's bytes (a capability-gated filesystem read
  in production); a missing or refused asset is `None`, never fatal.
- `ArtworkRasteriser` turns encoded bytes into `side`×`side` straight-alpha
  RGBA8 (the parser sandbox in production).
- `MAX_ARTWORK_BYTES` (256 KiB) is the single fixed validation bound on an
  artwork file — a defence against hostile input, not a growable capacity
  (`AGENTS.md` §24.4). The sandboxed rasteriser (`lib/sandbox`) refuses
  over-long input against this same one definition, so the bound cannot
  diverge between the two crates (`AGENTS.md` §2.2).
- `artwork_kind_for_file(name)` accepts exactly `<asset-id>.png` for a known
  kind and refuses anything else (an unknown id, a wrong extension, an empty
  name, or a directory-bearing path-traversal attempt), so the image build can
  reject an asset the desktop could never resolve.

`ArtworkCache` retains each decode — success **or** refusal — keyed by asset
path and pixel side, over the one shared reclaimable-memory cache
(`lib/reclaim`, `plans/SMARTRAM.md`), so a crowded or crafted bundle store can
never grow a session without bound and a bad asset is not re-read every frame.
A cache miss reads the bytes, refuses **before** rasterising when they exceed
`MAX_ARTWORK_BYTES`, rasterises, verifies the reply is exactly `side`×`side`
straight-alpha RGBA8 with checked arithmetic (never a panic), and builds the
`Surface`; a zero side, an unreadable path, an over-long asset, a refused
decode, or a wrong-length reply all yield a cached `None`. Lookups return a
**borrow** into the cache, never a clone, so a grid drawing many icons a frame
reads each surface in place. `artwork_cache(label, seat, fb_bytes, pressure,
sink)` builds the cache identically for both consumers, and
`IconArtworkSource` binds a cache to its two seams so a renderer receives a
plain `IconArtwork` lookup that knows nothing about I/O. `NoArtwork` is the
all-glyph lookup a headless build or a test uses — it never resolves any
artwork, so every draw site falls back to its built-in glyph.

## Stability

Tier: `experimental`.
