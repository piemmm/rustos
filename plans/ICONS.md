# ICONS.md — the desktop's icon artwork, end to end

Binding under `AGENTS.md`. This plan owns one cross-cutting concern: **how a
thing on screen acquires the picture that represents it** — the shipped
artwork, the vocabulary that names it, the content-type registry that chooses
it, the build step that plants it, the runtime path that decodes it safely,
and every surface that draws it.

It exists because the answer spans crates that no single other plan owns: the
taskbar (`plans/NEW-TASKBAR.md`), the file manager
(`plans/NEW-FILEMANAGER.md`), the desktop session (`plans/DISPLAY.md`), and
the shared controls (`plans/GUI-CONTROLS-DESIGN.md`). Those plans own their
*surfaces*; this one owns the artwork pipeline they all draw through, so the
rule is stated once and cannot drift.

Read first: `AGENTS.md` §10 (the three-tier asset rule), §16.2
(`/System/Graphics`), §19.5 (parser sandboxing), §24.4 (fixed validation
bounds), and `plans/GUI-CONTROLS-DESIGN.md`.

## Status

`done` — I1–I7 complete. Every stage's section below records what it now
guarantees.

## 0. The binding decisions

- **Tiers, always total.** A *thing* resolves to its own icon first — an
  application bundle's `Resources/` master, named by its signed manifest —
  and then to its *class*: raster artwork
  (`/System/Graphics/Icons/<asset-id>.png`), else the on-disk vector asset
  (`<asset-id>.svg`), else the first-party built-in glyph. The glyph tier is
  mandatory: no icon may exist as an asset alone, so a missing, oversize,
  corrupt, or refused file degrades to a meaningful picture and can never
  blank a surface. The charter carries this rule; this plan implements it.
- **The order is decided in exactly one place.** `ArtworkCache::artwork`
  takes one `IconRequest` (a kind alone, a kind plus an already-resolved
  asset path, or a kind plus a bundle directory) and owns the tier order, so
  a taskbar button, a launcher row, a desktop icon and a file-manager tile
  cannot resolve the same thing three different ways.
- **Every tier is retained, the glyph included, so no frame resolves coverage
  twice.** A glyph is cached as an *untinted coverage mask* keyed
  `(kind, side)` — the shape does not depend on the colour it is drawn in, so
  one mask serves every tint and control state, and the drawing control
  composites it with `Surface::blit_tinted`. Resolving a multi-layer glyph
  means painting its layers enlarged and averaging them back down to remove
  the seams between them, which measured 20–38 µs per icon: paid per icon per
  frame that is what made a long listing crawl, and paid once it is free
  thereafter. The mask is resolved *in this process*, not through the
  `ArtworkResolver`, because first-party vector art compiled into the binary
  needs neither a read nor a sandbox — and that also keeps it synchronous, so a
  glyph is on screen in the first frame that asks for it.
- **The draw path blits and never rasterises.** `IconArtwork::artwork` is
  total: it always answers, with shipped artwork or a glyph mask, tagged as
  `IconPicture` so the control knows which to tint. The one uncached path — a
  caller holding `NoArtwork`, a headless build or a test — draws through the
  same mask-and-tint arithmetic, so a cached icon and an uncached one are the
  same pixels.
- **A bundle's own icon is its identity, not a launcher detail.** The
  manifest's `library-icon` is independent of its `library` listing: every
  command app declares an icon and none of them is listed in the program
  library. The two were coupled once; the coupling is gone. Declaring one is
  **mandatory** for every launchable app, SVG by preference — that rule is
  `plans/APPS.md` §14 and this plan does not restate it.
- **One vocabulary.** `IconKind` (`lib/icon`) is the single closed icon
  vocabulary. A file-class kind and a chrome glyph are the same kind of
  thing to every draw site; the difference is only which tier resolves.
- **Two independent facts, never conflated.** `MediaType` (`lib/browse`)
  names *what a file is*; `MediaType::icon()` names *which picture
  represents it*. The first is one-to-one and must never shrink (it is the
  application-association vocabulary); the second is deliberately
  many-to-one. A subclass chain (`MediaType::parent`) keeps a generic
  declaration matching a specific file, exactly as shared-mime-info models
  it.
