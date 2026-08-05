# `tairix-image` — raster-image decoding

`lib/image` turns an untrusted raster-image byte stream into a validated,
straight-alpha RGBA8 [`RasterImage`], or a typed refusal — never a panic,
and never more memory than the caller allows. The desktop's sandboxed
image-rendering service is the reason this crate exists: an application
bundle's icon artwork (SVG or PNG) and the desktop wallpaper (a shipped
master, or a photograph the user picked) are each decoded inside a
minimum-capability parser sandbox before they ever reach the compositor,
because neither ships from the system. This crate is the raster half of
that pipeline (the vector half is `lib/svg`).

## Formats

`ImageFormat` is a deliberately closed enum: `Png` and `Jpeg`.
`sniff(bytes) -> Option<ImageFormat>` identifies a format from its leading
signature, and both `decode` and `decode_fitted` dispatch on it, refusing
an unrecognised signature before any format-specific parsing runs. A
further format is added only when a real consumer needs it — never
speculatively — exactly as PNG was added for the icon pipeline and JPEG for
the pinboard's wallpaper masters.

### PNG

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

### JPEG

The JPEG decoder covers the Huffman-coded DCT modes of ITU-T T.81 at 8-bit
sample precision: baseline sequential (`SOF0`), extended sequential
(`SOF1`), and progressive (`SOF2`) frames; 1-component greyscale and
3-component YCbCr, plus RGB when an Adobe APP14 marker (Adobe TN5116)
declares colour transform zero; any per-component sampling factor from 1 to
4, with chroma upsampled from its own sample plane; restart markers and
multi-scan streams; up to four DC and four AC Huffman tables; 8- and 16-bit
quantisation tables in the standard zig-zag order; and all five scan shapes
progressive coding defines (sequential, DC first, DC refinement, AC first,
AC refinement, including end-of-band runs that span blocks). Entropy
decoding uses a fast lookup table for short Huffman codes and a
bit-at-a-time canonical search only for the long ones, and reconstruction
inverse-DCTs each block with no per-pixel allocation.

