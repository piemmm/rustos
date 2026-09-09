# tairix-image

Stability tier: **experimental**.

First-party TAIRiX raster-image decoding: complete, fail-closed PNG, JPEG,
GIF, BMP, and ICO/CUR decoders that turn untrusted artwork into a validated,
straight-alpha RGBA8 pixel buffer, or a typed refusal — never a panic, and
never more memory than the caller allows.

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

The picture viewer (`plans/VIEW.md`) is the other consumer, through the same
sandbox. Every format it claims lands here rather than beside it, so a
format's decoder exists once. *Admitting* a format is still each consumer's
own decision: the icon pipeline deliberately takes only PNG and SVG
(`plans/ICONS.md`), and the wallpaper catalog only its own extensions.

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
- **GIF** (`ImageFormat::Gif`, GIF89a and its GIF87a subset): both versions;
  global and local colour tables at every declared size; the whole block
  chain; the variable-code-width LZW dialect with the not-yet-defined-code
  case and the deferred clear; four-pass interlacing; the transparent colour
  index; the full frame-disposal model; and the de-facto `NETSCAPE2.0`
  animation-loop count.
- **BMP** (`ImageFormat::Bmp`, the Windows device-independent bitmap): the
  `BITMAPFILEHEADER` and every DIB header of the Windows lineage —
  `BITMAPCOREHEADER`, `BITMAPINFOHEADER`, and the `V2`/`V3`/`V4`/`V5`
  headers extending it; 1, 2, 4, 8, 16, 24, and 32 bits per pixel;
  `BI_RGB`, `BI_RLE4`, `BI_RLE8`, `BI_BITFIELDS`, and `BI_ALPHABITFIELDS`;
  bottom-up and top-down row order; and colour tables of both entry widths.
- **ICO and CUR** (`ImageFormat::Ico`, the Windows icon and cursor
  containers): the directory of independent pictures at different sizes,
  each entry a DIB with its 1-bit AND mask or a whole PNG file, with
  addressed access to any page.

A format this crate claims is decoded **completely** — every bit depth,
compression, colour handling, and structural variant the format defines, not
the subset a common file happens to use. A format that could only be
half-decoded is not claimed at all.

BMP reads the specification literally but for two places, both stated in its
module's own rustdoc. A 32-bit `BI_RGB` pixel's fourth byte is undefined, so
a BMP file's is ignored and the picture comes out opaque, while an icon's is
the alpha channel every writer since Windows XP fills — the file header is
what tells the two apart. And pixels a run-length-encoded array never covers
stay fully transparent, because the format gives them no value and every
other choice invents one. An icon whose alpha channel is zero in every pixel
carries none at all, so its 1-bit mask is what says which pixels are absent.

The OS/2 2.x header lengths are refused by name rather than half-read: they
share the Windows prefix but read compression codes 3 and 4 as Huffman 1D and
RLE24, two codecs with no other consumer here.

GIF reads the specification literally but for two places, both stated in the
module's own rustdoc: *restore to background* clears to fully transparent
rather than to the declared background colour (every producer means "clear
it", and an opaque colour would flash a box through nearly every real
animation), and a frame's delay is reported exactly as the file gives it,
including zero, because clamping a too-fast animation is a playback decision.

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
- `Sequence::{open, info, next_frame, page, rewind}`, `SequenceInfo`,
  `SequenceKind::{Animation, Pages}`, and `Frame` — the multi-entry shape
  (see below).
- `DecodeError` — every fail-closed refusal reason, including a
  `CompressedData` variant wrapping `tairix_compress::zlib::Error`, the
  `Jpeg*` family covering signature, marker, segment, table, entropy,
  scan-header, restart, unsupported-mode, and progressive-store refusals,
  and the `Bmp*` and `Ico*` families covering header version, bit count,
  compression, mask, colour-table, pixel-offset, run-length, and directory
  refusals.

### Sequences and pages

`Sequence` is the one shape for a container holding more than one picture:
`open` validates the structure and decodes no pixels, `info` answers the
format, the geometry of the picture the container is, the entry count, and
whether the entries are an `Animation { loop_count }` or `Pages`;
`next_frame` decodes the next entry and lends a `Frame` (index, geometry,
declared delay in nanoseconds, pixels); `page` decodes one entry by index;
`rewind` restarts, which is also what makes it safe to step on after a
refusal — a refusal hands out no pixels and is remembered until then, because
an animation's frame that stopped part-way leaves the canvas describing no
whole frame.

