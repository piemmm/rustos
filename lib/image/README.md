# tairix-image

Stability tier: **experimental**.

First-party TAIRiX raster-image decoding: complete, fail-closed PNG and
JPEG decoders that turn untrusted artwork into a validated, straight-alpha
RGBA8 pixel buffer, or a typed refusal — never a panic, and never more
memory than the caller allows.

## Consumers

The desktop's sandboxed image-rendering service (`lib/sandbox`'s
`imagerender`) decodes both an application bundle's own icon — SVG or
PNG — and the desktop wallpaper (a shipped master, or a photograph the
user picked) inside a minimum-capability parser sandbox before either
reaches the compositor, because neither ships from the system. This crate
is the raster half of that pipeline; the vector half is `lib/svg`. The
shipped wallpaper masters in `lib/wallpaper/assets` are baseline JPEG
authored no larger than the wallpaper renderer's own maximum destination
(3840×2160), but a user-picked photograph can still arrive at many times
that, which is what `decode_fitted` and the progressive-store bound below
exist for.

## Formats

`ImageFormat` is a deliberately closed enum, and both entry points
dispatch on the signature `sniff` recognises. A further format is added
only when a real consumer needs it — never speculatively.

- **PNG** (`ImageFormat::Png`, W3C PNG): every colour type and bit depth,
  interlaced or not.
- **JPEG** (`ImageFormat::Jpeg`, ITU-T T.81): baseline sequential (`SOF0`),
  extended sequential (`SOF1`), and progressive (`SOF2`) DCT frames with
  Huffman coding at 8-bit precision; 1-component greyscale and 3-component
  YCbCr, plus RGB when an Adobe APP14 marker (Adobe TN5116) declares
  colour transform zero; any per-component sampling factor from 1 to 4;
  restart markers; multi-scan streams; up to four DC and four AC Huffman
  tables; and 8- or 16-bit quantisation tables.

Everything else a JPEG stream can declare is a typed, fail-closed
refusal rather than a best effort: arithmetic coding, lossless and
hierarchical (differential) frames, 12-bit precision, 2- or 4-component
images, a height deferred to a `DNL` marker, and any malformed stream.

Reconstruction inverse-DCTs each block with no per-pixel allocation, at
every scale through a fast fixed-point integer butterfly of that scale's own
size. The full-scale path is the standard AAN / Loeffler-Ligtenberg-Moerlein
separable row-column inverse DCT (the formulation libjpeg names
`jpeg_idct_islow`), in `i32` with the usual descale/rounding shifts and a
flat-block (all-AC-zero) fast path, replacing a direct `O(8^3)` matrix
multiply with `O(8^2)` multiply-adds.

A reduced scale discards the block's high-frequency coefficients and
inverse-transforms the surviving top-left `m`×`m` corner with the
**`m`-point** basis, so its `m` samples span the whole 8-sample block — the
block's band-limited decimation. Re-using the *8*-point basis over that
corner would instead evaluate the block's first `m` spatial positions, a
magnified crop that tiles the image with visible block seams; each reduced
scale therefore has its own butterfly and dequantises only the coefficients
it reads. The arithmetic is `wrapping_*`: for a valid 8-bit frame the
coefficients are bounded and no wrap ever occurs, so the transform is exact,
while a hostile file can at worst wrap an intermediate into the closing
fixed clamp to `0..=255` — never a panic, and never a pixel outside range.

The final assembly reconstructs a subsampled component by **triangle
interpolation** on both axes: a chroma sample sits at the centre of the
output pixels it covers, so an output pixel blends the two chroma samples it
lies between. Replicating each sample instead reproduces the chroma grid as
2x2 blocks of flat colour, and projecting by a bare ratio (skipping the
half-sample centre offset) fringes every hard edge with colour. The taps are
planned once — they are identical for every row — so each component resolves
one output-width row at a time and the per-pixel work is three byte reads and
the colour convert; a component already as dense as the frame is read straight
from its plane.

## API shape

- `sniff(&[u8]) -> Option<ImageFormat>` — identify a format from its
  leading signature.
- `probe(&[u8]) -> Result<ImageInfo, DecodeError>` — the format and natural
  size from the header alone, decoding no pixels. For the caller that cannot
  state its target size until it knows the source's. The reported geometry is
  the file's own claim, so nothing is sized from it here and the caller holds
  it to its own bounds; the header itself is validated by the same parsers a
  full decode uses.
- `decode(&[u8], &DecodeLimits) -> Result<RasterImage, DecodeError>` —
  decode at natural (full) size.
- `decode_fitted(&[u8], &DecodeLimits, FitBox) -> Result<RasterImage,
  DecodeError>` — decode no smaller than it has to be to cover the
  caller's target box (see below).
- `FitBox::new(width, height)` plus `width()`/`height()` — a small public
  copy type carrying the largest output the caller intends to use.
- `DecodeLimits::new(max_width, max_height, max_pixels,
  max_progressive_coefficient_bytes)` plus its accessors.
- `RasterImage::{width, height, pixels, into_pixels}` — the one output
  shape every format decodes into: row-major RGBA8, **straight**
  (non-premultiplied) alpha. `lib/raster`'s `Surface::from_rgba8` is where
  premultiplication happens, once, on the consumer side.