- **Every decode is sandboxed and bounded.** A system asset is treated
  exactly like a third-party bundle's own icon: read under a fixed byte
  bound (`MAX_ARTWORK_BYTES`), refused *before* decode when over it, decoded
  in a minimum-capability sandbox process, and accepted only when the reply
  is exactly the pixels requested. The desktop trusts validated pixels,
  never a file.
- **Decode once, per (thing, pixel side).** One reclaim-governed cache
  (`lib/icon::ArtworkCache`), shared by every consumer process, keyed by what
  was resolved (an asset path, or a bundle directory) and the pixel side,
  returning borrows so a hundred-tile grid never copies a hundred images per
  frame. Negative results are cached too, so a bad asset — or a bundle with
  no icon at all — is not re-read every frame.
- **Decoded once means once, not once per band.** The cache is the *working
  set* of the surfaces drawing from it, not speculation around one: re-deriving
  an entry costs a capability-gated read plus a sandbox round trip, and the
  whole budget is a small fraction of one frame of the output the icons are
  drawn on. So mild and moderate memory pressure leave it alone
  (`tairix_reclaim::working_set_ui_cache`, `plans/SMARTRAM.md` section 6.4);
  severe and critical still take it, and the glyph tier is then the honest
  answer. A refusal the cache cannot avoid is reported to the resolver
  (`ArtworkResolver::declined`) and not re-offered until the band moves, so
  giving the pixels up is never a storm of reads that cannot be kept.
- **Rendered exactly, at the size it is drawn.** A vector asset is rasterised
  straight onto the requested `side`×`side` surface — never at a nominal size
  and rescaled — and every pixel takes the *exact* fraction of its own area
  the artwork covers (`lib/raster`'s scan converter). The layer stack is
  composed through `Surface::layered`, which paints a multi-layer icon larger
  and averages it down, because two anti-aliased edges that abut otherwise
  blend as if they overlapped and leave a shape's outline short of opaque.
  Both matter most exactly where icons live: a 256-unit drawing in twenty-odd
  pixels, with strokes a fraction of a pixel wide. Sampling a handful of
  points per pixel instead cost up to a third of full alpha on an edge and
  rendered a symmetric shape asymmetrically.
- **Artwork is data, discovered at build time.** The shipped set is whatever
  is in `lib/icon/assets/` and in each bundle's own `Resources/`; adding an
  icon is dropping a file there. No hand-maintained list exists in the
  kernel, the image builder, or a test fixture, and the build refuses a file
  the desktop could not resolve.

## 1. The shipped set

Two families of master, one contract.

**The class artwork** — `lib/icon/assets/<asset-id>.png` or
`<asset-id>.svg`, planted at `/System/Graphics/Icons/`. The file name *is*
the asset id, and the id *is* an `IconKind::asset_id()`, so a typo cannot
ship: `tools/syshelp`'s build script fails the build on an unrecognised name,
an oversize file, or a duplicate id. A kind ships **one** master, in whichever
format suits its artwork — the folders are vector (`folder.svg`,
`folder-filled.svg`), the
illustrative file-class and disk pictures are raster. Two files claiming one
id is a duplicate the build refuses: the raster tier would always win, so the
vector could never be selected.

**Each bundle's own icon** — `<crate>/Resources/<name>.svg` (the preferred
form) or `<name>.png`, declared as `library-icon` in that bundle's
`AppInfo.toml` and planted inside the bundle. Every command app and every GUI
app under the three app roots the resource walk covers (`userland/apps`,
`userland/shell`, `userland/gui`) carries one, so browsing the system program
stores shows fifty distinct pictures rather than fifty copies of the generic
bundle icon.
Services outside those roots keep the service-bundle class artwork, which is
the honest picture for them.

