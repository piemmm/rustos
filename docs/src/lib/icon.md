# tairix-icon

The shared desktop-icon library (`lib/icon`, `AGENTS.md` §6 / §10 — `PLAN.md`
Stage 7). It lives in `lib/*` so the taskbar, the desktop session, and the
file manager all draw icons without depending on one another (`AGENTS.md`
§17.4). The crate is `no_std`, `#![forbid(unsafe_code)]`, and owns no scan
converter, colour arithmetic, or glyph rasteriser of its own — it draws
through `lib/raster`'s one `Surface::fill_polygon` path and its one
`Surface::layered` composition, exactly like `lib/cursor`, and marks an
account's disc through `lib/font`.

The built-in glyph representation (`VectorIcon`, `IconLayer`, `IconKind`,
`builtin_icon`, the SVG-first `IconSet`/`IconAssetSource` loader) and an
account's circular identity disc (`monogram_of`, `monogram_disc`) are described
under [Desktop icons](../desktop/icons.md). This page covers the crate's
**asset model** and the `artwork` layer that resolves it.

## The asset model

Every request resolves to drawable pixels through on-disk asset tiers over an
always-present built-in floor, tried in order, so resolution is **total** — a
draw site always gets something:

0. **The thing's own icon (preferred, where it has one).** An application
   bundle names an icon inside its own `Resources/` in its signed `AppInfo`
   manifest, so `ls.app` draws `ls`'s picture and not the generic
   every-application picture. Every app ships one — an SVG by preference, else
   a raster master (`plans/APPS.md` §14) — and the format is decided from the
   bytes, never from the file name. This tier is asked for by naming the thing
   in the request (see below); everything without an icon of its own starts at
   tier 1.
1. **Raster artwork (next).** A pre-rasterised master shipped by the OS
   at `/System/Graphics/Icons/<asset-id>.png` (`icon_artwork_path(kind)`).
   Raster masters are a canonical icon source: they carry richer detail than a
   monochrome silhouette can.
2. **Vector SVG (next).** The scalable `<asset-id>.svg` source under the same
   directory (`icon_vector_path(kind)`), decoded into a `VectorIcon` through
   the SVG-first loader. A kind ships **one** class master, in whichever of
   the two formats suits its artwork — the folders are vector, the
   illustrative file-class and disk pictures are raster. Shipping one id in
   both formats is a packaging defect the image build refuses, because the
   raster tier would always win and the vector could never be selected.
3. **Built-in glyph (always last).** `builtin_icon(kind, colour)` — the
   monochrome vector silhouette compiled into the crate. This tier can never
   be absent, so the desktop always shows a meaningful icon even with no
   on-disk assets at all (a headless or freshly-installed system), which is
   why it is the **required** fail-closed fallback for every kind
   (`AGENTS.md` §2.9). It is retained like the asset tiers: `glyph_mask(kind,
   side)` rasterises the silhouette in opaque white so the mask's alpha *is*
   its coverage, and the drawing control supplies the colour
   (`Surface::blit_tinted`). Coverage does not depend on the tint, so one
   retained mask serves every colour and control state — and resolving a
   multi-layer glyph is expensive enough (its layers are painted enlarged and
   averaged back down to remove their seams) that paying it per icon per frame
   is what a caching tier exists to prevent.

A fine-grained file-class kind (`TextHtml`, `TextRust`, `ImagePng`,
`ImageSvg`, `DiskHard`, `DiskUsb`, …) names its own distinct raster/vector
asset id but deliberately shares its broad family's built-in glyph — text
kinds fall back to the text glyph, image kinds to the image glyph, every disk
medium to the one disk glyph. So a system that ships the artwork shows the
precise icon, and one that does not still shows a meaningful family glyph
rather than a bare placeholder.

## The `artwork` layer

The `artwork` module turns those on-disk tiers into drawable pixels for the
two processes that need it — the desktop session and the file manager —
through injected seams, so the crate stays `no_std` and the untrusted decode
never runs in this library or in the renderer that consumes it
(`AGENTS.md` §19.5):

- `ArtworkCache::artwork` is **total**: it answers every request, with shipped
  artwork or with the glyph mask that always resolves, tagged as an
  `IconPicture` so the drawing control knows whether to composite the pixels as
  they are or to tint them. A draw site therefore never rasterises vector art
  itself; the one exception is a caller holding `NoArtwork` (a headless build,
  a test), which draws through the *same* mask-and-tint arithmetic so a cached
  icon and an uncached one are the same pixels. A caller that *stores* a
  picture rather than drawing it now — a taskbar slot keeping its
  application's icon — takes `IconPicture::artwork()` and stores nothing for a
  mask, because a mask is not finished pixels.