It is **forward-only with a rewind**, because that is what an animation *is*.
A GIF frame composites onto whatever its predecessors left on the logical
screen under the disposal method declared for each, so a decoder that could
be asked for frame *n* directly would have to re-composite every frame before
it — wrong per-index, and quadratic over a walk. Holding the canvas and
stepping makes each frame cost its own decode and no more, and the pixels a
`Frame` lends are the canvas *after* compositing, so a consumer never has to
know the format's disposal model. They are borrowed rather than owned for the
same reason: the canvas has to be retained for the next frame, so an owned
buffer per step would copy the whole canvas every frame for nothing.

A **page** container's entries are independent pictures instead, so `page`
addresses one directly and a refused page disturbs no other. An icon file is
the case that matters: its pages are one picture at several sizes, and
choosing between them is the whole point of the format. Addressing a frame of
an *animation* still costs a restart and a walk, which is why a player steps.

A still picture is the **one-entry case** of the same shape, so a consumer
that shows pictures, animations, and icon files needs one path rather than
three. `decode` on a multi-frame container answers its first composited frame
— the picture the format shows first — and on a page container its largest
page, which is what a container of one picture at several sizes means by its
picture.

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

An icon container has a scale of its own kind: it *is* one picture at several
sizes, so `decode_fitted` takes the smallest page covering the box that stays
within the limits, falling back to the largest that does. Nothing is computed
and nothing is resampled — a page is already the picture at that size.

None of PNG, GIF, and BMP has such a process — filtered zlib-compressed
scanlines, an LZW code stream, and a padded row array do not separate into
scale-selectable passes — so `decode_fitted` on those *is* `decode`, at
natural size, with no scale to degrade to. That asymmetry is an honest
property of the formats, not a gap in this crate: a caller that wants a
smaller one resamples the decoded image through `lib/raster` (the one shared
resampler), exactly as it would to hit a size no JPEG scale lands on.

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

A GIF's frame count carries its own fixed containment bound of 16 384: a
frame block costs about ten bytes, so a small file can declare enormous
numbers of them, and no viewer has use for an animation longer than that. It
bounds the count the structural pass accepts, and nothing is allocated per
frame.

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
`src/gif_tests.rs`, `src/bmp_tests.rs`, `src/ico_tests.rs`, `src/crc32.rs`)
with no external fixture files, the PNG writer they share living in
`src/png_fixture.rs` because an icon entry may be a whole PNG file: the JPEG
tests build their streams marker by marker, check a progressive stream
against the pixels of
the equivalent baseline one, check **every** inverse-DCT scale against a
direct reference the test file restates from the standard's own definition
over many pseudo-random full coefficient blocks (asserting no more than a
one-level per-sample difference), and assert that reducing a block preserves
its mean — the property a scaled transform has and a magnified corner crop
does not. The GIF tests build their streams block by block through one
code-stream writer that mirrors the width schedule a conforming decoder reads
at, and cover every structural variant, the whole disposal model with
hand-verified canvases, an exhaustive check that the four interlace passes
cover every row exactly once, a compressed stream matched against the literal
stream of the same pixels, and every refusal — including every prefix of a
valid file being refused rather than half-decoded. The BMP and icon tests
build every header version, bit depth, encoding, row order, and mask layout
the same way, and cover the run-length escapes, the alpha-versus-mask rule,
page addressing, and a truncated container still answering a page it wholly
holds. Every format is fuzzed by `tests/fuzz_image.rs` — random bytes, random
bytes behind each valid signature, and structurally mutated valid fixtures
(PNG chunks, JPEG baseline and progressive marker segments, GIF blocks, icon
directory entries, and a BMP header's declared fields), each walked through
`decode`, `decode_fitted`, and a full `Sequence` pass with a rewind — through
the shared `tests/fuzzseed` seed and budget seam, registered with
`cargo xtask fuzz`.
The subsystem page is `docs/src/lib/image.md`.