A **raster** master is square, straight-alpha
and at least `MIN_ARTWORK_SIDE` (256×256), so a slot only ever downscales it.
A **vector** master has no pixel side at all: the decoder requires its design
box to be square and the desktop rasterises it at the side it is about to
draw. Either form stays within `MAX_ARTWORK_BYTES` (256 KiB), and both are
*authored artwork*: adding or replacing one is dropping the file on disk, never
editing a list. The image build proves every one of them is artwork the desktop
will really draw (below), so "the icon is broken" is a build failure rather
than a silent glyph on someone's desktop.

Masters that no live consumer can select are **not** shipped. They live in
`artwork/` (reference art, not shipped) and return to `lib/icon/assets/` in
the change that gives them a consumer — `artwork/icons/disk-floppy.png` is
the current example: nothing in the block stack can report a floppy medium,
so shipping it would be a picture nothing could ever choose.

## 2. I1 — the vocabulary and the artwork layer (`lib/icon`) — **done**

- `IconKind` covers chrome, application/service bundles, the file classes,
  and the drive media. A fine-grained kind (`text-x-rust`, `image-png`)
  deliberately shares its family's built-in glyph, so the glyph tier stays
  meaningful without hand-authoring an outline per file type.
- `artwork.rs` owns the whole runtime path: `GRAPHICS_DIR` / `ICONS_DIR`,
  `icon_artwork_path` / `icon_vector_path`, `artwork_kind_for_file` (the
  build's fail-closed name check), the `MAX_ARTWORK_BYTES` bound (one
  definition, `lib/sandbox` consumes it), the `ArtworkReader` /
  `ArtworkRasteriser` seams, the `IconArtwork` draw-site lookup with its
  all-glyph `NoArtwork` implementation, `ArtworkCache`, and
  `IconArtworkSource` which binds a cache to its seams.
- `disk_icon(Option<BlkDeviceClass>)` maps a mounted volume's real storage
  medium to its icon, with paravirtual and unknown both resolving to the
  generic drive glyph — the honest answer, not a guess.

## 3. I2 — the content-type registry (`lib/browse`) — **done**

One closed `MediaType` registry replaces the two overlapping
extension-keyed tables that used to exist (a four-class icon classifier and
a separate association table). It provides the media-type spelling, the
reverse lookup, the icon, the subclass parent, and the entry classifier
(which distinguishes an application bundle from a service bundle by the
store it was listed from). Association matching walks the subclass chain and
ranks a specific declaration ahead of a generic one.

## 4. I3 — build-time discovery and planting — **done**

`tools/syshelp` walks `lib/icon/assets/` and emits `GRAPHICS_FILES`
alongside the existing per-bundle `Help/` and `Resources/` tables. One
shared planting walk (`plant_system_payload`) now serves both the image
builder and the QEMU encrypted-root fixture, which previously carried
hand-mirrored copies of the same loops. A read-back test mounts the built
`/System` read-only and proves the bytes arrive intact.

The same stage closed a live defect: a bundle could declare a
`library-icon` larger than the desktop will ever decode, and would then
silently render as a glyph forever with nothing telling the author. The
image build now refuses it, naming the bundle, the file, its size, and the
bound — and, since I7, refuses any icon that is not artwork the desktop could
draw at all: the format is decided from the bytes as the runtime decides it, a
raster master must be square and at least the master side, and either form
must actually draw something.

## 5. I4 — the taskbar and the program library — **done**

The bar's two permanent launchers and its trailing Switchboard capsule draw
their shipped artwork; a pin and a running-task item use the application's own
icon, then its kind's artwork, then the glyph — one rule, expressed once.
The program-library popup shows
each application's own icon, resolved only for the rows actually on screen
and re-resolved on scroll. The session owns the one `ArtworkCache` and its
seams; the taskbar renders and never reads a file.

A latent bring-up defect was fixed here: the shared cache admits nothing
while the reported memory-pressure band is the fail-closed unknown, and the
session refreshed its band only *after* building its caches — so artwork and
glyph caches would have stayed cold through bring-up.

## 6. I5 — the file manager — **done**

The grid (icon) view draws file-class artwork through the same shared cache,
decoding in a sandbox the app hosts itself under the spawn authority it
already held — no new capability, and no in-process decode. Decoding is
strictly demand-driven: only visible tiles, only newly visible kinds on
scroll, released on teardown and trimmed when pressure deepens (driven by
the same kernel pressure wake the session uses, never a timer).

