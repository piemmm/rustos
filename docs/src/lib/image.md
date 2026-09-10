# `tairix-image` — raster-image decoding

`lib/image` turns an untrusted raster-image byte stream into a validated,
straight-alpha RGBA8 [`RasterImage`], or a typed refusal — never a panic,
and never more memory than the caller allows. The desktop's sandboxed
image-rendering service is the reason this crate exists: an application
bundle's icon artwork (SVG or PNG) and the desktop wallpaper (a shipped
master, or a photograph the user picked) are each decoded inside a
minimum-capability parser sandbox before they ever reach the compositor,
because neither ships from the system. This crate is the raster half of
that pipeline (the vector half is `lib/svg`). The picture viewer
(`plans/VIEW.md`) is the crate's other consumer, and every format it claims
lands here rather than beside it — being the one raster registry is what
keeps a format's decoder in a single place. *Admitting* a format is still each
consumer's own decision: the icon pipeline deliberately takes only PNG and
SVG (`plans/ICONS.md`), and the wallpaper catalog only its own extensions.

## Formats

`ImageFormat` is a deliberately closed enum: `Png`, `Jpeg`, `Gif`, `Bmp`,
`Ico`, `Sprite`, and `Tiff`. `sniff(bytes) -> Option<ImageFormat>` identifies a format
from its leading signature, and `decode`, `decode_fitted`, and
`Sequence::open` dispatch on it, refusing an unrecognised signature before any
format-specific parsing runs. A further format is added only when a real
consumer needs it — never speculatively — exactly as PNG was added for the
icon pipeline, JPEG for the pinboard's wallpaper masters, and the rest for the
viewer.

Only a format that carries a signature can be recognised from content, and a
RISC OS sprite area does not: its first word is the sprite count, and RISC OS
types a file from its directory entry rather than from its bytes. So `sniff`
never answers `Sprite`, and never guesses one from a structural coincidence —
a heuristic there would be a false-positive machine, and one the crate would
then act on. A caller that already knows the type, from an
`acorn.filetype` attribute, a `lib/browse` media type, or the name a file was
picked by, instead **names** the format: `probe_as`, `decode_as`, and
`Sequence::open_as` take an `ImageFormat` in place of the sniff. The named
format's own parser still validates the bytes, so naming the wrong one is
refused rather than misread, and `probe`/`decode`/`Sequence::open` are exactly
`sniff` plus these.

A format this crate claims is decoded **completely** — every bit depth,
compression, colour handling, and structural variant the format defines, not
the subset a common file happens to use. A format that could only be
half-decoded is not claimed at all.

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

Every decode scale reconstructs its block with a fast fixed-point integer
butterfly of its own size. The full-scale path is the standard AAN /
Loeffler-Ligtenberg-Moerlein separable row-column inverse DCT (the
formulation libjpeg names `jpeg_idct_islow`), in `i32` with the usual
descale/rounding shifts and a flat-block (all-AC-zero) fast path, which
replaces a direct `O(8^3)` matrix multiply with `O(8^2)` multiply-adds.

A **reduced** scale discards the block's high-frequency coefficients and
inverse-transforms the surviving top-left `m`×`m` corner with the
**`m`-point** basis — `alpha(u) * cos((2x+1) * u * pi / (2m))` — so the `m`
samples it produces span the whole 8-sample block. That is the block's
band-limited decimation, and it is what makes a reduced decode a faithful
smaller picture rather than a piece of a larger one: re-using the *8*-point
basis over the same corner would instead evaluate the block's first `m`
spatial positions, which is a magnified crop of its top-left corner and
tiles the image with visible block seams. Each reduced scale has its own
butterfly (`idct4_islow`, `idct2_islow`, `idct1_islow`) and dequantises only
the coefficients that butterfly reads, so it never forms the products of
coefficients it is about to discard.

The transform's arithmetic is `wrapping_*`: a valid 8-bit frame's
coefficients are bounded so no wrap ever occurs and the result is exact,
while a hostile file can at worst wrap an intermediate into the closing
fixed clamp to `0..=255` — never a panic under the workspace's overflow
checks, and never a pixel outside range.

