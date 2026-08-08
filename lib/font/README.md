# tairix-font

The single shared **text rasterisation** primitives for TAIRiX (`AGENTS.md`
§6, §16.4, §17.4 — `PLAN.md` Stage 7).

Font rendering is one of the curated OS shared-library classes (`AGENTS.md`
§16.4). Like `lib/geometry` (coordinate types), `lib/theme` (design tokens),
and `lib/raster` (the pixel surface), text rendering lives in `lib/*` so the
framebuffer console (`lib/fbcon`), the taskbar (`userland/gui/taskbar`), and
the default apps can draw text without depending on the window manager and
without duplicating a blitter (`AGENTS.md` §17.4, §2.2).

## The built-in family

The system family keeps **Inconsolata EX** as its primary face for Latin,
Greek, and Cyrillic, uses **M PLUS 1 Code Regular** as its Japanese companion,
**D2Coding Regular** as its Korean companion, and **Noto Sans Hebrew
ExtraCondensed** for Hebrew and Yiddish. All four are licensed under SIL Open
Font License 1.1. The TrueType sources and licence notices are committed under
`assets/`. The text console draws from the pre-generated atlas and never parses
TrueType; the desktop's resized text rasterises glyphs from these same outlines
at runtime through the shared `lib/fontface` engine (see *Rendering at a chosen
size*). Faces have precedence in that order, so each companion fills only
codepoints the earlier faces do not map and existing glyphs remain unchanged.

