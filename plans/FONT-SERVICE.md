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
kernel/headless text console. The full-Unicode 3.6 MB atlas is dominated by
the CJK (Japanese/Korean) companion faces; the console does not need those
megabytes compiled into the kernel. So the atlas is split (see §2.4): a small
compiled-in **console atlas** — the *primary Inconsolata EX face's whole
repertoire* (ASCII, Latin-1, Latin Extended, Greek, Cyrillic, box drawing,
arrows, punctuation, currency, U+FFFD; single-cell) — stays with the kernel
path, and the CJK + Hebrew companions are no longer compiled in anywhere.

**Consequence (deliberate, agreed):** the kernel/headless framebuffer text
console renders the primary Latin face's repertoire and shows U+FFFD for a
CJK/Hebrew scalar; rich CJK/Hebrew text is available through the graphical
terminal, which draws via the font service. The primary-face boundary keeps
the deliberately-fixed Ukrainian Cyrillic console working while shedding the
multi-megabyte CJK bulk. `lib/fbcon` tests that asserted CJK/Hebrew console
ink are updated to the new (fallback) behaviour.

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

- **Owns the font payload**, loaded **once** from `/System/Fonts/` at start
  (the four committed TrueType faces). No font bytes live in any other
  process.
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
  [`FontRequest::Glyph { scalar, cell_height }`] reply is the 8-bit coverage
  bitmap the client blits. The service resolves the scalar to the covering
  face (Latin→Inconsolata, JP→MPLUS, KR→D2Coding, HE→Noto, else U+FFFD),
  rasterises once at the requested cell height (4-bit engine coverage scaled
  ×17 to the protocol's 8-bit samples — byte-identical to the old
  atlas/scaled blitter), and memoises in the byte-budgeted `(face, glyph,
  height, weight)` cache of §3.1. It also answers
  [`FontRequest::Metrics`] from `CellGeometry::derive` for any client that
  holds no geometry of its own.

### 2.2 `FONT_ENDPOINT` — the IPC protocol (`lib/abi/src/font_ipc.rs`)

A new reserved-endpoint protocol modelled on `window_ipc.rs` /
`display_ipc.rs`: a fixed-size, bounds-checked request framing and a
length-prefixed coverage reply, both `#[repr(C)]`, versioned and hashed
under the same ABI discipline as the syscall table (§9) and frozen on the
first release (mutable now — `abi-v1` is not frozen). The generated C view
follows (`cargo xtask c-header`).

- Reserved id `FONT_ENDPOINT` (ASCII-hex-spelled, per the existing
  convention; register with `crate::ipc::is_reserved_endpoint`).
- Requests: `Glyph { scalar: u32, cell_height: u32, weight }` and a one-shot
  `Metrics { cell_height }` returning the monospace cell geometry the client
  needs to lay text out. Fixed max request length; reject anything else,
  fail closed (§5.4).
- Reply: `{ width, height, advance, bytes[coverage] }`, length-bounded.
- **No new capability** (§5.2): drawing text is not a security boundary, so
  the endpoint is callable by any process (the *action* still validates every
  field and fails closed). The service holds the only privileged thing — the
  `/System/Fonts` read authority — and only at startup.

### 2.3 `lib/font` becomes the thin client (+ protocol reuse)

`lib/font` keeps its public shape for consumers (`BitmapFont::draw_text`,
`with_pixel_height`, the `glyph`/blitter surface) but its body changes:

- The `render` path no longer embeds TTFs or runs `lib/fontface`
  in-process. It sends a `FONT_ENDPOINT` request and blits the returned
  coverage, caching replies locally per `(scalar, height, weight)` in the
  byte-budgeted cache of §3.1, so steady-state redraws issue no IPC.
- The `include_bytes!` of the four faces (`cache.rs`) and the full atlas
  (`atlas.rs` / `atlas_coverage.bin`) are **deleted** from the crate (§2.14).
- Protocol request/reply encoders live in `lib/abi::font_ipc` and are shared
  by client and service (§2.2), never re-spelled.

### 2.4 Kernel console atlas subset (`lib/fbcon` / kernel path)

The framebuffer text console cannot call a service (boot floor). It keeps a
**small, compiled-in console atlas** — the primary Inconsolata EX face's
whole repertoire (ASCII, Latin-1, Latin Extended, Greek, Cyrillic, box
drawing, arrows, punctuation, currency, U+FFFD; single-cell) — generated by
`cargo xtask font-atlas --write` into the same `lib/font/src/atlas.rs` +
`atlas_coverage.bin` the kernel path already embeds, now containing only that
subset. The generator builds it from the primary committed face alone; the
CJK + Hebrew companions are not compiled in anywhere. The one runtime
rasterisation source (the faces `fontd` loads) and this one compiled-in
console subset share the same `lib/fontface` engine, so there is still
exactly one source of truth (§2.2). There is no precomputed full-Unicode
atlas artifact.

### 2.5 `/System/Fonts` — the one on-disk font store

The image builder plants the four committed TrueType faces under
`/System/Fonts/` **once** (`tools/mkimage` + `tools/xtask` image pipeline),
read-only within the read-only `/System` (§16.2). No atlas artifact is
planted — `fontd` derives everything from the faces. `fontd` opens the faces
at startup with a one-shot read authorised by its `CAP_FS_ACCESS` and then
holds only the parsed in-memory faces; it retains no open fd for its serving
lifetime, and `/System` is read-only so the reach never writes (minimum
authority, §19.5, §5.4).

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

---

## 3. Status — done

The migration is complete: the ~10 MB font payload no longer lives in any app.
`/System/Fonts` holds the four committed TrueType faces; `fontd` is the only
process that parses a face or runs the outline rasteriser, and every other
process draws through the thin `lib/font` client over `FONT_ENDPOINT`.

