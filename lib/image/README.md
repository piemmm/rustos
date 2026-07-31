# tairix-image

Stability tier: **experimental**.

First-party TAIRiX raster-image decoding: a complete, fail-closed PNG
decoder that turns untrusted bundle artwork into a validated, straight-alpha
RGBA8 pixel buffer, or a typed refusal — never a panic, and never more
memory than the caller allows.

## Consumer

The desktop's application-icon pipeline decodes a bundle's own icon —
SVG or PNG — inside a minimum-capability parser sandbox before it ever
reaches the compositor, because a bundle's icon ships from whoever authored
the `.app`, not from the system. This crate is the PNG half of that
pipeline; the SVG half is `lib/svg`. Nothing else in the tree depends on it
today.

## Formats

PNG only, today: `ImageFormat` is a deliberately closed enum, and
[`decode`] dispatches on the signature [`sniff`] recognises. A further
format is added only when a real consumer needs it, exactly as PNG was
added for the icon pipeline — never speculatively.

- `sniff(bytes) -> Option<ImageFormat>` — identify a format from its
  leading signature.
- `decode(bytes, limits) -> Result<RasterImage, DecodeError>` — decode,
  honouring the caller's `DecodeLimits`.
- `RasterImage` — the one output shape every format decodes into: row-major
  RGBA8, **straight** (non-premultiplied) alpha. `lib/raster`'s
  `Surface::from_rgba8` is where premultiplication happens, once, on the
  consumer side.

## Security

Every declared size — a chunk length, a palette entry count, the
decompressed image size a PNG's geometry implies — is validated against the
bytes actually available, or against a size computed purely from
already-bounded geometry, before it is used to allocate or index anything.
`DecodeLimits` (max width, max height, max total pixels) is checked the
moment a format decoder reads its declared dimensions, before a single
scanline or output pixel is allocated, so a file lying about its size
cannot make this crate reserve memory proportional to the lie rather than
the bytes actually present.

Every entry point is total: malformed, truncated, or adversarial input
returns a typed `DecodeError`, never a panic, and every size/offset
computation over untrusted values uses checked or widened integer
arithmetic so a crafted input cannot provoke an overflow panic even in a
debug build. The crate is `no_std` + `alloc`, `#![forbid(unsafe_code)]`, and
has no dependency beyond `tairix-compress` (PNG's `IDAT` stream is
zlib/DEFLATE, so the decode-only `inflate`/`zlib` modules there are reused
rather than re-implemented).

This crate performs no I/O and holds no authority of its own: it is meant
to run inside the icon pipeline's parser sandbox (`AGENTS.md` §19.5), which
supplies the capability boundary — a crash or resource exhaustion here is
contained to that sandbox, never the calling service.

## API shape

- `sniff(&[u8]) -> Option<ImageFormat>`
- `decode(&[u8], &DecodeLimits) -> Result<RasterImage, DecodeError>`
- `DecodeLimits::new(max_width, max_height, max_pixels)` plus its accessors.
- `RasterImage::{width, height, pixels, into_pixels}`.
- `ImageFormat::Png` (closed).
- `DecodeError` — every fail-closed refusal reason, including a `CompressedData`
  variant wrapping `tairix_compress::zlib::Error`.

`no_std` + `alloc`; host-unit-tested beside the code (`src/png_tests.rs`,
`src/crc32.rs`) and fuzzed by `tests/fuzz_image.rs` (registered with
`cargo xtask fuzz`). The subsystem page is `docs/src/lib/image.md`.
