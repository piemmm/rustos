# FONT-SERVICE.md — One sandboxed OS font service; no font data in any app

Binding under `AGENTS.md`. This plan removes the desktop's slow app launch
at its root and, in doing so, makes text rendering a single, sandboxed,
OS-provided resource — strictly more secure and more memory-efficient than
the per-process font libraries Linux/Windows ship.

The rule this plan enforces is already in the charter: font rendering is a
curated OS shared library, not per-app static data (§16.4); system fonts
live under `/System/Fonts` (§16.2); a parser of untrusted input — font
rendering is named explicitly — runs in a minimum-capability sandbox
process (§19.5); and shared data is defined once, never copied into every
consumer (§2.2, §2.3). The current font stack violates all four.

---

## 1. The defect (measured)

`lib/font` compiles the entire font payload into **every** consumer via
`include_bytes!`:

- `lib/font/src/atlas.rs`: `pub static COVERAGE = include_bytes!("atlas_coverage.bin")`
  — a **3.6 MB** full-Unicode native-size glyph-coverage atlas.
- `lib/font/src/cache.rs`: four embedded TrueType faces — Inconsolata-EX
  (416 KB), MPLUS1Code (1.7 MB), D2Coding (4.0 MB), Noto Sans Hebrew
  (20 KB) ≈ **6.1 MB** — parsed by the in-process `lib/fontface` engine to
  rasterise text at non-native (scaled desktop) sizes.

Every GUI consumer — `userland/apps/{terminal,files,viewer,widgets}`,
`userland/gui/{wm,taskbar,session}` — therefore carries its own private
~10 MB read-only copy. `readelf -lW` on the built `Run` images confirms it:
`terminal` and `files` each have a ~10 MB `R` LOAD segment (0x9b45d8 /
0x9c89b0); a non-GUI app (`cat`) has ~59 KB.

Consequences, all of which this plan removes:

- **Slow launch (the reported bug).** The launch path reads the whole `Run`
  rxe off disk, SHA-256-hashes the whole bundle, and eagerly copies every
  loadable page into private frames. ~10 MB of read + hash + copy per launch
  is slow on metal and glacial under QEMU TCG — for *every* GUI app.
- **Duplication / bloat** (§2.2, §2.3): N copies of identical immutable font
  data on disk and in RAM.
- **Unsandboxed untrusted parsing** (§19.5): the TrueType parser
  (`lib/fontface`) links into every GUI process. A malformed face is a code
  path in the terminal, the file manager, the compositor — not in a
  minimum-capability sandbox.

## 1.1 Constraint — the kernel boot console needs an atlas

`lib/fbcon` (the framebuffer text console) links `lib/font` with
`default-features = false` (no `alloc`) and draws text from the atlas
**before any service exists** — it is boot floor (§18.6), and it is also the
kernel/headless text console. It runs in the kernel and has no way to call a
user-space service for a glyph, so **its repertoire is whatever is compiled
in**: the atlas carries every face the console family names (§2.4), the
Japanese, Korean and Hebrew companions included.

**This is not a size trade to make.** A script left out of the atlas is one
the console can never draw — a `man` page in it, a login prompt, a panic — and
no runtime service can rescue that. At the 8×16 cell the whole family costs
about 1.6 MB compiled in, less than half the 3.6 MB the primary face alone
cost at the old 15×28 cell, so completeness is also the cheaper outcome than
the one it replaced.

**No precomputed full-atlas artifact exists.** `/System/Fonts` holds only the
four committed TrueType faces; `fontd` loads them and rasterises every size
(native and scaled) on demand through the one `lib/fontface` engine, cached.
A second, precomputed full-Unicode atlas beside the faces it is derived from
would be the duplication §2.2 forbids, so there is exactly one runtime
rasterisation source (the faces) and exactly one compiled-in atlas (the
console subset).

---

## 2. Design — a single, sandboxed font service

### 2.1 `fontd` — the font service (`userland/system/fontd`)

A long-running user-space system service shipped as a signed
`/System/Services/fontd.app` bundle (§16.2, §16.5), discovered and spawned
through the normal signature + capability + interface-hash gate (§18.3) —
never baked into the kernel.

- **Owns the font payload**: it scans `/System/Fonts/` at start and reads
  each face's bytes on first use. No font bytes live in any other process.