The surrounding hot paths are held to the same bar. The entropy reader
keeps its bits **left justified** in a 64-bit buffer, so a peek is one shift
rather than a mask and a re-justification, and refills four bytes in one
load whenever none of them is `0xFF` (falling to the byte-at-a-time path
that resolves stuffing and stops at a marker). YCbCr → RGB tabulates all
four chroma terms over the 256 values a chroma sample can take, leaving
three table reads, three adds and a shift per pixel with no multiply and no
division.

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
straight from its plane with no copy and no arithmetic at all. The
component count and colour space are resolved once per row rather than once
per pixel, so the pixel loop is a straight three-way zip with no per-pixel
bounds check; a component row shorter than the output row means the frame's
declared geometry and its sample planes disagree, and refuses the decode
rather than inventing pixels.

Everything else a stream can declare is a typed, fail-closed refusal
rather than a best effort: arithmetic coding, lossless and hierarchical
(differential) frames, 12-bit precision, 2- or 4-component images, a height
deferred to a `DNL` marker, and any malformed stream.

### GIF

The GIF decoder is complete against the GIF89a specification and its GIF87a
subset: the signature and both versions; the Logical Screen Descriptor;
global and local colour tables at every declared size; the whole block chain
(Image Descriptor, Graphic Control Extension, Application Extension,
Comment, Plain Text, trailer, and any extension a later specification adds,
all framed as data sub-blocks); the variable-code-width LZW dialect of
Appendix F with its clear/end codes, the not-yet-defined-code case, and the
deferred clear a full table without a clear code relies on; four-pass
interlacing; the transparent colour index; the full frame-disposal model
(keep, clear, restore-to-previous); and the de-facto `NETSCAPE2.0`
animation-loop count.

Two places it does not read the specification literally, both stated in the
module's own rustdoc. *Restore to background* clears the frame's area to
fully transparent rather than to the logical screen's declared background
colour: every producer means "clear it" and every other decoder does this,
and restoring an opaque colour would flash a coloured box through nearly
every real animation. And a frame's delay is reported exactly as the file
gives it, including zero — clamping a too-fast animation is a *playback*
decision, and the thing that plays frames is the one that knows how fast its
screen can show them.

Everything else is a typed refusal: a reserved disposal method (`4`..=`7`),
a frame reaching outside the logical screen, a frame with neither a local
nor a global colour table, an index past the end of the table in force, an
out-of-range LZW minimum code size, a code the table cannot resolve, an LZW
stream that ends before its frame's last pixel, and a chain declaring more
frames than the decoder's fixed containment bound accepts.

### BMP

The BMP decoder is complete against the Windows device-independent bitmap:
the `BITMAPFILEHEADER` and every DIB header of the Windows lineage
(`BITMAPCOREHEADER`, `BITMAPINFOHEADER`, and the `BITMAPV2INFOHEADER`,
`BITMAPV3INFOHEADER`, `BITMAPV4HEADER`, and `BITMAPV5HEADER` extending it);
1, 2, 4, 8, 16, 24, and 32 bits per pixel; `BI_RGB`, `BI_RLE4`, `BI_RLE8`,
`BI_BITFIELDS`, and `BI_ALPHABITFIELDS`; bottom-up and top-down row order,
with rows padded to a four-byte boundary; three-byte `RGBTRIPLE` and
four-byte `RGBQUAD` colour tables, sized by `biClrUsed` or by the bit count;
and channel masks of any contiguous width, widened to eight bits through a
table built once per decode rather than a division per channel per pixel.
The fields a `V4` or `V5` header adds past the masks — colour space,
endpoints, gamma, rendering intent, embedded profile — describe how to
interpret colour rather than where the pixels are, and are read past.

Two places it does not read the specification literally, both stated in the
module's own rustdoc. A 32-bit `BI_RGB` pixel's fourth byte is *undefined*,
so a BMP file's is ignored and the picture comes out opaque; an icon's is
its alpha channel, which the container asks for, because the file header is
what tells the two cases apart and only the container has it. And pixels a
run-length-encoded array never covers — past a delta, after a short line, or
beyond an end-of-bitmap — stay fully transparent, because the format gives
them no value at all and every other choice invents one.