Every other icon the window draws resolves through that same cache too: the
**list** view's row icons, the toolbar's tools, and the places rail's rows. The
list asks by *class* rather than naming a bundle — a row-height picture cannot
show one application apart from another, and reading each bundle's manifest to
find out would be per-bundle I/O the reader never sees — while the grid, whose
tiles are large enough to tell two applications apart, names the bundle. Only
the rows and tools on screen are asked for.

## 7. I6 — storage media are real, not guessed — **done**

A volume's medium is threaded from the block device's own declaration
through the kernel mount table onto the ungated `MOUNT_LIST` record, so the
places sidebar's drive icons reflect what is actually attached. An
unrecognised class word stays an explicit *unknown* the whole way rather
than being rewritten into a fabricated "paravirtual" identity; the
cautious I/O budget for an unknown device is unchanged.

One residual gap this stage surfaced rather than buried is tracked in
`plans/OPEN-DEFECTS.md`: a composition can still fold an unreadable member
class into a concrete class, so a composed volume may publish a medium
nobody declared. Its only user-visible consumer today is the drive icon,
where the generic glyph is already the right picture.

## 8. I7 — every app carries its own icon — **done**

The last stage closed the gap the tiers implied but nothing supplied: the
system shipped one picture for *all* applications. Now:

- Every command and GUI bundle ships its own icon in its `Resources/` and
  declares it in its manifest — mandatory for any new app (`plans/APPS.md`
  §14). The shipped set is one visual family of 256×256 raster masters — a
  bevelled plate, a chrome motif, an orange accent — with the plate tint
  grouping a bundle by what it does (files and shell, text utilities,
  network, process and monitoring, storage and devices, users and security),
  so a strip of them reads as one system rather than fifty unrelated images.
- `AppInfoHeader` no longer refuses an icon on an unlisted bundle. That rule
  made sense when the program library was the only consumer; the file manager
  and the desktop are consumers too, and neither has anything to do with the
  launcher's folders.
- The file manager's grid and the desktop's icons name the bundle in their
  request, so a `.app` tile draws the application's own picture. Resolution
  stays demand-driven (only the tiles on screen), and a bundle is keyed in
  the cache by its *directory*, so its manifest is read once and a bundle
  with no icon of its own remembers that too.
- The image build proves every icon it is about to plant — class artwork and
  bundle icon alike — is artwork the desktop will draw, deciding the format
  from the bytes exactly as the runtime does: a PNG through `lib/image` under
  the same limits the sandboxed rasteriser applies, else the supported SVG
  subset through `lib/svg`. A raster master must be square and at least
  `MIN_ARTWORK_SIDE`, and either form must draw something — an empty document
  or a wholly transparent master would ship as an invisible icon.
- A bundle's manifest is untrusted at that boundary: the icon name is
  accepted only as a plain file name and resolved *inside* the bundle's own
  directory, so a hostile `library-icon` cannot aim the desktop at a file
  elsewhere. It draws its class picture instead.

## 9. The pressure vertical asserts over the bands it is sound for — **done**

`tairix-test-desktop-pressure-qemu-aarch64` required the icon bar's untouched
slot to be **byte-identical** between its two frames, reading any drift as "the
desktop stopped drawing its decoded artwork". Its pressure witness, though, is
only that the published band *left normal*, which admits severe and critical —
and section 0's policy is that those bands **do** take the cache, with the
glyph tier as the honest answer. So the assertion was a true statement about
mild and moderate alone while the run it judged could be in any band, and it
failed whenever the machine went deep: measured 22.6% slot drift under eight
concurrent QEMU guests against 0% when the vertical ran alone.

The band is not steerable (no test hook in the pressure path) and how deep a
run goes turns on how much retained content it accumulated, so the guest now
reports what it observed: one serial record
(`PRESSURE_DEEPENED_MARKER`) the first time the published band passes moderate,
written straight to the serial sink because it is emitted inside an audit
callback and a log event there would re-enter the sink that called it. The
host's artwork assertion reads the persisted transcript and applies the
zero-drift bound only to a run that stayed within the promised bands; an
unreadable transcript fails closed rather than counting as shallow. The gate
that matters — moderate never takes the artwork — keeps its full strength, and
a deep run no longer fails for behaving as this plan specifies.

