# VIEW — the picture and document viewer (`View.app`)

Binding under `AGENTS.md`. What `View.app` is, which formats it claims, where
each piece of the work lives, and the seams that keep the app itself free of
decoding and of I/O.

`viewer.app` — the read-only *text* viewer that first proved the app-window and
picker-delegation paths (`plans/APPWIN.md` AW5, `plans/CAPABILITY_USE.md` CU6) —
is **deleted**, not evolved. Its purpose was a proof; its design was a text
pager holding a `ScrollModel` over sanitised lines, which is not a viewer for
pictures. Text belongs to `edit.app`, which already renders it: two apps
claiming `text/plain` would be two text-rendering paths, and the charter permits
one.

## What it is

A first-class viewer for **pictures and documents**, on par with Preview: the
thing the file manager hands a picture to (`Activation::OpenFile` /
`OpenWith`, `plans/NEW-FILEMANAGER.md`), and a standalone app that asks the
session's trusted picker when launched with no document.

`instances = "multiple"`: several documents open side by side, because
comparing two pictures is the ordinary case.

**It is a viewer.** It holds no write capability and has no editing, saving,
export, annotation, or printing. That is not an omission to be filled in later;
it is what the app is, and it is why the app needs no filesystem authority of
its own.

### Formats, and what "supported" means

JPEG, PNG, SVG, GIF, TIFF, WEBP, BMP, ICO, RISC OS Sprite, and PDF.

Every format the app claims is supported **completely** — every bit depth,
compression, colour space, and structural variant the format defines — not the
subset a common file happens to use. A format that can only be half-decoded is
not claimed at all. A file the decoder refuses fails closed to a **stated
reason drawn in the window and written to `stderr`**; never a blank surface,
and never a fabricated image.

Associations are **pictures and PDF only**. Text is `edit.app`'s.

Each new format is a private module in `lib/image`'s existing shape: `probe`
returning declared geometry; `decode` weighing `DecodeLimits` **before**
allocating a scanline, palette, or output pixel; format-namespaced
`DecodeError` variants; `#[cfg(test)] #[path = "<mod>_tests.rs"] mod tests;`
with every input synthesised in test code (the crate ships no fixtures).
`no_std`, `forbid(unsafe_code)`, fallible allocation through
`tairix_util::fallible`, checked arithmetic on every untrusted value. Each
gains a structure-aware generator in `lib/image/tests/fuzz_image.rs`, so the
already-registered `fuzz_image` target covers it with no new harness.

What "complete" means, per format:

- **JPEG** — already complete as a codec before this plan; what it lacked was
  the EXIF `Orientation` attribute every camera writes, without which a
  viewer shows every photograph on its side. It is now applied, not
  reported, exactly as TIFF's own copy of the same tag is.
- **GIF** — LZW in GIF's variable-code-width dialect with deferred clear,
  interlacing, global and local palettes, transparent index, the full
  frame-disposal model, and the `NETSCAPE2.0` loop count.
- **BMP + ICO** — BMP written once and shared: `BITMAPCOREHEADER` through
  `BITMAPV5HEADER`, 1/2/4/8/16/24/32 bpp, RLE4/RLE8, bitfield masks, top-down
  and bottom-up rows. ICO/CUR is the directory over it, per entry, including
  PNG-compressed entries and the 1-bpp AND mask.