The M PLUS source is the static Regular TTF from upstream commit
`4bf69824e45a175b9121b248c46abff103569051`, SHA-256
`c5b8c7a2dc8fe8430afa741e3525032b4878c77bc1220be5ab22bf6f21ddb405`.
Copyright 2021 The M+ FONTS Project Authors
(<https://github.com/coz-m/MPLUS_FONTS>).
The D2Coding source is the unmodified Regular TTF from official release 1.3.2,
SHA-256
`8b1b23e5de4dff652fb0b938528150d2f531edfda281d3944618b655711aba84`.
Copyright 2015 NAVER Corporation
(<https://github.com/naver/d2codingfont/releases/tag/VER1.3.2>); its upstream
licence notice is `assets/D2Coding-OFL.txt`.
The Noto Sans Hebrew source is the static ExtraCondensed Regular TTF from
upstream commit `a8c864f84fa0967d319b70a56d62f417d3142c67`, SHA-256
`cb46b5153a5fb971b8b1a63c390d20521acf8f659f603c391d8f262459e5b8c2`.
Copyright 2019 The Noto Project Authors
(<https://github.com/notofonts/noto-sans-hebrew>); its upstream licence notice
is `assets/NotoSansHebrew-OFL.txt`.
`cargo xtask font-atlas --write` rasterises the merged repertoire into
the generated atlas (`src/atlas.rs` + `src/atlas_coverage.bin`), and
`cargo xtask font-atlas` (run by `ci`) fails closed if the committed atlas
drifts from a fresh generation — the same generated-view discipline as the C
ABI headers. The generator is first-party and deterministic: it rasterises
each outline through the shared `lib/fontface` engine (a minimal TrueType
reader and an anti-aliasing scanline rasteriser) — the *same* engine the
runtime uses to resize glyphs, so the atlas and live text can never diverge
(`AGENTS.md` §2.2). The 3.54 MiB coverage payload starts
with a little-endian glyph-offset table and stores each glyph as an independent
bounded LZ block. Lookup therefore decodes exactly one glyph into its fixed
420-byte value, with no allocation or whole-atlas startup pass. Exact
round-trip and malformed-stream tests cover the codec, and generation fails if
the payload exceeds the pre-Korean size ceiling.

## What this crate owns

- `atlas` — the generated data: a 15×28-pixel terminal cell (a 25 px em),
  two-cell-capable glyph bitmaps for Japanese and Korean full-width outlines,
  losslessly compressed 4-bit coverage, a sorted codepoint→glyph range table,
  and the U+FFFD fallback index. Pure `const`/`static` data with no
  dependencies.
- `glyph` — Unicode lookup over the atlas: binary search of the range table,
  bounded single-glyph decompression, the packed-nibble accessor, and the
  U+FFFD fallback for any scalar the face does not map (visibly wrong rather
  than silently dropped, `AGENTS.md` §2.9). Coverage spans the merged
  20,209-glyph repertoire: Latin and its extensions, Greek, Cyrillic (including
  the full Ukrainian alphabet), box drawing and block elements, arrows,
  punctuation, currency, hiragana, katakana, Japanese kanji, all 11,172
  precomposed Hangul syllables and 94 compatibility jamo, plus Hebrew and
  Yiddish letters, punctuation, and marks.
- `font::BitmapFont` — the face's metrics (cell size, pen advance, line
  height) plus the glyph blitter. `draw_text` composites each covered pixel
  onto a `lib/raster` `Surface` through that crate's single
  premultiplied-alpha `Pixel::over` path, scaling the text colour once into a
  256-entry coverage table — anti-aliased edges and translucent text both
  blend correctly with no colour arithmetic duplicated here (`AGENTS.md`
  §2.2). Both of a glyph's axes are clipped against the surface once, before
  any pixel is touched, and each visible row then blends its coverage bytes
  against the destination row slice (`Surface::row_span_mut`) in step, so a bounds
  check and a row address are paid per row rather than per pixel and a glyph
  off the edge clips instead of being tested pixel by pixel. `text_width` and
  `truncate_to_width` give the shared layout arithmetic.
- `font::BitmapFont::elide_to_width` / `font::BitmapFont::wrap_to_width` —
  the two shared fitters over that arithmetic, so no text region writes its
  own break loop (`AGENTS.md` §2.2). The first returns the longest prefix
  that fits *once room for `ELLIPSIS` is reserved* and whether the mark is
  needed; a box too narrow for even the mark draws nothing, a mark that
  spills out of the box it exists to enforce being worse than an empty box.
  The second lays a label over at most *n* lines — breaking at whitespace so
  a word starts the next line rather than being split, breaking an
  over-long word mid-word on a `char` boundary (a run that cannot break must
  still advance), drawing no surrounding whitespace, and eliding only the
  last line. It is a **lazy iterator** of `TextLine { text, elided }`
  borrowing the label, never a `Vec`: a caller counts a clone to size the
  block and walks the original to draw, so a label re-laid out every repaint
  allocates nothing.
- `font::ELLIPSIS` — the one definition of the mark a shortened line ends
  with, measured and drawn through the same constant so the two can never
  disagree.
- `client` — the process-global font-service client behind `render`: the
  injected transport seam to `fontd` and the byte-budgeted local glyph cache
  fetched coverage is memoised in. See *Rendering at a chosen size* below.
- `glyph_cache` — the one cached-glyph declaration (behind `glyph-cache`,
  pulled in by `render`): the retained value type, its reclaim
  classification, and the RAM-derived byte budget. `fontd` builds its own
  service-side cache from this same declaration, so the two sides of
  `FONT_ENDPOINT` cannot drift apart (`AGENTS.md` §2.2).

## Rendering at a chosen size

A `BitmapFont` renders at a chosen **cell height in physical pixels**.
`BitmapFont::inconsolata()` keeps the atlas's native height and is what the
text console (`lib/fbcon`) draws at — its glyphs come straight from the atlas
with no resampling, so console rendering is byte-for-byte unchanged.
`BitmapFont::with_pixel_height(px)` asks for any other cell: the desktop
resolves a comfortable physical size from the theme's logical font size and
the DPI scale (`tairix_geometry::Scale`), so window titles, the taskbar, the
program-library popup, and the file browser render at that size. Every derived metric
(advance, cell width, baseline, line height) scales with the cell height,
keeping the font monospaced and its aspect ratio fixed.

A non-native cell is rasterised **directly from the TrueType outline** at that
exact size — but by `fontd`, not here: this crate parses no TrueType and holds
no face. Sampling the curve at the target resolution keeps text crisp whether
tiny or very large, so a 200-pixel heading is as sharp as 14-pixel body text
and neither is a stretched bitmap. A scalar the faces do not cover falls back
to the same U+FFFD glyph the atlas shows, and an unreachable or refusing
service composites nothing rather than reaching for local font data (fail
closed).

Because the desktop redraws the same glyphs at the same size every frame, each
reply is memoised per `(scalar, cell height, weight)` in a byte-budgeted
`tairix_reclaim::ReclaimCache`, so a steady-state redraw issues no IPC. The
bound is **bytes, derived from the machine's total RAM** (a small fraction of
it — a glyph working set is a few hundred bitmaps), never a hand-picked entry
count: a small board and a large server each get a cache proportioned to what
they have, and a caller rendering at ever more sizes evicts the least recently
used entries instead of growing without limit. The cache shrinks on the
system's memory-pressure bands like every other reclaimable cache, and
overwrites each released entry — the set of cached glyphs reveals which
characters the user has had displayed.

The cache is installed, not constructed in place: sizing it needs the
machine's RAM figure and governing it needs the process's pressure gauge and
audit sink. A program that links `tairix-font/rt` gets one lazily on its first
draw and needs no setup; a host test installs its own through
`set_glyph_cache`, the same seam `set_font_transport` uses. Until one is
installed every glyph is fetched and served without being retained — correct,
merely one call per glyph. A RAM reading the System Information service cannot
supply is zero, which yields a zero budget and exactly that uncached
behaviour, never a guessed ceiling. The client and its cache ride the `render`
feature; the allocator-free `atlas`/`glyph` view never touches them.

The cell model is **one scalar per grid entry** — the deliberate simplification
`lib/vt` and `lib/curses` document. A zero-advance combining mark renders in
its own cell. `tairix_vt::char_width` remains the one layout rule: a wide glyph
is stored as a lead plus continuation cell, while its atlas bitmap may paint
across both cells.

The `atlas` and `glyph` modules are allocator-free, so a consumer that brings
its own blitter (`lib/fbcon`, which blends coverage into device-coherent
memory itself) depends with `default-features = false`; the
`lib/raster`-backed blitter rides the default-on `render` cargo feature — one
font definition either way (§2.2).

There is no installed-font machinery yet: a `tairix-theme` font role selects a
font by family name under `/System/Fonts`, but no faces are installed, so
everything draws with the built-in `BitmapFont::inconsolata` face. When
installed faces arrive they extend this crate; consumers keep calling
`draw_text`.

## Why it lives in `lib/`

Sibling userland GUI crates may not depend on one another (`AGENTS.md`
§17.4), and the kernel's boot console must not depend on userland, so the
text rasteriser they share belongs in `lib/*`. It depends only on
`lib/raster` (behind `render`) and is depended on by `lib/fbcon` and the GUI
crates, never the reverse — `Layer::Lib` in the §17.4 layering.

## Stability tier

`experimental` — consumed by `lib/fbcon` (every arch port's display console),
`userland/gui/taskbar`, and the default apps. It is `no_std`, contains no
`unsafe`, and follows the shared workspace lints.