- `DecodeError` — every fail-closed refusal reason, including a
  `CompressedData` variant wrapping `tairix_compress::zlib::Error` and the
  `Jpeg*` family covering signature, marker, segment, table, entropy,
  scan-header, restart, unsupported-mode, and progressive-store refusals.

### Reduced-scale decode is a JPEG property, not a shared feature

`decode_fitted` picks the smallest JPEG DCT decode scale — one whole, one
half, one quarter, or one eighth of natural size, produced by inverse-DCT
transforming only the coefficients that scale needs — whose result still
covers the caller's `FitBox` on both axes. It never scales up and never
resamples: reduced dimensions round up, so a result can be modestly larger
than the box but never smaller. Decoding an 8.3-megapixel wallpaper master
straight to an eighth costs a fraction of the full-size arithmetic and
output buffer.

Where that covering scale's own output would breach the caller's
`DecodeLimits`, `decode_fitted` **degrades** to the largest scale that
stays within them rather than refusing — a deliberate trade of a little
sharpness for a decode the caller can afford, never a trade of correctness
or memory safety. The scale is settled from the frame header's declared
geometry before any coefficient store or pixel buffer is allocated, so no
scale is ever attempted, abandoned, and retried, and nothing is decoded
twice. Only when even the one-eighth scale breaches the limits is the image
refused, and then with whichever limit that smallest possible output broke.
`decode` has no such freedom and keeps none: it always means natural size,
and is refused outright when that size breaches the limits.

PNG has no such process — its filtered, zlib-compressed scanlines do not
separate into scale-selectable passes — so `decode_fitted` on a PNG *is*
`decode`, at natural size, with no scale to degrade to. That asymmetry is an
honest property of the two formats, not a gap in this crate: a caller that
wants a smaller PNG resamples the decoded image through `lib/raster` (the
one shared resampler), exactly as it would to hit a size no JPEG scale
lands on.

## Security

Every declared size — a chunk length, a palette entry count, the
decompressed size a PNG's geometry implies, a JPEG segment length,
sampling factor, table index, or spectral band — is validated against the
bytes actually available, or against a size computed purely from
already-bounded geometry, before it is used to allocate or index anything.
`DecodeLimits`' width, height, and pixel-count ceilings are weighed against
the size the decode is about to produce — the declared dimensions for
`decode`, the chosen scale's output for `decode_fitted` — the moment a
format decoder reads the header, before a single scanline, coefficient, or
output pixel is allocated, so a file lying about its size cannot make this
crate reserve memory proportional to the lie rather than the bytes actually
present.

`max_progressive_coefficient_bytes` is the same defence for the one buffer
whose size a JPEG's *mode* rather than its output geometry dictates. A
progressive scan may only refine coefficients an earlier scan already
placed, so no pixel can be produced until the last scan has been read:
every component's every block's every coefficient must be held, at 2 bytes
each, for the whole of the entropy-coded data. A 25-megapixel 4:2:0 image
alone needs roughly 75 MB of that store, which a 1 GiB machine cannot
spend freely. The total is therefore computed in checked 64-bit arithmetic
from the already-validated frame geometry and compared against this bound
**before** the store is allocated; over it, the decode is refused with
`JpegProgressiveCoefficientStoreExceedsLimit`. It is a fixed security
bound, not a growable capacity: this crate never enlarges it to make a
stream fit. A caller that only ever decodes PNG or baseline/extended
sequential JPEG passes `0`, which refuses every progressive stream
outright.

Every entry point is total: malformed, truncated, or adversarial input
returns a typed `DecodeError`, never a panic, and every size/offset
computation over untrusted values uses checked, saturating, or widened
integer arithmetic so a crafted input cannot provoke an overflow panic
even in a debug build. The crate is `no_std` + `alloc`,
`#![forbid(unsafe_code)]`, and has no dependency beyond `tairix-compress`
(PNG's `IDAT` stream is zlib/DEFLATE, so the decode-only `inflate`/`zlib`
modules there are reused rather than re-implemented).

This crate performs no I/O and holds no authority of its own: it is meant
to run inside the image pipeline's parser sandbox, which supplies the
capability boundary — a crash or resource exhaustion here is contained to
that sandbox, never the calling service.

## Tests

Host-unit-tested beside the code (`src/png_tests.rs`, `src/jpeg_tests.rs`,
`src/crc32.rs`) with no external fixture files: the JPEG tests build their
streams marker by marker, check a progressive stream against the pixels of
the equivalent baseline one, check **every** inverse-DCT scale against a
direct reference the test file restates from the standard's own definition
over many pseudo-random full coefficient blocks (asserting no more than a
one-level per-sample difference), and assert that reducing a block preserves
its mean — the property a scaled transform has and a magnified corner crop
does not. Both formats are fuzzed by
`tests/fuzz_image.rs` — random bytes, random bytes behind each valid
signature, and structurally mutated valid fixtures (baseline and
progressive) through the shared `tests/fuzzseed` seed and budget seam —
registered with `cargo xtask fuzz`. The subsystem page is
`docs/src/lib/image.md`.
