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
  16-entry coverage table — anti-aliased edges and translucent text both
  blend correctly with no colour arithmetic duplicated here (`AGENTS.md`
  §2.2). `text_width` and `truncate_to_width` give the shared layout
  arithmetic.
- `cache` — the on-demand outline rasteriser (over embedded faces + the shared
  `lib/fontface` engine) and its bounded, process-global cache (behind
  `render`). See *Rendering at a chosen size* below.

## Rendering at a chosen size

A `BitmapFont` renders at a chosen **cell height in physical pixels**.
`BitmapFont::inconsolata()` keeps the atlas's native height and is what the
text console (`lib/fbcon`) draws at — its glyphs come straight from the atlas
with no resampling, so console rendering is byte-for-byte unchanged.
`BitmapFont::with_pixel_height(px)` asks for any other cell: the desktop
resolves a comfortable physical size from the theme's logical font size and
the DPI scale (`tairix_geometry::Scale`), so window titles, the taskbar, the
start menu, and the file browser render at that size. Every derived metric
(advance, cell width, baseline, line height) scales with the cell height,
keeping the font monospaced and its aspect ratio fixed.

A non-native cell rasterises each glyph **directly from the TrueType outline**
at that exact size, through the shared `lib/fontface` engine over the embedded
faces — the very engine the atlas is generated with. Sampling the curve at the
target resolution keeps text crisp whether tiny or very large, so a 200-pixel
heading is as sharp as 14-pixel body text and neither is a stretched bitmap.
Because the desktop redraws the same glyphs at the same size every frame, each
rasterised glyph is memoised in a bounded, spinlock-guarded process-global
cache keyed by `(face, glyph, cell height)`; a hit copies into the caller's
reusable buffer with no rasterisation and the cache evicts its oldest entry
when full, so its footprint stays bounded (`AGENTS.md` §2.16, §24.1). The
faces are parsed once, lazily, and a scalar the faces do not cover falls back
to the same U+FFFD glyph the atlas shows; if the (trusted) faces ever fail to
parse, rasterisation fails closed to blank rather than panicking. The cache
and rasteriser ride the `render` feature; the allocator-free `atlas`/`glyph`
view never touches them.

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