The OS/2 2.x header lengths (16 and 64) are refused **by name** rather than
half-read. They share the Windows header's prefix but read compression codes
3 and 4 as Huffman 1D and RLE24 — two codecs with no other consumer here —
so accepting the length while reading the codes as `BI_BITFIELDS` and
`BI_JPEG` would decode a file into something it is not. Everything else is a
typed refusal too: an unclaimed compression (an embedded JPEG or PNG pixel
array, a CMYK encoding), a bit count the format does not define, a
colour-plane count other than one, a run-length encoding at the wrong bit
count or declaring top-down rows, a mask that is discontiguous, overlapping,
zero for a colour channel, or outside the pixel, a `biClrUsed` larger than
the bit count can index or a colour table that does not fit before the pixel
array, a `bfOffBits` pointing inside the headers or past the end, a pixel
array shorter than the geometry needs, an index past the colour table, and a
run, delta, or line reaching outside the picture.

### ICO and CUR

An icon or cursor container is a directory of independent pictures at
different sizes. Each entry is either a whole PNG file — decoded by the PNG
decoder above, not a second one — or a DIB the BMP decoder reads, declaring
twice its picture's height: the colour rows, then a 1-bit AND mask over them
saying which pixels are absent. Icons and cursors differ only in a type
field and in what a directory entry's two 16-bit fields mean (colour planes
and bit count, or a hot spot), and neither is load-bearing, so one decoder
reads both.

A directory entry's declared width, height, and bit count are **hints**:
real files get them wrong, and a 256-pixel side is spelled zero. Everything
the decoder acts on comes from the entry's own picture header.

A 32-bit entry carries alpha in each pixel's fourth byte, and a writer that
fills it leaves the mask zero — so where the alpha channel says anything at
all, it is what decides transparency and the mask is not applied. An entry
whose alpha is zero in every pixel carries none (a pre-XP icon, or a writer
that never filled the byte), and honouring it would show an entirely
transparent picture, so the mask is what says which pixels are absent
whenever the alpha channel says nothing.

Because the pages are independent, one unreadable page does not refuse the
file: `probe` and `decode` pass over a page whose header will not parse and
answer the largest that does, and only where none parses is the first page's
own refusal the answer. `Sequence` exposes every page, including the
unreadable ones, each with its own result.

### TIFF

The TIFF decoder is complete against TIFF 6.0: both byte orders; the chain of
image file directories, each an independent page; strips *and* tiles; chunky
and planar plane arrangements; bit depths 1, 2, 4, 8, 16, and 32 across the
unsigned, signed, and IEEE-float sample formats (float at 16 and 32 bits, the
two widths a float has); the `WhiteIsZero`, `BlackIsZero`, `RGB`, palette,
transparency-mask, separated (CMYK) and `YCbCr` photometrics, the last with
its chrominance subsampling, luma weights and coded ranges; the horizontal
and floating-point predictors; associated and unassociated extra-sample
alpha; the `Orientation` tag; and the compressions none, `PackBits`, LZW,
Deflate/`AdobeDeflate`, CCITT modified-Huffman, Group 3 (one- and
two-dimensional) and Group 4, and JPEG-in-TIFF.

The LZW and Deflate paths reach the crate's existing codecs rather than a
second copy: `tairix_compress`'s `zlib` module carries TIFF's zlib-wrapped
DEFLATE, and the LZW dictionary and expansion loop are shared with the GIF
decoder (`lib/image/src/lzw.rs`), which differs only in how codes are packed
and when a new entry widens the code that follows it. TIFF carries **two** LZW
dialects: its own, which widens one code earlier than the arithmetic suggests,
and the classic one older writers emit, which packs a code least significant
bit first *and* widens at the later point. They always travel together, and a
stream's opening clear code tells them apart exactly — `0x80 0x00` for TIFF's
own, `0x00 0x01` for the classic. A JPEG-compressed strip or tile is spliced
from the `JPEGTables` field and the unit's own abbreviated stream and handed
to the existing `jpeg` module, so the container grows no second JPEG decoder.