- **TIFF 6.0** — both byte orders; the IFD chain; strips *and* tiles; planar
  and chunky; bit depths 1/2/4/8/16/32 across the unsigned, signed, and IEEE
  float sample formats (float at the two widths a float has, 16 and 32);
  photometric WhiteIsZero / BlackIsZero / RGB / palette / transparency-mask /
  CMYK / YCbCr, the last with its subsampling, luma weights and coded ranges;
  the horizontal and floating-point predictors; associated and unassociated
  extra-sample alpha; the Orientation tag; compressions none, PackBits, LZW
  (TIFF's dialect and the classic variant), Deflate/AdobeDeflate through
  `tairix_compress::zlib` (TIFF's Deflate is zlib-wrapped, not raw), CCITT
  G3 1D/2D and G4, and JPEG-in-TIFF through the existing `jpeg` module.

  Three readings the format does not settle, and one it does not have:
  - **A plain decode answers the first page the file does not call a reduced
    copy of another** (`NewSubfileType`, or the superseded `SubfileType`). A
    TIFF is an ordered *document*, not one picture at several sizes, so its
    picture is its first page — where an icon file's and a sprite area's is
    their largest. Reading the file's own thumbnail declaration beats
    guessing from size, which would answer page two of a mixed-paper scan.
    `SequenceInfo` still reports the largest page's geometry, because that is
    the canvas a container needs, so it and `probe` may differ for a TIFF
    where they agree for the other two.
  - **Orientation is applied, not reported.** A decoder that ignored it would
    hand every consumer a sideways picture *and* the format knowledge to
    right it. A transposing orientation therefore swaps the geometry `probe`
    answers, and the permutation is free: each sample is written where it
    belongs rather than moved afterwards.
  - **A missing photometric under a fax compression reads as WhiteIsZero.**
    Every fax is; the tag is otherwise required and nothing else defaults.
  - **TIFF has a signature, so it enters `sniff`** — `II*\0` / `MM\0*`, and
    nothing else in the sniff order can shadow it (PNG opens `0x89`, JPEG
    `0xFF`, GIF `G`, BMP `BM` = `0x42 0x4D`, an icon `0x00 0x00`).

  Refused by name rather than half-read: **BigTIFF** (version 43) — a
  separate format with its own version marker, offset width and directory
  layout, so claiming it would mean claiming it completely, and `sniff`
  recognises it precisely so the refusal can state that rather than reading
  as no format at all; the CIE L\*a\*b\* and LogLuv photometrics; old-style
  JPEG (compression 6) and word-aligned CCITT (32771); samples of mixed depth
  or sample format, such as a 5-6-5 RGB page, since the claimed depths are
  the six the format's own tables list; a fill order of 2 at any depth but
  one, where reversing a byte's bits would reorder each pixel's own bits and
  not just the pixels; separate planes under JPEG or subsampled chrominance,
  which no writer produces and no reader implements; T.4's uncompressed-mode
  extension, a bypass of the run coding rather than a part of it; and a
  predictor over subsampled blocks, which have no row of samples to run
  along.
- **WEBP** — the RIFF container in both its simple and `VP8X`-extended
  forms; `VP8 ` lossy (the bool decoder; the segmentation, loop-filter and
  quantiser headers with their per-segment and per-mode deltas; the token
  probability updates; the four whole-macroblock luma modes, the four chroma
  modes, and all ten subblock modes; DCT and WHT reconstruction; both the
  normal and the simple loop filter, at macroblock and at subblock edges;
  and the conversion to RGB); `VP8L` lossless (the prefix-coded image
  stream, meta-Huffman through the entropy image, the colour cache, the LZ77
  backward references with their distance mapping, and all four transforms —
  predictor with every one of its fourteen predictors, colour, subtract
  green, and colour indexing with its pixel bundling); `ALPH` at both
  compression methods and all four filtering methods; and `ANIM`/`ANMF`
  animation under the container's own blend and dispose model.

  Five readings the format leaves to the decoder:
  - **The container's canvas is authoritative, and a frame whose own
    bitstream disagrees with the rectangle it was given is refused.** A
    `VP8X` canvas is a 24-bit declaration while a `VP8 `/`VP8L` bitstream
    carries its own 14-bit one, so the two can disagree — and reconciling
    them would mean cropping, padding, or scaling, none of which is what
    either declaration says. Refusing is the only answer that invents no
    pixels. A simple-format file has no container geometry at all, so there
    its bitstream's own dimensions *are* the canvas. `probe` and
    `SequenceInfo` therefore always agree for a WEBP, where they may differ
    for a TIFF.
  - **Nothing is sized from an `ANMF`'s own declaration.** Its 24-bit frame
    extent is checked to lie inside the canvas — which allocates nothing —
    and the frame is then decoded at the size its *payload* declares and
    refused unless the two match. A tile's extent is not bounded by the
    picture it covers, and neither is a frame's.
  - **The kind a WEBP is comes from the file, not from the format.** A file
    carrying `ANIM` is an animation, with `ANIM`'s loop count (`0` meaning
    for ever) and the retained-canvas stepping disposal forces. One without
    is a still picture — the one-page case, exactly as a PNG is — because
    there is no loop count to report and answering "for ever" would be
    fabricating a declaration the file does not make.
  - **The canvas clears and disposes to fully transparent, ignoring `ANIM`'s
    background colour.** The specification makes that colour explicitly
    optional, and a straight-alpha decoder's job is to carry the file's
    transparency out to its consumer rather than pre-flatten it against a
    colour the viewer is going to draw its own checkerboard behind. This is
    the same reading the GIF decoder already takes of *restore to
    background*.
  - **`VP8X`'s alpha and metadata flags are a demuxer's hints; the chunks
    present are the fact.** The alpha, ICC, EXIF and XMP flags say what a
    file "contains", and every real encoder sets them consistently, but the
    decode is driven by the chunks actually found — so a set alpha flag with
    no chunk decodes, and a chunk with a clear flag decodes, rather than
    either being refused over a disagreement that costs nothing. The
    **animation** flag is not a hint: it is what says whether the file is an
    animation at all, so it and the chunks must agree. The reserved bits and
    reserved field values are refused either way.

  Colour profiles are read past, not applied: `ICCP`, `EXIF`, and `XMP `
  are skipped like any unknown chunk, because no decoder in this crate
  colour-manages and the output is RGBA8 in the file's own primaries.

  Refused by name rather than half-read: a `VP8 ` **interframe**, since the
  container specification requires the bitstream to be a keyframe, and an
  interframe's prediction is against reference frames a WEBP never carries;
  `ALPH` beside a `VP8L` bitstream, which carries its own alpha; a `VP8L`
  version other than zero; an `ALPH` reserved compression method,
  pre-processing value, or reserved bit; and an `ANIM`, `ANMF`, or `ALPH`
  chunk in a file with no `VP8X`, which the extended form is what defines.
  The lossy bitstream's `horizontal`/`vertical scale` fields and `ALPH`'s
  pre-processing field are read, validated, and then not acted on: all three
  are display hints, and the specification calls the last informative, so
  applying a smoothing or an upscale would fabricate pixels the file does
  not hold.

  WEBP has a signature, so it enters `sniff` — but a two-part one, `RIFF`
  at 0 and `WEBP` at 8 with the RIFF size between, which is why the format's
  own module answers the test rather than `lib.rs` matching a constant.
  Nothing in the sniff order can shadow it or be shadowed by it: no other
  signature opens with `R`, and requiring both halves means a bare RIFF file
  of some other form is not a WEBP.

  Neither codec has a reduced-scale decode process, so `decode_fitted` on a
  WEBP is exactly `decode`. That is not the absence of a JPEG-style
  scaled-IDCT path but a property of the formats: VP8's intra prediction
  reads full-resolution neighbours, so a coarser transform would change the
  prediction and decode a *different* picture rather than a softer one, and
  VP8L's spatial predictors and backward references have the same
  dependency on the pixels already produced.
- **RISC OS Sprite** — the sprite-area header and control-block chain,
  left/right wastage, old-style mode numbers, RISC OS 3.5 sprite mode words,
  the RISC OS 5 extended mode words with their mode-flags channel order and
  alpha, 1/2/4/8/16/24/32 bpp across the 1:5:5:5, 5:6:5, 4:4:4:4, 8:8:8 and
  8:8:8:8 packings, sprite palettes including the full 256-entry form and the
  short VIDC1 ones, and all three mask forms — an old-format mask at the
  image's own depth, a new-format 1-bit mask, and a wide 8-bit alpha mask.
  `plans/RISCOS-EMULATOR.md` already specifies a sandboxed sprite *data*
  decode for `!Sprites22`/`!Sprites` icon loading, so this decoder has a
  second planned consumer and belongs in the shared crate.

### A format with no signature is named, never guessed

A RISC OS sprite area carries no magic number — its first word is the sprite
count, and RISC OS types a file from its directory entry, not its content — so
`sniff` cannot recognise one and **must not try**. A structural-plausibility
heuristic would be a false-positive machine, and one the crate would then act
on; both planned consumers already know the type without it (the emulator
loads `!Sprites22`/`!Sprites` by name and filetype, and `lib/browse::media`
already maps `.spr` to `image/x-riscos-sprite`).

So `ImageFormat::Sprite` exists — the crate must be able to *name* what it
decoded — and `sniff` never answers it. The caller that knows the type reaches
the decoder through `probe_as` / `decode_as` / `Sequence::open_as`, which take
the format in place of the sniff; the sniffing entry points are exactly those
plus `sniff`, so there is one dispatch table rather than two. The named
format's own parser still validates the bytes, so naming the wrong one is
refused rather than misread.

The sprite *name* is not surfaced. Nothing addresses a sprite by name yet, and
a `Frame::name` would be `None` for every other format — speculative surface.
Name-addressed lookup lands with the emulator, its first consumer.

## Where each piece lives, and why there

| Piece | Home |
|---|---|
| GIF, TIFF, WEBP, BMP, ICO, Sprite decoders | `lib/image`, private modules |
| Multi-frame / multi-page decode | `lib/image` sequence API |
| Exact 90° rotation and flip | `lib/raster::reorient` (`Surface::reoriented`) |
| Viewport-targeted SVG rasterisation | `lib/svg`, `decode` evolved in place |
| Untrusted decode | `lib/sandbox::imagerender` |
| The app-side window shell | `lib/window::app` |
| The app | `userland/apps/view` |
| PDF | `lib/pdf`, behind the page source (next change) |

`lib/image` is already *the* raster registry: `ImageFormat`/`sniff`/`probe`/
`decode` dispatch, and `DecodeLimits`/`RasterImage`/`DecodeError` carry the
fail-closed discipline. Sibling crates would duplicate all of it, and a format
that lands here needs no second decoder for any other consumer that later
admits it — though admitting one stays that consumer's decision: the icon
pipeline deliberately takes only PNG and SVG (`plans/ICONS.md`).

A format is one module, except where it is genuinely more than one codec.
WEBP is three — `webp` for the container, the alpha plane, and the
compositing, and `vp8` and `vp8l` for the two bitstreams — on the precedent
the facsimile codec set: a self-contained codec with its own bitstream,
tables, and refusals earns its own module. Neither codec module knows the
container, and neither knows the other: `ALPH`'s compressed method *is* a
lossless stream over the alpha plane, so the container is what reaches for
`vp8l` on the lossy path, which keeps the container's rules in the container
and leaves both codecs testable on their own.

The rotation is a pixel permutation, so it belongs to the one rasterisation
path the charter allows; an app-local copy would be a second one.

Nothing is needed in the content-type registry — `lib/browse::media` already
carries every one of these types with an icon. Only the bundle's own
`associations` list is new.

## The page source — one seam, every document

The engine reaches every document through **one** seam, so `lib/pdf` drops in
behind it unchanged:

- page/frame count,
- per-page natural geometry and metadata,
- render one page at a target size.

Designed before PDF exists precisely so PDF needs no rework, and shaped so a
still picture is the one-page case rather than a special case.

### Sequences are stateful, because disposal makes them so

A GIF or animated WEBP frame is composited **onto its predecessors** under the
format's disposal model, so a per-index decode would be both wrong and O(n²).
The sequence decoder therefore holds the composition canvas and yields
composited frames in order: stepping is O(1) amortised. Page-addressed formats
(TIFF, ICO, PDF) expose the same shape with independent entries, and add
addressed access when the first of them lands.

The canvas is why a refusal is **remembered**: a frame that stopped part-way
has already had its predecessor's disposal applied and may hold part of its
own pixels, so nothing on the canvas describes a whole frame any more.
`next_frame` therefore answers the same refusal until `rewind`, and the app's
playback loop rewinds (or stops and states the reason) rather than stepping
on. No pixels ever cross a refusal.

### Animation is one-shot and tickless

Playback waits with `waitset_wait(set, timeout_ns, …)` against a deadline from
`clock_get()`: the loop wakes on the next window event **or** the frame
deadline, whichever is first. No periodic tick, no spin, and no timer armed
while playback is paused.

## Decoding untrusted files

A viewer must never decode a file in its own address space, and the worker must
hold no filesystem reach at all.

`SpawnAttach` wires exactly the four standard streams, so there is no slot to
hand a worker a document descriptor — and handing it one would widen the
minimum-capability sandbox anyway. So:

1. `Run` holds the read-only descriptor it was given (inherited at spawn, or
   redeemed from the picker's one-shot grant).
2. It reads the bytes under a **fixed input-byte ceiling** — a containment
   bound, not a capacity, so it does not scale with the machine.
3. It streams them into the worker in `proto::MAX_FRAME`-bounded frames.
4. It drives probe and per-page render requests, collecting pixels through the
   band protocol `imagerender` already defines.

The parent trusts nothing about a reply beyond its length and echoed geometry.
`fs_read` is positional, so the ceiling bounds what is **resident**, not what is
addressable.

## The app

**Engine (`src/lib.rs`)** — host-tested, no window, no I/O:

- the document model over the page source;
- a viewport carrying continuous zoom, fit modes (fit, fit-width, actual size),
  clamped pan, and rotation/flip;
- one `Layout::for_window(w, h, theme, scale)` producing every `Rect` that
  render, hit-test, and the tests read, so the three cannot disagree;
- renderers that **paint only from state**;
- a decoded-surface cache under `lib/reclaim`'s budget and pressure bands;
- one pure `InputEvent` entry point returning state change plus damage;
- a request/answer desk `Run` services — **the engine performs no I/O**.

Composed from the existing `lib/controls`, painting no control of its own: a
`Toolbar` of `IconButton`s, two `ScrollBar`s, a `Slider` for zoom, `IconTile`s
in a `Panel` for the thumbnail sidebar, a `FactList` for the info panel, and
`damage::set` / `damage::move_mark` for every guarded write.

**`Run` (`src/run.rs`)** over the shared shell: the app-bar declaration, the
wait-set extended with the worker channel and the animation deadline, document
acquisition by `DOCUMENT_ROLE_ARG` + `STDIN` or `pick_file`, menus via
`open_menu` + `AppMenu` (session-owned plates — the app draws no menu pixel),
`set_tooltip` for the toolbar, and `Scrolled { dx, dy }` for the wheel.

Behaviour: zoom in/out/fit/actual size, drag pan with scrollbars when zoomed
in, rotate and flip, page/frame navigation with a thumbnail sidebar, animation
play/pause and frame step, an alpha checkerboard, ICO size selection, an info
panel (format, geometry, bit depth, colour space, frame/page count, file size,
and per-format specifics — GIF loop count, TIFF compression, Sprite mode), an
app-declared menu, a context menu, and keyboard equivalents throughout.

## `lib/window::app` — the shared app shell

The windowed-app boilerplate was copy-pasted across seven `Run` binaries: the
`WindowTransport` impl over `ipc_call(WINDOW_ENDPOINT, …)` was **byte-identical**
in all seven. `lib/window` already owns the app half of the channel, so the rest
belongs beside it rather than in an eighth copy.

Shared: the transport; the base park (event mailbox + memory-pressure band) an
app's own `EventSource` calls; `bind_event_mailbox`; `bring_up_desktop`;
`mode_for`/`region_bytes`; the four reserved exit codes and `fail`; one
window's `WindowPane`; and the single-window `AppWindow` that pairs a pane
with the retained `Surface` and takes the paint as a closure.

`WindowPane` is **one window**, however many the app has: its id, its shared
frame region, and the layout both are shaped as, with the create dance
(`open`, `open_popup`), `present`, `resize`, `release_frames`, and `close`. It
holds **no picture** — a plain `Surface` for most apps, a screen model
carrying its own cell diff for the terminal — because a pane that owned a
surface would force a second window-sized allocation on every app whose
retained picture is not literally one.

The **resize ordering is the load-bearing part**: allocate the spare surface,
create the new frame region, grant it, ask the server to resize, and only then
swap. A refusal at any step leaves the old geometry standing, so a refused
resize is an answer rather than a broken window.

Apps keep what genuinely differs — their own extra wait-set members and the
tokens for them. The shell reserves the base tokens so an app's own cannot
collide.

`userland/gui/session` and `userland/session/greeter` are **out of scope**: they
are the window *server* and the pre-session login surface, not app-side
clients, and their exit-code sets are their own.

## Status

- `plans/VIEW.md` and the jump-sheet row — **done**.
- `lib/window::app` shared shell, and the migration of every other app-side
  consumer — **done**. `datetime`, `widgets`, `wallpaper`, and `switchboard`
  are single-window and took `AppWindow` whole; `datetime` additionally stopped
  allocating a window-sized surface per paint, because painting into the
  retained one is what the shell offers. `files` and `terminal` are
  multi-window and hold `WindowPane`s — one per window, and per popup for the
  terminal's settings sheet — beside their own retained pictures, which is why
  the pane owns no surface.
- `lib/image` sequence API (`Sequence`/`SequenceInfo`/`SequenceKind`/`Frame`),
  with the still picture as its one-entry case — **done**. Stepping is
  forward-only with a rewind, because disposal makes an animation exactly
  that; `page(index)` addresses a page container's entries directly, and is
  total over every kind (addressing an animation's frame restarts the
  composition and steps to it, which is what a frame *n* means when frames
  composite).
- `lib/image` GIF — **done**, complete as specified above, with the whole
  disposal model, the deferred clear, interlacing, and a structure-aware fuzz
  generator. `decode` on a GIF answers its first composited frame, which is
  what a still consumer wants; whether the icon or wallpaper pipeline admits
  the format stays their own decision, and neither does today.
- `lib/image` BMP and ICO/CUR — **done**, complete as specified above. BMP is
  the shared decoder and ICO the directory over it, so an icon entry is either
  a DIB that decoder reads or a whole PNG file through the existing `png`
  module — never a second decoder for either. Two de-facto readings the
  format's own text does not give are stated in the module rustdoc and the
  crate docs: a 32-bit `BI_RGB` pixel's undefined fourth byte is ignored in a
  BMP file and read as alpha in an icon (the file header tells the cases
  apart), and an icon whose alpha is zero everywhere falls back to its 1-bit
  mask. The OS/2 2.x header lengths are refused by name rather than half-read,
  because they reuse compression codes 3 and 4 for Huffman 1D and RLE24.
  `Sequence` gained the addressed access page-addressed formats were always
  going to need — `page(index)`, total over every kind — and a page container
  weighs nothing against the caller's limits when it opens, because it
  allocates nothing until a page is asked for and a caller may want a small
  page out of a file whose largest it could never afford.
- `lib/image` RISC OS Sprite — **done**, complete as specified above. The
  format is reached only by being named, for the reason above. Two crate
  units were lifted out of their single homes so the sprite decoder shares
  them rather than carrying a second copy, and so the page-addressed formats
  still to come do too: `channel` (a packed pixel's channel fields and the
  sampler that widens one to eight bits, from `bmp`) and `pages` (the
  `PageSource` trait plus the cursor, remembered refusal, and one retained
  decode a page container's walk is, from `ico`). Three readings the format's
  own text does not settle are stated in the module rustdoc and the crate
  docs: a sprite with no palette is resolved against the palette the OS
  assigns on entering a mode of that depth (at eight bits the screen-memory
  byte's own tint arrangement, so it is exact); a palette shorter than the
  depth needs is the VIDC1 arrangement, its last sixteen entries being the
  hardware registers with a pixel's top four bits overriding supremacy bits;
  and a mask supersedes a pixel's own alpha. Indexed pixels run least
  significant first, the opposite of every other format here. The CMYK,
  JPEG-data, and YCbCr sprite types are refused by name rather than
  half-read, as are Teletext and third-party extension mode numbers. Because
  the format has no signature, the fuzz harness drives *every* input it
  already builds through the naming door as well, so the sprite decoder gets
  the whole corpus rather than only its own.
- `lib/image` TIFF — **done**, complete as specified above, with the
  facsimile codec (`ccitt`) and both LZW dialects. It is a page container and
  took `pages` as it stood. One more crate unit was lifted out of its single
  home so the two formats that carry an LZW stream share it rather than each
  holding a subtly different copy: `lzw` (the dictionary, the string walk,
  the entry a code defines for itself, and the deferred clear, from `gif`),
  parameterised by the two things the dialects actually disagree on — how
  codes are packed into bytes and when a new entry widens the code that
  follows it. TIFF's own dialect widens one code early; the classic one older
  writers emit packs least significant bit first *and* widens at the later
  point, and the two differences always travel together, so a stream's
  opening clear code (`0x80 0x00` against `0x00 0x01`) tells them apart
  exactly. A tile's own extent is weighed against the caller's limits like
  the picture is, because a tile is not bounded by the image it covers — a
  fixed 256-pixel tile over a 16-pixel image is what a real writer produces —
  so without that nothing bounds the buffer behind one.
- `lib/image` WEBP — **done**, complete as specified above. Three modules,
  because two of the three pieces are self-contained codecs: `webp` for the
  container, the alpha plane and the compositing, `vp8` for the lossy
  bitstream, and `vp8l` for the lossless one. Neither codec knows the
  container and neither knows the other — the container is what reaches for
  the lossless codec on the *lossy* path, because a compressed `ALPH` chunk
  is a lossless stream over the alpha plane.

  The container is the one format here that is either a still picture or an
  animation, and the file says which: carrying `ANIM` makes it an animation
  and it took the shared animation walk, while carrying none makes it the
  one-page still case a PNG already is. That walk is the fourth crate unit
  lifted out of its single home so a second format shares it rather than
  copying it: `frames` (the cursor, the remembered refusal, the declared
  geometry and the delay most recently read, from `gif`), leaving each
  container only its own disposal model. A fifth was lifted for the same
  reason: `huffman`, the canonical prefix-code assignment and the walk that
  decodes one, which a JPEG Huffman table and a lossless WEBP's prefix codes
  are the same construction of. It holds a code as its length counts and
  nothing else, because a lossless WEBP carries one prefix code per channel
  per meta-Huffman group and a file may declare tens of thousands of groups:
  at that count a table of precomputed per-length values would cost hundreds
  of times the bytes the stream declaring them occupies. The group count is
  additionally held to what the remaining bits could describe, because a
  sparse entropy image can name a high group while spending almost nothing.

  What the tests turn on is that the fixtures are written from each
  specification rather than by inverting the decoder beside them. The lossy
  writer is RFC 6386's own *encoder*, checked by round-tripping several
  hundred random probability-and-choice sequences, and on top of that the
  tests pin absolute pixels: a flat keyframe fills at 128 and converts to
  exactly 130 per channel, and one luma DC token of four spreads through the
  Walsh-Hadamard transform to lift every sample by one. Four spec-table
  transcription errors were caught mechanically by diffing against the
  published tables rather than by any test — which is why the tables are
  extracted rather than typed.
- `lib/image` orientation — **done**, and not previously in this plan. TIFF
  applied its `Orientation` tag; JPEG never read the EXIF attribute that is
  the same tag number with the same eight values, so every photograph a
  camera turned opened on its side. The eight-case position map moved out of
  `tiff` into a shared `orientation` unit — the eleventh — and `jpeg` now
  reads the `APP1` block through it. The attribute swaps the geometry
  `probe` reports, the axes a `decode_fitted` box is measured against, and
  the dimensions the limits are checked against, so a caller is told about
  the picture rather than the raster. Placing it costs nothing per picture:
  a decoded row is copied whole when there is no orientation, and scattered
  pixel by pixel only when there is.

  Metadata is advisory, so an absent, truncated, or malformed block leaves
  the picture as stored rather than refusing the file — deliberately unlike
  TIFF, where the tag sits in the directory describing the pixels being
  decoded and a bad value means the file cannot be read at all. A camera's
  malformed metadata must not cost a reader the photograph. The reader
  follows neither the next-directory pointer nor any sub-directory, so it is
  one bounded pass that allocates nothing, and the JPEG fuzz generator now
  emits EXIF blocks — in both byte orders, with undefined values and
  over-declared entry counts — so mutation reaches it.
- `lib/svg` viewport decode; `lib/raster` rotate/flip — **done**.

  `lib/raster` gained `Reorient`: the eight ways a picture can be set down
  without resampling it, as a group rather than a handful of methods. A
  viewer holds one value and composes onto it (`then`), so a picture the
  user has both turned and mirrored is still set down in **one** pass rather
  than two allocations and two copies; `inverse` is what a pointer position
  is read back through. `Surface::reoriented` allocates and returns `Option`
  like the rest of the crate, because a turn that swaps the axes cannot be
  done in place on an oblong and a caller-supplied destination would make
  every caller agree its geometry. It reads each destination pixel back
  through the undoing rather than scattering each source pixel forward, so
  the writes run along the destination's rows and the identity needs no
  special case to stay fast.

  `lib/svg`'s `decode` took a `Viewport` in place, and it chooses only the
  *shape* a drawing is fitted to. `Square` is the existing letter-boxed slot
  every icon and cursor wants. `Natural` normalises the drawing across both
  axes, and `source_extent()` carries the proportions it was authored in —
  so rasterising into a surface of that shape gives the picture undistorted,
  at the grid's full precision on both axes, with no bands to find and crop.
  That works because `Surface::fill_contours` already stretches the grid
  across the surface it is given, so normalising in the decoder and
  un-normalising in the surface is one uniform scale: **no non-square design
  grid, and no change to the scan converter**.

  One consequence is real and is not papered over: filling both axes
  flattens curves to the larger scale's tolerance, so a drawing close to the
  total-vertex bound can pass it under `Natural` and be admitted under
  `Square`. The bound is a containment bound and is not relaxed to suit a
  shape; the SVG fuzz harness drives both viewports and asserts they can
  disagree *only* about complexity.

### Outstanding: SVG is not yet a view backend — blocked on a decision

The wire protocol is source-neutral by design — open, page, render, band —
but the worker currently has exactly **one** backend behind it, the
`lib/image` `Sequence`. So the eight raster formats open and SVG does not:
`tairix_image::sniff` never answers `Svg`, and an SVG document handed to
`OP_VIEW_OPEN` is refused as an unrecognised format. `Viewport::Natural`
therefore still has **no** production caller; the note above that it gained
one here was wrong.

Adding SVG is the second backend, and it is not a patch. Three things have
to be decided rather than guessed:

- **A view format vocabulary.** `ViewDocument::format` is
  `tairix_image::ImageFormat`, which is a *raster* registry and must not
  grow an `Svg` variant it cannot decode. The wire needs its own enum over
  the raster formats plus the vector one.
- **How a vector crop is drawn.** `Surface::fill_contours` stretches the
  design grid across the whole surface it is given, so it renders a
  drawing, not a *rectangle* of one. Rasterising the whole drawing at the
  zoom factor and taking the sub-rectangle is trivial and allocates the
  full zoomed picture — which at a viewer's zoom is exactly the unbounded
  cost the crop-based render exists to avoid. Transforming the contours
  per render is bounded but needs the placement to live in one shared
  place rather than beside the icon path's copy.
- **A non-square vector rasterise.** `VectorIcon::rasterise(side)` is
  square because every icon slot is. A picture is not.

Until that is settled the viewer claims eight formats, not nine. This is
recorded rather than stubbed: nothing in the tree pretends to open an SVG
document.

### `MAX_VIEW_DECODE_PIXELS` is set by what a viewer must open

A viewer's page bound is deliberately *not* sized to a particular machine:
sixty-four megapixels sits above the top of the current 35 mm camera range,
so no photograph a user owns is refused for being a photograph. What a small
machine can actually hold is enforced where it belongs — the decode
allocates fallibly and answers a typed refusal the viewer draws — rather
than by a ceiling a larger machine would outgrow. A page beyond the bound is
refused with a stated reason rather than served at a reduced scale, which is
the fail-closed answer; serving it softer would need a *fitted* page decode,
and `decode_fitted`'s reduced scales are a JPEG property that a TIFF page or
an icon entry has no equivalent of.

### The two eight-case maps are not one, and that is deliberate

`lib/image`'s `orientation` and `lib/raster`'s `Reorient` both express the
eight symmetries of a rectangle, and they are deliberately **not** unified.
They share no value that could drift: the eight permutations are a
mathematical fact each proves for itself, not a project constant. What they
do not share is everything else — one is keyed to the wire numbering of a
TIFF/EXIF tag and applies during a decode as a write address, the other is
keyed to a user's intent and carries a composition law and an inverse no
decoder needs.

Unifying them would mean either an `lib/image` → `lib/raster` dependency —
dragging the theme, reclaim and parallel crates into a decoder that today
depends on two crates and runs inside a sandbox — or a third crate holding
ten lines of index arithmetic. Both are worse than two small, separately
exhaustive tests. A future reader tempted to "fix" this duplication should
read this paragraph first; the rustdoc on both types points here.
- `lib/sandbox::imagerender` view operations — **done**. The view is a
  *session*, not a one-shot render, because a viewer holds a file open and
  moves about inside it: `OP_VIEW_OPEN` answers what the container
  declares, `OP_VIEW_PAGE` decodes one entry and the worker holds it,
  `OP_VIEW_RENDER` fixes which *rectangle* of that held page is drawn onto
  which destination, `OP_VIEW_BAND` returns exactly the destination rows
  asked for, and `OP_VIEW_RELEASE` drops both. Two properties fall out of
  that shape, and both are the point of it: a zoomed-in viewer sends the
  crop it is showing, so the work and the reply are bounded by the window
  rather than by the picture; and the decoded page stays in the worker, so
  panning and zooming re-draw rather than re-decode.

  Holding the walk across requests is what made this change larger than
  the protocol. `Sequence<'a>` borrowed the document, and a worker cannot
  own both a buffer and a borrow of it — not in safe Rust, and both crates
  forbid `unsafe`. Rebuilding the walk per request was the alternative and
  is not one: an animation's frames composite onto their predecessors, so
  playing a hundred-frame animation through would have cost five thousand
  frame decodes. So `Sequence<B: AsRef<[u8]>>` now **owns** what it reads —
  `&[u8]` keeps a borrowing caller zero-copy, `Vec<u8>` lets the walk
  outlive whatever produced the bytes — and no format's chain holds a
  borrow any more: each holds offsets and is handed the document per call.
  `Sequence::current()` came with it, lending the entry already decoded so
  a band draws the page the walk holds rather than a second copy of it.

  Addressing a frame stopped being quadratic in the same change.
  `page(index)` rewound and replayed unconditionally; the canvas already
  holds the frame before the cursor, so reaching a later one now composites
  only the frames in between, and walking an animation through by address
  costs what stepping does. Only going back restarts, and so does a
  remembered refusal — which is what keeps a later frame from ever being
  composited onto a canvas holding part of a refused one.

  **The viewer's own rotation and flip are not in the sandbox**, and the
  earlier note in this plan that they would be is wrong. They are a
  permutation of pixels the caller already holds and has validated, not a
  decode, so they belong to whatever holds the picture: `Surface`'s own
  rustdoc says a viewer should turn what it *displays*, and what it
  displays is a premultiplied `Surface`, where the wire is straight alpha —
  turning in the worker would mean a lossy round trip through premultiply
  and back for no gain. `Reorient`/`Surface::reoriented` therefore get
  their production caller in the app (step 7), not here.

  One defect was found and fixed on the way. Every untrusted file now
  reaches this service one way — `OP_DOC_BEGIN` declares a length and
  `OP_DOC_PUSH` carries it in pieces of at most `MAX_DOCUMENT_CHUNK`,
  derived from `MAX_FRAME` rather than chosen — and the wallpaper prepare,
  which used to carry its whole source inline, was moved onto it.
  `MAX_WALLPAPER_BYTES` and `MAX_FRAME` are both exactly 8 MiB, so a
  wallpaper in the top 22 byte-lengths of its own documented bound was
  admitted by the caller's check and then refused by the transport: the
  user got the backdrop colour instead of the picture they chose. Deriving
  the chunk size from the frame bound is what makes that unrepresentable,
  and a second upload path beside it would have been the duplication the
  charter forbids anyway. The destination bound was unified for the same
  reason: `MAX_DESTINATION_WIDTH`/`_HEIGHT`/`_PIXELS` is one figure every
  consumer of this service asks the same question of.
- `userland/apps/view` engine, `Run`, bundle, 13 Help locales — planned.
- Deletion of `userland/apps/viewer` and the reference sweep — planned.
- `lib/pdf` behind the page source — next change. Encrypted PDFs need MD5/RC4/
  AES, which `lib/crypto` deliberately does not carry; whether to admit those as
  interop-only primitives or refuse encrypted files fail-closed is a decision to
  take then.

## Noticed and not yet fixed

- **`lib/sandbox` allocates its bounded buffers infallibly.** Every band,
  destination, and frame buffer in the crate is `vec![0u8; n]`
  (`proto.rs`, `decode.rs`, `imagerender.rs`), which aborts rather than
  answering when the memory is not there. Inside a worker that is
  contained — the sandbox reports a typed failure and replaces it — but
  `render_wallpaper`'s parent-side assembly allocates the whole
  destination, up to 33 MiB, *in the calling desktop session*, so a
  session under memory pressure dies rather than falling back to its
  backdrop colour. The view's own parent side already avoids this by
  taking the caller's buffer (`render_page`), and the document upload
  reserves fallibly; the rest is a crate-wide allocation-discipline
  change spanning four modules and is not smuggled into this one. It
  carries its regression test when it lands.

- **WEBP does not apply an EXIF `Orientation` either.** The container reads
  past its `EXIF` chunk, exactly as `jpeg` used to read past `APP1`. Now
  that JPEG and TIFF both apply the tag, WEBP not applying it is an
  inconsistency between three formats that should agree. It was left out of
  the change that fixed JPEG because applying an orientation to an
  *animation* is not the same job: the composition canvas, the per-frame
  rectangles, and the declared canvas geometry would all have to be turned
  together, which is the animation path rather than a row placement. The
  shared reader (`orientation::from_exif`) is already there to be called.
  This is recorded rather than deferred silently, and carries its regression
  test when the fix lands.

## Verification

Per-format decoder tests over valid, malformed, truncated, and adversarial
input — every bit depth, compression, photometric, and disposal path,
degenerate geometry, overflow edges, and limits refused *before* allocation.
Structure-aware generators per format in the already-registered `fuzz_image`
harness. `lib/sandbox` loopback tests including a hostile worker reply (wrong
echoed geometry, wrong pixel length) being refused. Engine host tests for
viewport clamping, rotation/flip composition, layout at several `Scale`s, input
routing, cache eviction across pressure bands, animation deadlines, the
request/answer desk, and the three damage-correctness properties every app owes.
The re-pointed `filepick_qemu_aarch64` vertical, extended to open a real image
end to end from Files.