- `ArtworkReader` reads an asset's bytes (a capability-gated filesystem read
  in production); a missing or refused asset is `None`, never fatal.
- `ArtworkRasteriser` turns encoded bytes into `side`×`side` straight-alpha
  RGBA8 (the parser sandbox in production).
- `MAX_ARTWORK_BYTES` (256 KiB) is the single fixed validation bound on an
  artwork file — a defence against hostile input, not a growable capacity
  (`AGENTS.md` §24.4). The sandboxed rasteriser (`lib/sandbox`) refuses
  over-long input against this same one definition, so the bound cannot
  diverge between the two crates (`AGENTS.md` §2.2).
- `MAX_ARTWORK_SIDE` (2048) is the source side an icon is ever decoded at,
  and `MIN_ARTWORK_SIDE` (256) the side a *shipped raster* master is authored
  at so a slot only ever downscales it — a vector master has no pixel side,
  being rasterised at the side it is drawn. Both are one definition: the
  sandboxed rasteriser bounds its decode by the first, and the image build
  refuses a first-party raster master that fails either, an icon of either
  format that will not decode, and one that decodes but draws nothing.
- `artwork_kind_for_file(name)` accepts exactly `<asset-id>.png` or
  `<asset-id>.svg` for a known kind — both, because both class tiers read the
  one directory — and refuses anything else (an unknown id, a format no tier
  reads, an empty name, or a directory-bearing path-traversal attempt), so the
  image build can reject an asset the desktop could never resolve.

## Asking for a picture

A draw site states what it wants as one `IconRequest`, and the artwork layer
owns the order the tiers are tried in — so a taskbar button, a launcher row,
a desktop icon and a file-manager tile cannot resolve the same thing three
different ways:

- `IconRequest::kind(kind)` — the class picture alone.
- `IconRequest::asset(kind, path)` — an icon whose path the caller already
  knows (the program-library catalog stores one per listed application),
  falling back to the class picture.
- `IconRequest::bundle(kind, dir)` — the application bundle at `dir`, whose
  own manifest names its icon. The artwork layer reads that manifest itself,
  so a draw site holding only a directory entry needs no manifest knowledge.

A bundle's manifest is authored by whoever built the bundle, so it is treated
as untrusted input at that boundary: it is read under the ABI's own wire
bound, decoded by the shared fail-closed header decoder, and the asset name is
accepted only as a plain file name (`tairix_path::validate_file_name`) and
then resolved *inside* the directory it came from. A bundle therefore cannot
aim the desktop at a file outside its own `Resources/`, and one that tries
simply draws its class picture.

`ArtworkCache` retains each decode — success **or** refusal — keyed by what
was resolved (an asset path, or a bundle directory) and the pixel side, over
the one shared reclaimable-memory cache
(`lib/reclaim`, `plans/SMARTRAM.md`), so a crowded or crafted bundle store can
never grow a session without bound and a bad asset is not re-read every frame.
A cache miss reads the bytes, refuses **before** rasterising when they exceed
`MAX_ARTWORK_BYTES`, rasterises, verifies the reply is exactly `side`×`side`
straight-alpha RGBA8 with checked arithmetic (never a panic), and builds the
`Surface`; a zero side, an unreadable path, an over-long asset, a refused
decode, or a wrong-length reply all yield a cached `None`. Lookups return a
**borrow** into the cache, never a clone, so a grid drawing many icons a frame
reads each surface in place. `artwork_cache(label, seat, fb_bytes, pressure,
sink)` builds the cache identically for both consumers, and
`IconArtworkSource` binds a cache to its resolver so a renderer receives a
plain `IconArtwork` lookup that knows nothing about I/O. `NoArtwork` is the
all-glyph lookup a headless build or a test uses — it never resolves any
artwork, so every draw site falls back to its built-in glyph.

A bundle is keyed by its *directory*, not by the asset its manifest names, so
the manifest read is paid once per bundle and a bundle that declares no icon
(or names one that will not decode) remembers that refusal too rather than
re-reading it every frame.

## What pressure may take, and what it may not

The cache is built through `tairix_reclaim::working_set_ui_cache`, so a quarter
of its budget is declared the owner's live **working set** — what one frame
draws — and its first `UI_CACHE_RESERVE_BYTES` are declared **irreducible**:
mild and moderate memory pressure take the scroll-back and off-screen
speculation above the working set but not the working set itself, and severe
and critical take everything above the reserve. Re-deriving an entry here is not local work the
session can repeat at will — it is a capability-gated read plus a
parser-sandbox round trip — so giving it back at the first tightening frees a
figure the machine barely notices and immediately costs both again, per icon,
on the next repaint (`plans/SMARTRAM.md` section 6.4).

