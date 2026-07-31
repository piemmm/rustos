# `tairix-image` — raster-image decoding

`lib/image` turns an untrusted raster-image byte stream into a validated,
straight-alpha RGBA8 [`RasterImage`], or a typed refusal — never a panic,
and never more memory than the caller allows. The desktop's
application-icon pipeline is the reason this crate exists: a bundle's icon
artwork (SVG or PNG) is decoded inside a minimum-capability parser sandbox
before it ever reaches the compositor, because it ships from whoever
authored the `.app`, not from the system. This crate is the PNG half of
that pipeline (the SVG half is `lib/svg`).

## Formats

`ImageFormat` is a deliberately closed enum — `Png` only, today.
`sniff(bytes) -> Option<ImageFormat>` identifies a format from its leading
signature, and `decode(bytes, limits) -> Result<RasterImage, DecodeError>`
dispatches on it, refusing an unrecognised signature before any
format-specific parsing runs. A further format is added only when a real
consumer needs it — never speculatively — exactly as PNG was added for the
icon pipeline.

The PNG decoder is complete against the W3C PNG specification: the 8-byte
signature and chunk framing (length, type, payload, CRC-32); the chunk
ordering rules (`IHDR` first and unique, `PLTE` before the first `IDAT`,
`IDAT` chunks contiguous, `IEND` last and empty, no data afterwards, an
unknown critical chunk refused while an unknown ancillary chunk is
skipped); every colour type (greyscale, truecolour, indexed, greyscale +
alpha, truecolour + alpha) at every bit depth the specification permits for
it, including sub-byte greyscale/indexed depths (1, 2, 4) unpacked
most-significant-bit first; `PLTE` and `tRNS` (including colour-key
transparency, compared at the image's native bit depth before any 8-bit
scaling); all five scanline filters (None, Sub, Up, Average, Paeth); and
full Adam7 interlacing. The `IDAT` stream is zlib/DEFLATE, decoded through
`tairix_compress`'s decode-only `inflate`/`zlib` modules rather than a
second copy of that logic.

## Security

[`DecodeLimits`] — a maximum width, height, and total pixel count — is the
caller's ceiling on the image this crate will ever produce. A format
decoder checks its declared dimensions against the limits **the moment it
reads them**, before allocating a single scanline, palette entry, or output
pixel, so a file that lies about its dimensions cannot make this crate
reserve memory proportional to the lie rather than the bytes actually
present. Every other declared size — a chunk length, a palette entry
count, the decompressed image size a PNG's geometry implies — is validated
against the bytes actually available, or against a size computed purely
from already-bounded geometry, before it is used to allocate or index
anything.

Every public entry point is total: malformed, truncated, or adversarial
input returns a typed `DecodeError`, never a panic. All size and offset
arithmetic over untrusted values uses checked or widened integer
operations, so a crafted input cannot provoke an overflow panic even in a
debug build. The crate is `no_std` + `alloc`, `#![forbid(unsafe_code)]`, and
holds no authority and performs no I/O of its own — it is meant to run
inside the icon pipeline's minimum-capability parser sandbox, which is the
actual capability boundary: a crash or resource exhaustion here is
contained to that sandbox, never the calling service.

## API shape

- `sniff(&[u8]) -> Option<ImageFormat>` — format identification from a
  byte signature.
- `decode(&[u8], &DecodeLimits) -> Result<RasterImage, DecodeError>` — the
  one decode entry point, dispatching on `sniff`.
- `DecodeLimits::new(max_width, max_height, max_pixels)` and its
  `max_width`/`max_height`/`max_pixels` accessors.
- `RasterImage::{width, height, pixels, into_pixels}` — row-major,
  4-byte-per-pixel, straight-alpha RGBA8.
- `ImageFormat::Png` — the closed format enum.
- `DecodeError` — every fail-closed refusal reason, from framing and
  chunk-ordering violations through `IHDR`/`PLTE`/`tRNS` validation to a
  `CompressedData` variant wrapping `tairix_compress::zlib::Error`.

The crate is `no_std` + `alloc`, host-unit-tested beside the code, and
fuzzed by `tests/fuzz_image.rs` (registered with `cargo xtask fuzz`).
Stability tier: experimental (`lib/image/README.md`).