The full-scale (unreduced) decode — the path that dominates a
wallpaper-sized render — reconstructs each 8×8 block with a fast
fixed-point integer inverse DCT: the standard AAN /
Loeffler-Ligtenberg-Moerlein separable row-column butterfly (the
formulation libjpeg names `jpeg_idct_islow`), in `i32` with the usual
descale/rounding shifts and a flat-block (all-AC-zero) fast path. That
replaces the direct routine's per-block `O(8^3)` scaled-basis matrix
multiply with `O(8^2)` multiply-adds; on the shipped 8.29-megapixel light
wallpaper it cut a full-scale decode from roughly 276 ms to 64 ms on the
development host. The reduced scales (one half, quarter, or eighth) keep the
direct matrix routine, which is already cheap when it evaluates only 1, 2, or
4 samples per block edge and doubles as the reference a permanent unit test
checks the fast routine against (no more than a one-level per-sample
difference, the tolerance the standard's accuracy requirement leaves). The
transform's arithmetic is `wrapping_*`: a valid 8-bit frame's coefficients
are bounded so no wrap ever occurs and the result is exact, while a hostile
file can at worst wrap an intermediate into the closing fixed clamp to
`0..=255` — never a panic under the workspace's overflow checks, and never a
pixel outside range.

Final assembly reconstructs a subsampled component by **triangle
interpolation** on both axes, the reconstruction a quality decoder performs:
a chroma sample sits at the centre of the output pixels it covers, so an
output pixel is the weighted blend of the two chroma samples it lies between.
Replicating each chroma sample across those pixels instead — the "fast"
reconstruction the standard permits — reproduces the chroma grid as 2x2
blocks of flat colour across the whole photograph, blockiness that appears
long before any resampling stage is reached, and a bare-ratio projection that
skips the half-sample centre offset shifts chroma against luma and fringes
every hard edge with colour. The interpolation is planned once rather than
per pixel: the horizontal taps are identical for every row, so each component
resolves one whole output-width row at a time and the per-pixel loop does
nothing but read three bytes and colour-convert. A component already sampled
as densely as the frame — luma, or every channel of an RGB image — is read
straight from its plane with no copy and no arithmetic at all.

Everything else a stream can declare is a typed, fail-closed refusal
rather than a best effort: arithmetic coding, lossless and hierarchical
(differential) frames, 12-bit precision, 2- or 4-component images, a height
deferred to a `DNL` marker, and any malformed stream.

## Reduced-scale decode (`decode_fitted`) is a JPEG property

`decode_fitted(bytes, limits, fit)` returns an image no smaller than it has
to be to cover the caller's `FitBox` on both axes. For JPEG it picks the
smallest DCT decode scale — one whole, one half, one quarter, or one eighth
of natural size, produced by inverse-DCT transforming only the
coefficients that scale needs — whose output still covers the box. It never
scales up and never resamples; reduced dimensions round up, so the result
can be modestly larger than the box but never smaller. Decoding a
8.3-megapixel wallpaper master straight to an eighth costs a fraction of the
full-size arithmetic and output buffer.

### Degrading rather than refusing

Where the smallest covering scale's own output would breach the caller's
`DecodeLimits`, `decode_fitted` decodes the largest scale that stays within
them instead of refusing: a screen larger than the limits allow is served
slightly soft rather than not at all. That is a deliberate trade of
sharpness for memory and never a trade of correctness or memory safety, and
it is what lets the desktop pinboard show an 8.3-megapixel master on a 4K
screen inside a bound a 1 GiB machine can afford.

The scale is decided entirely from the frame header's declared geometry,
**before** any coefficient store or pixel buffer is allocated: no scale is
ever attempted, abandoned, and retried, and nothing is decoded twice. Only
when even the one-eighth scale breaches the limits is the image refused, and
then with whichever limit that smallest possible output broke — so the
refusal still names the real reason.

`decode` has no such freedom and keeps none: it always means natural size,
and is refused outright when that size breaches the limits.

### PNG

PNG has no reduced-scale decode process — its filtered, zlib-compressed
scanlines do not separate into scale-selectable passes — so `decode_fitted`
on a PNG *is* `decode`, at natural size, and has no scale to degrade to.
That asymmetry is an honest property of the two formats rather than a gap in
this crate: a caller that wants a smaller PNG resamples the decoded image
through `lib/raster`'s one shared resampler, exactly as it must to hit any
size no JPEG scale lands on. `decode` keeps its meaning for both formats:
natural size.

## Security

[`DecodeLimits`] is the caller's ceiling on the image this crate will ever
produce. Its width, height, and total-pixel-count limits are weighed
against the size the decode is about to produce — the declared dimensions
for `decode`, the chosen scale's output for `decode_fitted` — **the moment a
format decoder reads the header** and before allocating a single scanline,
coefficient, or output pixel, so a file that lies about its dimensions
cannot make this crate reserve memory proportional to the lie rather than
the bytes actually present. Every other declared size — a chunk length, a
palette entry count, the decompressed image size a PNG's geometry implies,
a JPEG segment length, sampling factor, table index, or spectral band — is
validated against the bytes actually available, or against a size computed
purely from already-bounded geometry, before it is used to allocate or index
anything.

`max_progressive_coefficient_bytes` is the fourth limit, and the same
defence applied to the one buffer whose size a JPEG's *mode* rather than
its output geometry dictates. A progressive scan may only refine
coefficients an earlier scan already placed, so the decoder cannot produce
a single pixel until the final scan has been read: every component's every
block's every coefficient must be held, at 2 bytes each, for the whole of
the entropy-coded data. A 25-megapixel 4:2:0 image alone needs roughly
75 MB of that store, which the 1 GiB operating-conditions floor cannot
spend freely. The total is computed in checked 64-bit arithmetic from the
already-validated frame geometry and compared against the bound **before**
the store is allocated; over it, the decode is refused with
`JpegProgressiveCoefficientStoreExceedsLimit`. It is a fixed security
bound, not a growable capacity: the decoder never enlarges it to make a
stream fit. A caller that decodes only PNG or baseline/extended sequential
JPEG passes `0`, refusing every progressive stream outright.

Every public entry point is total: malformed, truncated, or adversarial
input returns a typed `DecodeError`, never a panic. All size and offset
arithmetic over untrusted values uses checked, saturating, or widened
integer operations, so a crafted input cannot provoke an overflow panic
even in a debug build. The crate is `no_std` + `alloc`,
`#![forbid(unsafe_code)]`, and holds no authority and performs no I/O of
its own — it is meant to run inside the image pipeline's
minimum-capability parser sandbox, which is the actual capability
boundary: a crash or resource exhaustion here is contained to that sandbox,
never the calling service.

## API shape

- `sniff(&[u8]) -> Option<ImageFormat>` — format identification from a
  byte signature.
- `probe(&[u8]) -> Result<ImageInfo, DecodeError>` — the format and natural
  size from the header alone, decoding no pixels and allocating no pixel
  buffer. It is for the caller that cannot state its target size until it
  knows the source's: a composition mapping part of an image onto part of a
  destination settles that question for the price of parsing a header instead
  of decoding at a guessed scale. The geometry it reports is the file's own
  claim, so it is exactly as trustworthy as the file — nothing is sized from
  it here and no limit is applied to it, and a caller holds it to its own
  bounds before acting on it. What a probe does guarantee is that the header
  is structurally valid: it reuses the same header parsers a full decode uses,
  so it refuses precisely the headers a decode would.
- `decode(&[u8], &DecodeLimits) -> Result<RasterImage, DecodeError>` —
  decode at natural (full) size, dispatching on `sniff`.
- `decode_fitted(&[u8], &DecodeLimits, FitBox) -> Result<RasterImage,
  DecodeError>` — decode at the smallest covering scale the format offers.
- `FitBox::new(width, height)` with `width()`/`height()` — the caller's
  target output box, a small public copy type.
- `DecodeLimits::new(max_width, max_height, max_pixels,
  max_progressive_coefficient_bytes)` and its four accessors.
- `RasterImage::{width, height, pixels, into_pixels}` — row-major,
  4-byte-per-pixel, straight-alpha RGBA8.
- `ImageFormat::{Png, Jpeg}` — the closed format enum.
- `DecodeError` — every fail-closed refusal reason: PNG framing and
  chunk-ordering violations, `IHDR`/`PLTE`/`tRNS` validation, a
  `CompressedData` variant wrapping `tairix_compress::zlib::Error`, and the
  `Jpeg*` family covering signature, marker, segment, quantisation- and
  Huffman-table, entropy-data, scan-header, restart-marker,
  unsupported-mode, and progressive-coefficient-store refusals.

The crate is `no_std` + `alloc` and host-unit-tested beside the code with
no external fixture files: the JPEG tests build their streams marker by
marker, check a progressive stream against the pixels of the equivalent
baseline one, and check the fast full-scale inverse DCT against the direct
matrix reference over many pseudo-random full coefficient blocks (asserting
no more than a one-level per-sample difference). Both formats are fuzzed by
`tests/fuzz_image.rs` — random
bytes, random bytes behind each valid signature, and structurally mutated
valid fixtures, baseline and progressive — registered with
`cargo xtask fuzz`. Stability tier: experimental
(`lib/image/README.md`).
