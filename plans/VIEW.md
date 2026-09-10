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

- **GIF** — LZW in GIF's variable-code-width dialect with deferred clear,
  interlacing, global and local palettes, transparent index, the full
  frame-disposal model, and the `NETSCAPE2.0` loop count.
- **BMP + ICO** — BMP written once and shared: `BITMAPCOREHEADER` through
  `BITMAPV5HEADER`, 1/2/4/8/16/24/32 bpp, RLE4/RLE8, bitfield masks, top-down
  and bottom-up rows. ICO/CUR is the directory over it, per entry, including
  PNG-compressed entries and the 1-bpp AND mask.
- **TIFF** — both byte orders; the IFD chain; strips *and* tiles; planar and
  chunky; bit depths 1/2/4/8/16/32 across the integer and float sample
  formats; photometric WhiteIsZero / BlackIsZero / RGB / palette /
  transparency-mask / CMYK / YCbCr; the horizontal and floating-point
  predictors; associated and unassociated extra-sample alpha; compressions
  none, PackBits, LZW (TIFF's dialect and the classic off-by-one variant),
  Deflate/AdobeDeflate through `tairix_compress::inflate`, CCITT G3 1D/2D and
  G4, and JPEG-in-TIFF through the existing `jpeg` module.
- **WEBP** — the RIFF container; `VP8 ` lossy (bool decoder, intra
  prediction, DCT/WHT, loop filter, YUV to RGB); `VP8L` lossless
  (meta-Huffman, colour cache, all four transforms); `ALPH` including its
  filtering methods; `VP8X`-extended files; `ANIM`/`ANMF` animation.
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
| Exact 90° rotation and flip | `lib/raster::surface` |
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
- `lib/image` TIFF, WEBP — planned, in that order (TIFF's CCITT and LZW
  codecs and WEBP's VP8 lossy decoder are each a change in their own right).
  TIFF is a page container and takes `pages` as it stands.
- `lib/svg` viewport decode; `lib/raster` rotate/flip — planned.
- `lib/sandbox::imagerender` view operations — planned.
- `userland/apps/view` engine, `Run`, bundle, 13 Help locales — planned.
- Deletion of `userland/apps/viewer` and the reference sweep — planned.
- `lib/pdf` behind the page source — next change. Encrypted PDFs need MD5/RC4/
  AES, which `lib/crypto` deliberately does not carry; whether to admit those as
  interop-only primitives or refuse encrypted files fail-closed is a decision to
  take then.

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
