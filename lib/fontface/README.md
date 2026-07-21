# tairix-fontface

Shared TrueType glyph-outline engine: the one parser + anti-aliased
non-zero-winding rasteriser that turns a committed TrueType face into 4-bit
coverage bitmaps **at any requested pixel size**, plus the merged-family
codepoint resolution (`FontFamily`) that both the atlas generator and the
runtime font build on.

Two consumers share this engine, so the rasteriser is written once
(`AGENTS.md` §2.2):

- `cargo xtask font-atlas` rasterises every mapped scalar once, at the native
  `ATLAS_EM_PX`, to emit the generated `lib/font` atlas.
- `lib/font` rasterises a glyph on demand at the desktop's requested cell
  height, so UI text is drawn from the outlines at its true size — crisp
  whether tiny or very large — rather than resampled from a fixed bitmap.

`no_std` + `alloc`, no `unsafe`. Fails closed: any malformed or unsupported
table yields a `FontError` rather than a wrong glyph or a panic. Float rounding
uses the crate's own bounded helpers, so it needs no `std` libm.

## Stability

`experimental` — the API is settled around the two in-tree consumers but may
change as installed-font support grows.