Load-bearing facts a future reader needs:

- **Protocol** (`lib/abi/src/font_ipc.rs`, `FONT_ENDPOINT = 0x464E_5400`,
  registered in `is_reserved_endpoint` as a privileged bind). A fixed 20-byte
  `FontRequest` — `Glyph { scalar: char, cell_height, weight }` / `Metrics {
  cell_height }`, the scalar a `char` so a surrogate is unrepresentable and the
  weight a closed `FontWeight` decoded from its wire value — and a
  status-framed reply: a glyph reply (`width`, `height`, `advance`, then
  `width*height` 8-bit samples, bounded by `FONT_MAX_GLYPH_REPLY`) or the
  `FontMetrics { cell_width, cell_height, baseline }`. One shared
  `glyph_coverage_len` bound governs encode and decode. Cell height is bounded
  by `FONT_MIN/MAX_CELL_HEIGHT` (8..=512) — a validation bound. Not part of the
  curated C-ABI surface, so the generated C headers carry no font view. The
  request/reply decoders are in the `fuzz_decode` harness; the `lib/fontface`
  TrueType parser has its own `tests/fuzz_face.rs`.
- **Console atlas** (`lib/font/src/atlas.rs` + `atlas_coverage.bin`,
  regenerated by `cargo xtask font-atlas --write` from the primary Inconsolata
  EX face alone, §1.1/§2.4). The compiled-in atlas is the primary face's whole
  repertoire only (~350 KB); the CJK + Hebrew companions are compiled in
  nowhere. `lib/fbcon` and the render client's const-fn geometry read it; the
  boot/headless console shows U+FFFD for a CJK/Hebrew scalar. There is no
  precomputed full-Unicode atlas artifact.
- **Service** (`userland/system/fontd`, `/System/Services/fontd.app`). A dual
  library + `Run`-binary crate modelled on `sysinfod`. The host-testable
  `FontService` dispatcher owns the parsed faces and a byte-budgeted `(face,
  glyph, cell height, weight)` glyph cache (§3.1), resolves a scalar to its
  covering face, rasterises through the shared `lib/fontface` engine (4-bit
  `×17` → the protocol's 8-bit samples; a `Regular` request is byte-identical
  to the old blitter), thickens the coverage to the requested weight, and
  always emits a reply (status-word error frame on failure, fail closed). The
  `Run` binary serves from a wait set carrying both `FONT_ENDPOINT` and the
  kernel's `WaitSourceKind::MemoryPressure` source, so it reacts to a band
  change while idle without polling either. Its manifest requests
  `CAP_IPC_BIND_PRIVILEGED`, `CAP_FS_ACCESS` (the one-shot startup read of the
  faces through the secured VFS — `fs_open` is capability-gated regardless of
  the file's mode; `/System` is read-only so no write reach), and
  `CAP_LOG_EMIT`, and the `fontd` service account (uid 15, `FONTD_CEILING`)
  grants exactly those three.
- **Synthesised weights** (`userland/system/fontd/src/embolden.rs`). The four
  committed faces ship one weight each, so a theme's `Medium`/`Bold` role is
  rasterised from the same outline and thickened in the service: a stroke of
  em/48 (Medium) or em/24 (Bold) — the strength a stroke-widening rasteriser
  applies for a synthetic bold, as FreeType's `FT_GlyphSlot_Embolden` does —
  carried in 1/256 px fixed point and applied to the 8-bit coverage, never the
  outline. The stroke is **horizontal only**, so the baseline, cell height, and
  pen advance are unchanged and layout is weight-independent; `Regular` adds a
  zero stroke. A weighted face added to `/System/Fonts` later would replace the
  synthesis without changing the protocol or the theme ladder.
- **Client** (`lib/font`, `render` feature). `BitmapFont` is a thin cached
  `FONT_ENDPOINT` client with the same public API; the four TTF embeds
  (`cache.rs`) and the full atlas are deleted. The transport is a
  process-global `FontTransport` seam: real programs link `tairix-font/rt`
  (routing through `tairix_rt::ipc_call`), host tests install a mock, and with
  no transport a draw fails closed. Its glyph cache (§3.1) is installed
  through the parallel `set_glyph_cache` seam and defaults lazily under `rt`.
  GUI `Run` images no longer carry the ~10 MB `R` LOAD segment.
- **Image + discovery.** `image_apps::system_font_files` plants the four faces
  under `/System/Fonts` in the shared `app_store_files`, and `fontd.app` is
  auto-discovered under `/System/Services`. `fontd` is **not** a boot-floor
  service (`init`'s `DEFAULT_CONFIG` does not name it): text rendering is a
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

- `CachedGlyph` — the retained value (`width`, `height`, owned coverage) and
  its `CachedBytes` impl: payload is the coverage length, `wipe` zeroes it.
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
with its own key (the client's `(scalar, height, weight)`, the service's
`(face, glyph, height, weight)`) and a `()` generation, since nothing
invalidates a glyph while the faces are loaded. Both are owned by
`ReclaimOwner::UserlandProcess` (`"font-client"` / `"fontd"`), the variant for
a cache that cannot resolve a numeric task id.

Why this matters on the service side: the cell height is **caller-supplied**,
and the widest permitted bitmap is `FONT_MAX_GLYPH_WIDTH × FONT_MAX_CELL_HEIGHT`
(512 KiB), so an entry-counted bound was a byte bound in the hundreds of
megabytes a hostile client could walk it up to. The byte budget closes that;
the protocol's own size validation (§2.2) is a separate, unchanged security
bound and is what refuses an out-of-range request in the first place.

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
