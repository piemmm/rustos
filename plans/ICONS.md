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

`done` — I1–I6 complete. Every stage's section below records what it now
guarantees.

## 0. The binding decisions

- **Three tiers, always total.** An icon resolves as raster artwork
  (`/System/Graphics/Icons/<asset-id>.png`), else the on-disk vector asset
  (`<asset-id>.svg`), else the first-party built-in glyph. The glyph tier is
  mandatory: no icon may exist as an asset alone, so a missing, oversize,
  corrupt, or refused file degrades to a meaningful picture and can never
  blank a surface. The charter carries this rule; this plan implements it.
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
- **Decode once, per (asset, pixel side).** One reclaim-governed cache
  (`lib/icon::ArtworkCache`), shared by every consumer process, keyed by
  asset path and pixel side, returning borrows so a hundred-tile grid never
  copies a hundred images per frame. Negative results are cached too, so a
  bad asset is not re-read every frame.
- **Artwork is data, discovered at build time.** The shipped set is whatever
  is in `lib/icon/assets/`; adding an icon is dropping a file there. No
  hand-maintained list exists in the kernel, the image builder, or a test
  fixture, and the build refuses a file the desktop could not resolve.

## 1. The shipped set

`lib/icon/assets/<asset-id>.png` — square, straight-alpha, 256×256 raster
masters, each within `MAX_ARTWORK_BYTES` (256 KiB). The file name *is* the
asset id, and the id *is* an `IconKind::asset_id()`, so a typo cannot ship:
`tools/syshelp`'s build script fails the build on an unrecognised name, an
oversize file, or a duplicate id.

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
bound.

## 5. I4 — the taskbar and the program library — **done**

The bar's two permanent launchers draw their shipped artwork; a pin and a
running-task item use the application's own icon, then its kind's artwork,
then the glyph — one rule, expressed once. The program-library popup shows
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

## 8. What this plan deliberately does not cover

- **Cursors and window chrome stay vector.** They are tintable silhouettes
  resolved from the theme; a raster master would be the wrong source format
  for them and the charter says so.
- **Animated formats (GIF playback), JPEG, and ICO decoding.** The shipped
  masters are PNG; the file-class artwork for a JPEG or GIF *file* is a
  static icon, which needs no decoder for that format. A decoder is added
  when something actually needs to display such a file's contents, in the
  plan that owns that viewer — never speculatively here.