Three readings the format's own text does not settle:

- **A plain `decode` answers the first page the file does not call a reduced
  copy of another.** A TIFF is an ordered document rather than one picture at
  several sizes, so its first page is its picture — unlike an icon file, where
  the largest is. `NewSubfileType` (and the superseded `SubfileType`) is the
  file's own statement that a page is a thumbnail, so honouring it beats
  guessing from size: a document whose second page happens to be larger still
  answers its first. `SequenceInfo` still reports the *largest* page's
  geometry, because that is the canvas a container needs.
- **`Orientation` is applied, not reported.** The tag says which way up the
  stored raster is, so a decoder that ignored it would hand every consumer a
  sideways picture and the format knowledge needed to right it. A transposing
  orientation therefore swaps the geometry `probe` reports, and the
  permutation costs nothing: each sample is written where it belongs rather
  than moved afterwards.
- **A missing photometric under a fax compression reads as `WhiteIsZero`.**
  Every fax is; the tag is otherwise required, and its absence is a writer's
  omission rather than a licence to guess in general.

A fax codes runs of white and black, so the CCITT decoder produces one bit per
pixel with a set bit meaning black — the arrangement `WhiteIsZero` describes.
A file that pairs it with `BlackIsZero` gets an inverted picture, because that
is what it asked for.

Refused by name rather than half-read: `BigTIFF` (version 43), which is a
separate format with its own version marker, offset width and directory-entry
layout, and which `sniff` therefore recognises so its refusal can state the
reason; the CIE L\*a\*b\* and `LogLuv` photometrics; old-style JPEG
(compression 6) and word-aligned CCITT (32771); samples of mixed depth or
sample format, such as a 5-6-5 RGB page, since the depths claimed are the six
the format's own tables list; a fill order of 2 at any depth but one, where
reversing a byte's bits would reorder each pixel's own bits and not just the
pixels; separate planes under JPEG or under subsampled chrominance, which no
writer produces and no reader implements; the uncompressed-mode extension of
T.4, which is a bypass of the run coding rather than a part of it; and a
predictor over subsampled blocks, which have no row of samples for one to run
along.

Two fixed containment bounds hold the working set behind a page: a page count,
because a directory costs six bytes and a chain that loops revisits one for
ever, and a bits-per-pixel ceiling, because the caller's limits bound the
*picture* and without it a page could declare hundreds of samples behind each
of those pixels. The chain walk is additionally held to the directory entries
the file has room for, so it stays linear in the input, and a strip or tile's
own extent is weighed against the caller's limits like the picture is — a tile
is not bounded by the image it covers, so nothing else would bound the buffer
behind one.

### RISC OS sprite areas