- **Rasterises in a §19.5 sandbox.** The TrueType parse + outline
  rasterisation (`lib/fontface`) runs only in this service, a
  minimum-capability address space: it requests only
  `CAP_IPC_BIND_PRIVILEGED` (to bind the reserved `FONT_ENDPOINT`),
  `CAP_FS_ACCESS` (the one-shot startup read of the faces through the secured
  VFS — `fs_open` is capability-gated regardless of the file's world-readable
  mode), and `CAP_LOG_EMIT` (audit) — no spawn and no network authority, and
  `/System` is mounted read-only so the fs reach can never write. The faces
  are trusted committed OS assets, but isolating the parser in its own address
  space means even a malformed face faults only this sandbox, never a
  compositor or terminal.
- **Serves glyph coverage** over the reserved `FONT_ENDPOINT` (§2.2): a
  [`FontRequest::Glyph { family, scalar, pixel_height, weight }`] reply is the
  8-bit coverage bitmap the client blits, with the pen advance and left side
  bearing to place it by. The service resolves the scalar within the named
  family — its own faces in order, then its fallback family's faces, else
  U+FFFD — rasterises once at the requested size (4-bit engine coverage scaled
  ×17 to the protocol's 8-bit samples), and memoises in the byte-budgeted
  `(family, resolved family, face, glyph, height, cells, weight)` cache of
  §3.1. The cell count is keyed because how many cells a scalar spans is a
  property of the scalar, not of the glyph a face maps it to. It also answers
  [`FontRequest::Metrics`] with the family's line metrics, and
  [`FontRequest::Families`] with the installed selectable families so a
  settings surface offers exactly what the store holds.

### 2.2 `FONT_ENDPOINT` — the IPC protocol (`lib/abi/src/font_ipc.rs`)

A new reserved-endpoint protocol modelled on `window_ipc.rs` /
`display_ipc.rs`: a fixed-size, bounds-checked request framing and a
length-prefixed coverage reply, both `#[repr(C)]`, versioned and hashed
under the same ABI discipline as the syscall table (§9) and frozen on the
first release (mutable now — `abi-v1` is not frozen). The generated C view
follows (`cargo xtask c-header`).

- Reserved id `FONT_ENDPOINT` (ASCII-hex-spelled, per the existing
  convention; register with `crate::ipc::is_reserved_endpoint`).
- Requests: `Glyph { family, scalar, pixel_height, weight }`, `Metrics {
  family, pixel_height, weight }` returning the line metrics the client lays
  text out with, and `Families` listing the installed selectable families.
  One fixed request length; every field an operation does not use must be
  zero, and anything else is refused, fail closed (§5.4).
- Replies: a glyph `{ width, height, advance, left, bytes[coverage] }`
  (length-bounded; `width == 0` is an ink-less glyph such as a space), the
  metrics `{ pixel_height, baseline, line_height, monospace_advance }`, and
  the family list. `monospace_advance == 0` *is* the statement that a family
  is proportional, so a caller cannot mistake one for the other.
- **No new capability** (§5.2): drawing text is not a security boundary, so
  the endpoint is callable by any process (the *action* still validates every
  field and fails closed). The service holds the only privileged thing — the
  `/System/Fonts` read authority — and only at startup.

### 2.3 `lib/font` becomes the thin client (+ protocol reuse)

`lib/font` is the drawing front end every surface uses:

- A `BitmapFont` is a `(family, pixel height, weight)` triple. It sends a
  `FONT_ENDPOINT` request and blits the returned coverage, caching replies
  locally per `(family, scalar, height, weight)` in the byte-budgeted cache
  of §3.1, so steady-state redraws issue no IPC. Line metrics are fetched
  once per `(family, height, weight)` and cached beside them; with no
  transport installed they fall back to the compiled-in console geometry, so
  a dead service degrades to unrendered text rather than broken layout.
- Layout is **per-glyph**: `text_width`, `truncate_to_width`, and
  `draw_text` accumulate each glyph's own advance and place it at its own
  bearing. A monospace family keeps a fast path over its single advance, so
  the terminal grid pays no per-glyph arithmetic, and its glyphs arrive
  already *in* the cell — one cell wide, two for a double-width scalar, with
  a zero bearing — so a grid blits them at the cell origin.
- No TrueType bytes and no outline rasteriser live in the crate: only the
  compiled-in console atlas (§2.4) the boot console draws from.
- Protocol request/reply encoders live in `lib/abi::font_ipc` and are shared
  by client and service (§2.2), never re-spelled.

### 2.4 Kernel console atlas subset (`lib/fbcon` / kernel path)

The framebuffer text console cannot call a service (boot floor), so it keeps a
**compiled-in console atlas** covering the whole console family: the primary
Inconsolata EX repertoire (ASCII, Latin-1, Latin Extended, Greek, Cyrillic,
box drawing, arrows, punctuation, currency, U+FFFD; single-cell) plus the
Japanese, Korean and Hebrew companions (full-width scalars occupying a lead
and a continuation cell). It is generated by `cargo xtask font-atlas --write`
into the `lib/font/src/atlas.rs` + `atlas_coverage.bin` the kernel path
embeds.

The generator never names a face: it reads the `mono` family's `FontFamily`
manifest through the same `tools/xtask` store reader that plants
`/System/Fonts`, so the console's faces, the shipped store's faces and the
service's faces are one list (§2.2). The atlas is the console's fixed-cell
*view* of those faces rather than a second copy of them, and it shares the
`lib/fontface` engine with the service's runtime rasterisation, so there is
exactly one source of truth.

The committed faces carry no TrueType hinting bytecode, so that engine
grid-fits every outline itself before filling it (`lib/fontface`'s `gridfit`):
strokes snap to whole pixels, never narrower than one, and rows snap to the
face's own baseline / x-height / cap-height / ascender / descender zones so a
line of text agrees on them. Columns are snapped only on the fixed-cell path —
the atlas, and the service whenever a *monospace* family asks for its cell —
where the cell owns the advance and moving a stem costs no spacing; the
proportional path fits rows alone so ink stays under the advance the client
laid out with.
Without it the console atlas put under a tenth of its ink at full coverage at
the 8×16 cell — every stem two columns of grey.

Box Drawing and Block Elements are not rasterised at all. They exist to tile,
which an outline manages only where its hairlines land on pixel boundaries, so
`lib/fontface`'s `lineart` draws them as whole pixels computed from the cell.
Both sources of a grid's glyphs use it — the atlas here, and the service when a
monospace family asks for a cell — so a border is the same picture on the
framebuffer console and in a terminal window.

### 2.5 `/System/Fonts` — the one on-disk font store

The store is a directory per family, planted verbatim from
`lib/font/assets/<family>/` by the image pipeline (`tools/mkimage` +
`tools/xtask`), read-only within the read-only `/System` (§16.2). A
directory is a family exactly when it carries a `FontFamily` manifest
(`lib/fontface`'s `store` module parses it on both the build and the service
side, so the two can never disagree):

```
/System/Fonts/<key>/FontFamily     label, kind, ordered faces, fallback key
/System/Fonts/<key>/<face>.ttf     the faces that manifest lists
```

`kind` is `proportional`, `monospace`, or `fallback`; a fallback family is
coverage only and is never offered to a user, which is how the three
proportional families share one set of Hebrew and CJK faces instead of
embedding three copies. Resolution is by order alone — the primary face owns
Latin, and a companion is reached only for what the primary does not map —
so there is no per-face script table to keep in step with the faces.

No atlas artifact is planted: `fontd` derives everything from the faces.
Adding a family is dropping its directory into `lib/font/assets/`; nothing
in the kernel, the service, or the image builder names a face.

`fontd` scans the store at startup and reads only the manifests — kilobytes.
A face's bytes are read on first use through the read-only handle opened
then, so a session that never draws Chinese never pays for the 17 MB
Chinese face, and a machine with little RAM is not charged for coverage it
is not using. The service uses its `CAP_FS_ACCESS` only against the store,
and `/System` is read-only so the reach can never write (minimum authority,
§19.5, §5.4).

### 2.6 Secondary defect — shippable image ships debug userland

`build_platform_image` builds the kernel with the Cargo profile matching the
image profile (`kernel_build_profile`: `installer`→`--release`,
`debug`→debug), but the app/driver `Run` binaries always go through
`pie_build::cross_compile_pie_elf`, which is hardcoded to the **debug**
profile. So the shippable `installer` image ships an optimised kernel beside
unoptimised userland/drivers. Thread the image profile through
`cross_compile_pie_elf` (mirroring `kernel_build_profile`) so `installer`
ships release-built userland/drivers, `debug` stays debug, and QEMU
integration-test images stay debug (fast iteration). This is independent of
the font work and is fixed in its own step.

### 2.7 Variable faces and real weights

The shipped faces are upstream **variable** fonts, committed unmodified.
`lib/fontface` instantiates a design-axis coordinate at parse time
(`fvar`/`avar` normalisation, `gvar` tuple deltas with IUP, `HVAR` advance
variations), so `FontWeight` renders the weight the type designer drew
rather than a synthesised approximation, and the advance changes with it as
it should. A face with no `wght` axis is thickened instead by the service's
bounded sub-pixel stroke, which leaves its advance alone. This is why the
store ships one file per family rather than one per weight, and why a
family's manifest names no weights.

---

## 3. Status — done

The migration is complete: the ~10 MB font payload no longer lives in any app.
`/System/Fonts` holds one directory per family — `inter`, `noto-sans`,
`noto-serif`, the `mono` console family, and the shared `sans-fallback`
coverage set — discovered at startup; `fontd` is the only process that parses a
face or runs the outline rasteriser, and every other process draws through the
thin `lib/font` client over `FONT_ENDPOINT`.

Load-bearing facts a future reader needs:

- **Protocol** (`lib/abi/src/font_ipc.rs`, `FONT_ENDPOINT = 0x464E_5400`,
  registered in `is_reserved_endpoint` as a privileged bind). A fixed 36-byte
  `FontRequest` — `Glyph { family, scalar: char, pixel_height, weight }` /
  `Metrics { family, pixel_height, weight }` / `Families`, the family a
  validated `FamilyKey` and the scalar a `char` so a stray byte or a surrogate
  is unrepresentable, the weight a closed `FontWeight` decoded from its wire
  value — and a status-framed reply: a glyph reply (`width`, `height`,
  `advance`, `left`, then `width*height` 8-bit samples, bounded by
  `FONT_MAX_GLYPH_REPLY`; `width == 0` is an ink-less glyph), the
  `FontMetrics { pixel_height, baseline, line_height, monospace_advance }`
  (where `monospace_advance == 0` *means* proportional), or up to
  `FONT_MAX_FAMILIES` `FamilyEntry` (key, label, kind) rows. One shared
  `glyph_coverage_len` bound governs encode and decode. Pixel height is bounded
  by `FONT_MIN/MAX_PIXEL_HEIGHT` (8..=512) — a validation bound. Not part of the
  curated C-ABI surface, so the generated C headers carry no font view. The
  request/reply decoders are in the `fuzz_decode` harness; the `lib/fontface`
  TrueType parser has its own `tests/fuzz_face.rs`.
- **Console atlas** (`lib/font/src/atlas.rs` + `atlas_coverage.bin`,
  regenerated by `cargo xtask font-atlas --write` from the whole `mono` family,
  §1.1/§2.4). Every face the family lists is compiled in — 23,602 cells in
  1.6 MB at the 8×16 cell — because the console runs in the kernel and cannot
  ask this service for a glyph, so a face left out is a script no console could
  ever draw. `lib/fbcon` and the render client's const-fn geometry read it; only
  a scalar no face maps shows U+FFFD. There is no precomputed full-Unicode
  atlas artifact.
- **Service** (`userland/system/fontd`, `/System/Services/fontd.app`). A dual
  library + `Run`-binary crate modelled on `sysinfod`. `discovery::discover`
  scans the store through the injected `FontStore`/`FaceLoad` seams (bounded to
  `FONT_MAX_FAMILIES`, sorted by key, a malformed family skipped with a
  `FAMILY_SKIPPED` warning, an empty store fatal), so the whole
  discovery-to-serve pipeline is host-tested from an in-memory fixture. The
  host-testable `FontService` dispatcher owns those families, their lazily-read
  faces and per-weight parsed instances, and a byte-budgeted `(requesting
  family, resolved family, face, glyph, pixel height, cells, weight)` glyph
  cache (§3.1) — both families in the key because two families sharing a
  fallback face rasterise it at their own primary face's geometry, and the cell
  count because a face maps every scalar it does not cover onto one replacement
  glyph, so without it a double-width scalar's two-cell bitmap would be served
  for a single-width one. It resolves a scalar
  through the family's own faces, then its fallback family's, then U+FFFD;
  rasterises through the shared `lib/fontface` engine at the primary face's
  geometry (4-bit `×17` → the protocol's 8-bit samples) so a run shares one
  baseline and box height; and always emits a reply (status-word error frame on
  failure, fail closed — an unknown family key is `NotFound`, never a
  substitution). The `Run` binary serves from a wait set carrying both
  `FONT_ENDPOINT` and the kernel's `WaitSourceKind::MemoryPressure` source, so
  it reacts to a band change while idle without polling either. Its manifest
  requests `CAP_IPC_BIND_PRIVILEGED`, `CAP_FS_ACCESS` (the manifest scan and
  the first-use face reads through the secured VFS — `fs_open` is
  capability-gated regardless of the file's mode; `/System` is read-only so no
  write reach), and `CAP_LOG_EMIT`, and the `fontd` service account (uid 15,
  `FONTD_CEILING`) grants exactly those three.
- **Weights.** A face declaring a `wght` axis is instanced at the requested
  weight's OpenType coordinate (400/500/700) and cached per (face, weight), so
  the glyph *and* its advance are the ones the designer drew (§2.7). Only a
  face without that axis falls to the synthetic stroke
  (`userland/system/fontd/src/embolden.rs`): em/48 (Medium) or em/24 (Bold) —
  the strength a stroke-widening rasteriser applies for a synthetic bold, as
  FreeType's `FT_GlyphSlot_Embolden` does — carried in 1/256 px fixed point and
  applied to the 8-bit coverage, never the outline. That stroke is
  **horizontal only**, so the baseline, box height, and pen advance are
  unchanged and a synthetic bold run occupies exactly what its regular twin
  would; `Regular` adds a zero stroke.
- **Client** (`lib/font`, `render` feature). `BitmapFont` is a thin cached
  `FONT_ENDPOINT` client with the same public API; the four TTF embeds
  (`cache.rs`) and the full atlas are deleted. The transport is a
  process-global `FontTransport` seam: real programs link `tairix-font/rt`
  (routing through `tairix_rt::ipc_call`), host tests install a mock, and with
  no transport a draw fails closed. Its glyph cache (§3.1) is installed
  through the parallel `set_glyph_cache` seam and defaults lazily under `rt`.
  GUI `Run` images no longer carry the ~10 MB `R` LOAD segment.
- **Image + discovery.** `image_apps::system_font_files` plants every family
  directory under `lib/font/assets/` — its `FontFamily` manifest and exactly
  the faces that manifest names — at `/System/Fonts/<key>/` in the shared
  `app_store_files` (discovered from the assets tree, never a list), and
  `fontd.app` is auto-discovered under `/System/Services`. `fontd` is **not** a
  boot-floor service (`init`'s `DEFAULT_CONFIG` does not name it): text
  rendering is a
  graphics-only resource, so **`login` starts `fontd`** (as its uid-15 service
  account, via `CAP_SPAWN_AS_USER`) the first login round a machine is
  display-capable — covering both a graphical login and the shell's `desktop`
  command, and never on a headless/text-only boot (§17.3). **This `login`-owned
  start is a transitional ad-hoc placement that `plans/NEW-SERVICEMANAGER.md`
  (SVC-5) removes:** once the service manager gains readiness-condition
  activation, `fontd` becomes an on-demand, `display-present`-gated service and
  the `login` start path plus the x86_64/riscv64 compiled-in fallbacks below
  are deleted (§2.14). Until then, login resolves it by
  path through the ordinary program gate: from the on-disk `/System/Services`
  bundle on aarch64, and from the compiled-in program registry
  (`spawn_paths::FONTD_PATH`, `program_manifests::FONTD_MANIFEST`,
  `spawn_layout::SPAWN_PROGRAMS`, `build.rs`) on x86_64/riscv64 until their
  storage floors land — a *spawnable* program there, not an init-auto-started
  service. Post-boot start is the headless-first-correct design on its own
  (§17.3); an earlier worry that a 5th concurrent boot service crashed the
  kernel (D18) was investigated and closed non-reproducing once this service's
  ~10 MB payload was removed (`plans/OPEN-DEFECTS.md`).
- **Profile fix (§2.6).** The image → Cargo-profile mapping lives once on
  `tairix_mkimage::ImageProfile`; both `kernel_build_profile` and
  `pie_build::cross_compile_pie_elf` read it, so `installer` cross-compiles
  userland/driver `Run` binaries `--release` while `debug`/QEMU images stay
  `dev`. Every `(arch, profile)` bundle memo in `image_apps`/`image_drivers` is
  re-keyed through the shared `memo_slot`.

### 3.1 The one glyph-cache declaration (both sides of the endpoint)

The client's memoised replies and the service's memoised rasters are the same
kind of memory, so they are **one declaration**, in `lib/font/src/glyph_cache.rs`
(feature `glyph-cache`, pulled in by `render`; `fontd` depends on that feature
alone, so it takes none of the drawing dependencies):

- `CachedGlyph` — the retained value (`width`, `height`, `advance`, `left`,
  owned coverage) and its `CachedBytes` impl: payload is the coverage length,
  `wipe` zeroes it.
- `glyph_cache_candidate(owner)` — class `DisposableUi`, `RebuildCost::Expensive`,
  `Sensitivity::UserData` (so every released entry is overwritten — the set of
  cached glyphs reveals which characters a user has had displayed),
  `InvalidationSource::OwnerTeardown`, `ReclaimRule::Drop`.
- `glyph_cache_budget(total_ram_bytes)` — `CacheBudget::from_ceiling(total /
  4096)`. A glyph working set is a few hundred bitmaps, so this is deliberately
  far below the 1/16th a kernel-heap-backed cache takes: 256 KiB on a 1 GiB
  machine, 16 MiB on a 64 GiB one. **Zero total RAM yields a zero budget**,
  which admits nothing and leaves everything served uncached — correct, merely
  slower, never a hand-picked fallback.

Each side builds its own `tairix_reclaim::ReclaimCache` from that declaration
with its own key (the client's `(scalar, family, pixel height, weight)`, the
service's `(requesting family, resolved family, face, glyph, pixel height,
cells, weight)`) and a `()` generation, since nothing invalidates a glyph while the
faces are loaded. Both are owned by `ReclaimOwner::UserlandProcess`
(`"font-client"` / `"fontd"`), the variant for a cache that cannot resolve a
numeric task id.

Why this matters on the service side: the pixel height is **caller-supplied**,
and the widest permitted bitmap is `FONT_MAX_GLYPH_WIDTH ×
FONT_MAX_PIXEL_HEIGHT` (512 KiB), so an entry-counted bound was a byte bound in
the hundreds of megabytes a hostile client could walk it up to. The byte budget
closes that; the protocol's own size validation (§2.2) is a separate, unchanged
security bound and is what refuses an out-of-range request in the first place.

### 3.2 The client cache only works if the process knows its band

`ReportedPressure` starts at `PressureBand::Critical` and `growth_permitted`
is true only at `Normal`, so a client process that never publishes a band
admits **nothing**: every character drawn becomes one `FONT_ENDPOINT` round
trip, for the life of that process, and `fontd` carries the whole desktop's
per-glyph traffic. That is a silent hundredfold cost, not a degraded cache, so
the wiring is load-bearing and is defined once rather than per program:

- `tairix_procinfo::pressure` is the single definition. `watch(set, token)`
  adds the `WaitSourceKind::MemoryPressure` member **and** primes the gauge
  with the band in force (the wake reports only *changes*, so neither half
  works alone); `refresh()` re-reads on the wake and reports whether it moved.
  Its `refresh_into(transport, gauge)` core is host-tested against a fixture.
- Every `Run` binary that links `tairix-font/rt` arms it — `files`,
  `terminal`, `viewer`, `wallpaper`, `widgets`, `switchboard`, and the desktop
  `session` (which hosts the compositor's and taskbar's caches too) — and on
  the wake calls `tairix_font::trim_glyph_cache()` alongside its own caches,
  so glyph memory is returned when the band moves rather than at the next
  draw. `fontd` arms the same member for its service-side cache.
- `lib/font`'s lazy `rt` cache constructor primes the band in the same breath
  as its RAM read, so a cache is never *born* against the fail-closed unknown
  band even before its program's loop is up.

## 4. Cross-references

- `AGENTS.md` §2.2, §2.3, §2.14, §5.2, §5.4, §16.2, §16.4, §16.5, §18.3,
  §19.5, §19.6, §17.3 — the rules this plan enforces.
- `plans/FIX-DESKTOP.md` — the async launch (done) and the demand-paged/CoW
  image build (DESK-4..7, planned); this plan removes the *payload* the
  launch path must move, complementary to shrinking the *per-page* cost.
- `plans/NEW-SERVICEMANAGER.md` — the first-class service manager that (SVC-5)
  replaces the transitional `login`-starts-`fontd` placement (§3) with
  readiness-condition on-demand activation, deleting the `login` start path.
- `plans/DISPLAY.md`, `plans/COMPOSITOR-WORK.md`, `plans/GUI-CONTROLS-DESIGN.md`
  — the text-drawing consumers of the font client.
- `lib/abi/src/{window,display,net}_ipc.rs` — the reserved-endpoint service
  protocol pattern `font_ipc.rs` follows.
- `lib/font`, `lib/fontface`, `lib/fbcon`, `tools/xtask` `font-atlas` — the
  crates this plan refactors.
- `plans/SMARTRAM.md`, `lib/reclaim` — the reclaimable-memory model both glyph
  caches (§3.1) are built from.