The ceiling is **one screenful**, not a fraction of one. Icons are drawn on the
output, so no more of them can be visible at once than fill it — and a grid of
them is nowhere near a fraction of it: a 480×480 file-manager window draws some
117 KiB of icon where a sixteenth of its frame is 57 KiB. A cache that cannot
hold what one frame draws evicts an entry the very next paint asks for again,
which is either a wrong picture on screen or a read and a round trip per icon
per frame. On an output whose frame is a mebibyte or less the whole budget sits
inside the reserve and no band takes a decoded icon at all; above that, severe
and critical take what is held above the reserve.

Where retention genuinely is refused — an entry larger than what the budget can
hold, or an output whose budget is smaller than one decode — the decode cannot
be kept and the draw site falls back to its built-in glyph, which is the tier
that exists for it. What must not follow is asking again on every frame, so the
cache reports the refusal to the resolver that produced it
(`ArtworkResolver::declined`) and a deferring resolver holds that key back until
the band moves.

## Who does the decode, and when

A read plus a sandbox round trip is far too much to spend inside a paint that
has to stay responsive, so `ArtworkResolver` is the seam between *deciding what
a draw needs* and *producing it*:

- `InlineArtwork::new(reader, rasteriser)` reads and decodes on the calling
  thread. It is the whole of what a program with nothing to defer to needs, and
  the path a process the kernel granted no thread falls back to.
- `ArtworkDesk` is the deferring resolver: it answers `Resolved::Pending` for a
  key it has just recorded, and `Resolved::Done` once a producer has delivered
  it. Nothing is retained meanwhile, the draw falls to the tier below — for the
  last tier, the built-in glyph — and the same lookup serves the artwork once
  the pixels land.
- Both produce the decode through `render_artwork`, so where it ran cannot
  change what it produced.

The desk holds no lock, thread, or syscall, so its whole policy is host-tested.
Two embedders drive it over the same rules, and both do it the same way: each
parks a worker thread on the desk behind the runtime's futex mutex and is woken
back through the wait-set it already parks in — the desktop session on its own
icon-decoder thread, the file manager on the one reader thread its directory
listings, folder cues, and program-store walks also share
(`plans/FIX-DESKTOP.md` DESK-8/DESK-11/DESK-12, `plans/NEW-FILEMANAGER.md`).
The lock carries only the desk: the decode cache stays on the paint side,
because a picture is handed out as a borrow into it and a borrow cannot outlive
a guard. So the read and the sandbox round trip run with nothing held.

### What the desk remembers

An answer handed over is *forgotten*: the cache that collected it owns it, and
if the cache later drops it the next paint's miss is a genuine one that must be
produced again. Remembering "already answered" instead left an evicted icon
drawing its glyph until unrelated input arrived, because nothing else would ever
ask for it — the file-manager defect that made a scrolled grid of
`/System/Commands` fall back to the generic bundle picture.

The decode cache is budgeted, though, so it can be asked to hold more than it
will, and a decode it *refuses* must not be offered again: the repaint its
landing drove would ask, the answer would be refused again, and every icon on
screen would be read and decoded every frame, precisely when the machine is
short of the memory that would have held them. The cache says so
(`ArtworkResolver::declined`) and the key is held as declined until
`ArtworkDesk::retry_declined` re-offers it on the pressure band's own wake.

A pending tier **stops** the walk rather than falling through, because whether
a later tier is reached at all depends on what this one turns out to be. A
deferred request therefore costs exactly the reads a synchronous walk would,
spread over as many answers as it has tiers to try.

`ArtworkCache::prefetch` is what keeps that spreading invisible. A caller that
knows what it is *about* to draw asks for it there, and the cache asks the
resolver for the one tier it does not already hold — so the decode is finished
before the frame that needs it. Without it, a surface showing a screenful of
icons paints every one as a built-in glyph and replaces them a round trip per
icon after the user is already looking at it. `InlineArtwork` prefetches
nothing, which is right: it has nothing to prepare, and "preparing" would be
the very stall the caller is avoiding.

`artwork` hands out a borrow, which is right for a surface that paints from
the cache and asks again next frame whatever the answer was. A caller that
*stores* the picture instead — a window's title-bar identity, resolved once
when the window opens — asks `owned_artwork` and receives an `ArtworkOutcome`:
`Ready(surface)`, `Refused` (asking again would only repeat it), or `Pending`
(ask again when the producer says the decode has landed). `owned_artwork` also
hands back a decode the cache was too tight to retain, rather than throwing
those pixels away.

## Stability

Tier: `experimental`.