## 10. Open — two surfaces still rasterise their glyphs per frame

The glyph tier is cached and the draw path blits, but a control only benefits
where its owner holds a cache to resolve through. Two surfaces hold none, so
their `paint_icon_slot` calls still take the inline path and re-resolve
coverage every frame:

- **The Switchboard** (`userland/gui/switchboard`) owns no `ArtworkCache` at
  all. Its metric tiles and task rows each draw a glyph per frame, and its
  resource tiles use the multi-layer kinds that cost most (20–38 µs per icon
  at a row-height side, measured). Closing it means giving the crate a cache
  built through the one shared `artwork_cache` constructor with its own seat,
  frame size, pressure gauge and audit sink — as `files.app` and the session
  do — and threading the lookup through `SectionCtx`, which every section
  render already receives.
- **The widgets gallery** (`userland/apps/widgets`) is a demo of the control
  family; it draws each control once per frame with `NoArtwork` deliberately,
  and is not a surface a user scrolls. It needs a cache only if the gallery is
  ever meant to demonstrate the cached path.

Three controls also draw their glyph outside the shared icon slot, so they
resolve coverage per frame even when their owner *does* hold a cache: a
`Button`'s leading icon (`ButtonContent::Icon`/`IconLabel`), a `MenuItem`'s
row icon, and the greeter's continue mark. All three now draw through the one
`glyph_mask` + `Surface::blit_tinted` arithmetic every other icon uses — so no
two surfaces disagree about a glyph's pixels — but none of them takes a
picture, so none is cached. Giving them one means an artwork parameter on
`Button::render`, `Menu::render` and the greeter's row painter, which their
callers would have to thread.

None of this is a correctness defect — the pixels are identical either way,
which is the point of routing every path through the same arithmetic — and
none is on the file manager's hot path, which is what the caching tier was
added for. They are the remainder of "no surface rasterises on the draw
path".

## 11. Open — two SVG class-asset decode paths

The crate now decodes a vector class asset in two places. `lib/icon/src/load.rs`
builds a whole `IconSet` up front, one slot per `IconKind`, from an injected
`IconAssetSource`; `ArtworkCache`'s vector tier decodes one asset per (asset,
pixel side) on demand, bounded and reclaim-governed. The single-decode-path
intent above wants one of them to own the class assets: the preloaded set is
the right shape for the always-resident chrome glyphs, the bounded cache for
artwork drawn at a slot's pixel side. Deciding which owns what is a design
question this plan has not settled.

## 12. Open — an undrawable element is skipped, not refused

`lib/svg` now decodes the drawable part of SVG 1.1 in full (`plans/SVG.md`),
so an authored master ships as its designer drew it. What it still cannot
draw — text, embedded images, filters, masks, clipping paths, patterns — it
**skips**, rendering the rest of the document rather than refusing it.

That is deliberate: one unsupported decoration should not lose a whole asset,
and it is the behaviour every surface has today. But it cuts against failing
closed, because a master carrying a clipping path renders *unclipped* — a
wrong picture — instead of falling back to the tier below. The alternative is
to refuse the document when it carries a drawable element the decoder cannot
honour, so a wrong picture becomes a clean fallback. Which of the two the
desktop wants is a decision this plan has not taken; the build gate already
refuses a master that draws nothing, so only the *partially* drawable case is
at stake.

## 13. What this plan deliberately does not cover

- **Cursors and window chrome stay vector.** They are tintable silhouettes
  resolved from the theme; a raster master would be the wrong source format
  for them and the charter says so.
- **Animated formats (GIF playback), JPEG, and ICO decoding.** A shipped
  raster master is PNG; the file-class artwork for a JPEG or GIF *file* is a
  static icon, which needs no decoder for that format. A decoder is added
  when something actually needs to display such a file's contents, in the
  plan that owns that viewer — never speculatively here.