A sprite area is a container of independent, *named* pictures — an
application's whole icon set in one file — so it decodes as a page container
rather than as one picture. The file holds the area control block without its
first word (the total size, which the file's own length already gives), so
every offset a file states is four greater than the position it names; the
sprites themselves form a chain of 44-byte control blocks rather than a table,
which is why the walk keeps the last block it located and a sequential pass
costs one step per page.

Complete means: old-style screen mode numbers, RISC OS 3.5 sprite mode words,
and the RISC OS 5 extended mode word with its mode-flags channel order and
alpha; 1, 2, 4, 8, 16, 24, and 32 bits per pixel across the 1:5:5:5, 5:6:5,
4:4:4:4, 8:8:8, and 8:8:8:8 packings; left- and right-hand wastage; sprite
palettes including the full 256-entry form and the short VIDC1 ones; and all
three mask forms. Indexed pixels run **least significant first** — the
leftmost pixel of a row is the low bits of its first word, the opposite of
every other format here.

Three readings the format's own text does not settle, all stated in the
module's own rustdoc:

- **A sprite with no palette does not state its colours.** RISC OS resolves
  those against whatever palette the display holds, so a file decoded away
  from a display has none to resolve against; the decoder adopts the palette
  the OS itself assigns on entering a mode of that depth. At eight bits that
  is not a table at all but the arrangement the Programmer's Reference Manual
  gives for a screen-memory byte — four bits per channel with the low two
  shared as the tint — so an 8bpp sprite needs no palette of its own to decode
  exactly.
- **A short palette is the VIDC1 arrangement, not a truncated one.** VIDC
  holds sixteen palette registers, so most 256-colour sprites carry sixteen
  entries and those written by `*ScreenSave` carry sixty-four; RISC OS passes
  the *last* sixteen to the hardware, and a pixel's top four bits then
  override supremacy bits of the entry its low four selected. A palette long
  enough for the depth is read straight through instead, which is what the
  full 256-entry form is for.
- **A mask supersedes a pixel's own alpha.** The mask is what the format calls
  a sprite's transparency, so a file carrying both is contradicting itself;
  taking the mask keeps one answer rather than inventing arithmetic over two.

An old-format (numbered-mode) mask is the image's own depth and shares its row
layout and wastage, and only whether a pixel's bits are all clear is read; a
new-format mask is one bit per pixel from bit zero of rows of its own; and a
wide mask — the mode word's top bit — is eight bits of alpha per pixel.

Sprite names are read over rather than reported: nothing addresses a sprite by
name yet, and a page index is what the shared sequence shape offers. The CMYK,
JPEG-data, and YCbCr sprite types are refused by name rather than half-read,
because none is a depth the decoder claims — as are Teletext and third-party
extension mode numbers, whose depth only the module that defined them knows.
As with an icon container, one unreadable sprite does not refuse the file: a
sprite whose *mode* the decoder does not claim is passed over when the area is
measured and refused only if it is asked for. A malformed *control block* is
fatal, because the chain is what finds the next sprite.

## Sequences and pages

Some containers hold more than one picture. `Sequence` is the one shape for
all of them:

- `Sequence::open(bytes, limits)` validates the structure — for GIF, one
  pass that walks the whole block chain, counts the frames, and reads the
  loop count; for a sprite area, one pass over the control-block chain that
  measures every sprite; for a TIFF, one pass over the directory chain that
  measures every page — and decodes no pixels. `Sequence::open_as(format,
  bytes, limits)` is the same with the format named rather than sniffed,
  which for a sprite area is the only door.
- `Sequence::info()` answers a `SequenceInfo`: the format, the geometry of the
  picture the container is, the entry count, and a `SequenceKind` of either
  `Animation { loop_count }` or `Pages`.
- `Sequence::next_frame()` decodes the next entry, lending a `Frame` carrying
  its index, geometry, declared delay in nanoseconds, and pixels.
- `Sequence::page(index)` decodes one entry directly.
- `Sequence::rewind()` restarts, which is how a loop plays again — and what
  makes it safe to step on after a refusal.

A refusal hands out no pixels and is **remembered**: stepping again answers
the same one until a rewind. An animation's frame that stopped part-way has
already had its predecessor's disposal applied and may hold part of its own
pixels, so the canvas describes no whole frame — and remembering the refusal
is what keeps a later frame from ever being composited onto it. A caller that
means to continue rewinds; one that does not simply reports the reason.

It is **forward-only with a rewind**, because that is what an animation *is*.
A GIF frame composites onto whatever its predecessors left on the logical
screen under the disposal method declared for each, so a decoder that could
be asked for frame *n* directly would have to re-composite every frame
before it — wrong per-index, and quadratic over a walk. Holding the canvas
and stepping makes each frame cost its own decode and no more, and the pixels
a `Frame` lends are the canvas *after* compositing, so a consumer shows the
whole picture without knowing the format's disposal model at all.

The pixels are borrowed rather than owned for the same reason: the canvas has
to be retained for the next frame to composite onto, so handing out an owned
buffer per step would copy the whole canvas every frame for nothing.

A **page** container's entries are independent pictures rather than one
canvas, so `page(index)` decodes any of them directly, in any order, and a
page that refuses disturbs no other. An icon file is the case that matters:
its pages are one picture at several sizes, and choosing between them is the
whole point of the format. Addressing a frame of an *animation* is still
defined — it restarts the composition and steps to that frame — but costs
exactly what a rewind and that many steps would, which is why a player steps.
For the same reason a page container weighs nothing against the caller's
limits when it opens: it allocates nothing until a page is asked for, and a
caller may well want a small page out of a file whose largest it could never
afford. `SequenceInfo`'s geometry is then the largest page's, and each
`Frame` carries its own.

A still picture is the **one-entry case** of the same shape — count `1`, kind
`Pages`, delay `0` — so a consumer that shows pictures, animations, and icon
files needs one path rather than three. `decode` on a multi-frame container
answers its first composited frame, which is the picture the format shows
first and exactly what a still consumer (an icon, a wallpaper) wants. On a
page container it answers the page that container means by its picture: an
icon file's or a sprite area's largest, and a TIFF document's first
non-thumbnail page.

## Reduced-scale decode (`decode_fitted`) is a JPEG property

`decode_fitted(bytes, limits, fit)` returns an image no smaller than it has
to be to cover the caller's `FitBox` on both axes. For JPEG it picks the
smallest DCT decode scale — one whole, one half, one quarter, or one eighth
of natural size, produced by inverse-DCT transforming only the
coefficients that scale needs, through that scale's own `m`-point basis —
whose output still covers the box. It never scales up and never resamples;
reduced dimensions round up, so the result can be modestly larger than the
box but never smaller. Decoding a 8.3-megapixel wallpaper master straight to
an eighth costs a fraction of the full-size arithmetic and output buffer.

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

### An icon container fits by choosing a page

An icon file *is* one picture at several sizes, so there is nothing to
compute: `decode_fitted` takes the smallest page covering the box on both
axes that also stays within the caller's limits, falling back to the largest
that does, and refuses only where no page does. A page is already the picture
at that size, so nothing is scaled and nothing is resampled. `decode` keeps
its own meaning there too — the picture the container is, which is its
largest page — and is refused outright when that breaches the limits rather
than quietly answering a smaller one.

### PNG, GIF, BMP, Sprite, and TIFF

None has a reduced-scale decode process — filtered zlib-compressed
scanlines, an LZW code stream, a padded row array, and a grid of strips or
tiles do not separate into scale-selectable passes — so `decode_fitted` on
those *is* `decode`, at
natural size, with no scale to degrade to. That asymmetry is an honest
property of the formats rather than a gap in this crate: a caller that wants
a smaller one resamples the decoded image through `lib/raster`'s one shared
resampler, exactly as it must to hit any size no JPEG scale lands on.
`decode` keeps its meaning for every format: natural size.

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

A GIF's frame count carries its own **fixed containment bound** of 16 384: a
frame block costs about ten bytes, so a small file can declare enormous
numbers of them, and no viewer has use for an animation longer than that. It
bounds the count the structural pass accepts and nothing is allocated per
frame — like every other bound here it is a security bound rather than a
growable capacity, and the decoder never enlarges it to make a stream fit.

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

Containment is the backstop, not the plan. A decode's row and output
buffers are sized by the file's own declared geometry — bounded by
`DecodeLimits`, but still megabytes at master resolution — so they are what
a machine short of memory refuses. They are reserved through
`tairix_util::fallible` and a refusal becomes `DecodeError::OutOfMemory`,
so the caller reads why its picture did not decode instead of watching its
sandbox die. Unlike every other variant it is a property of the machine
rather than of the input, so the same image may decode later.

## API shape

- `sniff(&[u8]) -> Option<ImageFormat>` — format identification from a
  byte signature. Never answers `Sprite`, which carries none.
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
- `probe_as(ImageFormat, &[u8])` and `decode_as(ImageFormat, &[u8],
  &DecodeLimits)` — the same as `probe` and `decode` with the format named
  rather than sniffed, which is the only door to a format that carries no
  signature.
- `FitBox::new(width, height)` with `width()`/`height()` — the caller's
  target output box, a small public copy type.
- `DecodeLimits::new(max_width, max_height, max_pixels,
  max_progressive_coefficient_bytes)` and its four accessors.
- `RasterImage::{width, height, pixels, into_pixels}` — row-major,
  4-byte-per-pixel, straight-alpha RGBA8.
- `Sequence::{open, open_as, info, next_frame, page, rewind}` with
  `SequenceInfo::{format, width, height, count, kind}`,
  `SequenceKind::{Animation, Pages}`, and
  `Frame::{index, width, height, delay_ns, pixels}` — the multi-entry shape,
  of which a still picture is the one-entry case.
- `ImageFormat::{Png, Jpeg, Gif, Bmp, Ico, Sprite, Tiff}` — the closed format
  enum.
- `DecodeError` — every fail-closed refusal reason: PNG framing and
  chunk-ordering violations, `IHDR`/`PLTE`/`tRNS` validation, a
  `CompressedData` variant wrapping `tairix_compress::zlib::Error`, and the
  `Jpeg*` family covering signature, marker, segment, quantisation- and
  Huffman-table, entropy-data, scan-header, restart-marker,
  unsupported-mode, and progressive-coefficient-store refusals; the `Gif*`
  family covering signature, version, block framing, extension, disposal,
  frame-geometry, colour-table, LZW code-size and code, and frame-count
  refusals; the `Bmp*` family covering signature, header version, geometry,
  colour planes, bit count, compression, channel masks, colour table, pixel
  offset, pixel-array length, and run-length refusals; the `Ico*` family
  covering directory signature, truncation, an empty directory, and an odd
  bitmap height; the `Sprite*` family covering a malformed area header or
  control-block chain, truncation, an empty area, an invalid mode word, an
  unsupported screen mode or sprite type, and used-bit fields leaving no whole
  pixels; the `Tiff*` family covering the byte-order mark and version,
  truncation, `BigTIFF`, an empty or looping directory chain, a missing or
  invalid tag value, an unsupported compression, photometric, bit depth,
  sample format, plane arrangement, fill order or ink set, mixed or
  over-wide samples, an invalid orientation, predictor, tile geometry,
  colour map or subsampling, a strip-count mismatch or short strip, an LZW
  code, a `TiffCompressedData` variant wrapping
  `tairix_compress::zlib::Error`, the fax code, row-overflow, truncation,
  missing-sync and uncompressed-mode refusals, and a JPEG unit of the wrong
  size; plus `OutOfMemory` for a buffer the allocator refused.

The crate is `no_std` + `alloc` and host-unit-tested beside the code with
no external fixture files: the JPEG tests build their streams marker by
marker, check a progressive stream against the pixels of the equivalent
baseline one, and check **every** inverse-DCT scale against a direct
reference the test file restates from the standard's own definition, over
many pseudo-random full coefficient blocks (asserting no more than a
one-level per-sample difference). A further test asserts the property a
scaled transform must have and a magnified corner crop cannot: reducing a
block preserves its mean. The GIF tests build their streams block by block
through one code-stream writer that mirrors the width schedule a conforming
decoder reads at, and cover every structural variant, the whole disposal
model with hand-verified canvases, an exhaustive check that the four
interlace passes cover every row exactly once, a compressed stream matched
against the literal stream of the same pixels, and every refusal — including
every prefix of a valid file being refused rather than half-decoded. The BMP
and icon tests build every header version, bit depth, encoding, row order,
and mask layout the same way, and cover the run-length escapes, the
alpha-versus-mask rule, page addressing, and a truncated container still
answering a page it wholly holds. The TIFF tests write whole documents from a
directory builder, so a fixture is a builder call and a refusal a mutation of
one, and cover every depth, sample format, photometric, plane arrangement,
predictor, orientation and compression, with the facsimile and LZW streams
stated as the bit strings ITU-T T.4 gives rather than re-derived; the fax
tables are additionally checked directly for being a prefix-free code that
holds exactly the runs the specification lists, which is the property a
hand-transcribed table most easily breaks. Every format is fuzzed by
`tests/fuzz_image.rs` — random bytes, random bytes behind
each valid signature, and structurally mutated valid fixtures (PNG chunks,
JPEG baseline and progressive marker segments, GIF blocks, icon directory
entries, TIFF directory entries — including a rewrite that hands a payload to
a compression it was not written for — and a BMP header's declared fields),
each walked through `decode`, `decode_fitted`, and a full `Sequence` pass
with a rewind — registered with `cargo xtask fuzz`. Stability tier: experimental
(`lib/image/README.md`).
