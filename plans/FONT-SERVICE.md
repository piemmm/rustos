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

`lib/fbcon` (the framebuffer boot console) links `lib/font` with
`default-features = false` (no `alloc`) and draws boot text from the atlas
**before any service exists** — it is boot floor (§18.6). The full-Unicode
3.6 MB atlas is far more than boot text (ASCII + box-drawing) needs. So the
atlas is split (see §2.4): a small compiled-in **console atlas** stays with
the kernel path; the full atlas moves into the service.

---

## 2. Design — a single, sandboxed font service

### 2.1 `fontd` — the font service (`userland/system/fontd`)

A long-running user-space system service shipped as a signed
`/System/Services/fontd.app` bundle (§16.2, §16.5), discovered and spawned
through the normal signature + capability + interface-hash gate (§18.3) —
never baked into the kernel.

- **Owns the font payload**, loaded **once** from `/System/Fonts/` at start
  (see §2.5). No font bytes live in any other process.
- **Rasterises in a §19.5 sandbox.** The TrueType parse + outline
  rasterisation (`lib/fontface`) runs only here, in a minimum-capability
  address space: exactly the `FONT_ENDPOINT` rendezvous, a read capability
  for `/System/Fonts` at startup (dropped after load, see §2.5), and nothing
  else — no spawn, no network, no other filesystem authority. A malformed
  face faults only this sandbox; the caller gets an error and the service is
  replaced (§19.5), never a compositor/terminal crash.
- **Serves glyph coverage** over the reserved `FONT_ENDPOINT` (§2.2): a
  request names `(face-selector | scalar, cell height)`; the reply is the
  8-bit coverage bitmap the client blits. The service resolves the scalar to
  the same face the atlas would pick (Latin→Inconsolata, JP→MPLUS,
  KR→D2Coding, HE→Noto, else U+FFFD), rasterises once, and memoises
  (the bounded `(face, glyph, height)` FIFO cache that lives in
  `lib/font/src/cache.rs` today moves into `fontd`).

### 2.2 `FONT_ENDPOINT` — the IPC protocol (`lib/abi/src/font_ipc.rs`)

A new reserved-endpoint protocol modelled on `window_ipc.rs` /
`display_ipc.rs`: a fixed-size, bounds-checked request framing and a
length-prefixed coverage reply, both `#[repr(C)]`, versioned and hashed
under the same ABI discipline as the syscall table (§9) and frozen on the
first release (mutable now — `abi-v1` is not frozen). The generated C view
follows (`cargo xtask c-header`).

- Reserved id `FONT_ENDPOINT` (ASCII-hex-spelled, per the existing
  convention; register with `crate::ipc::is_reserved_endpoint`).
- Requests: `Glyph { scalar: u32, cell_height: u32 }` and a one-shot
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

- The `render`/`cache` path no longer embeds TTFs or runs `lib/fontface`
  in-process. It sends a `FONT_ENDPOINT` request and blits the returned
  coverage, caching replies locally per `(face, glyph, height)` (the same
  bounded FIFO, now client-side, so steady-state redraws issue no IPC).
- The `include_bytes!` of the four faces (`cache.rs`) and the full atlas
  (`atlas.rs` / `atlas_coverage.bin`) are **deleted** from the crate (§2.14).
- Protocol request/reply encoders live in `lib/abi::font_ipc` and are shared
  by client and service (§2.2), never re-spelled.

### 2.4 Kernel console atlas subset (`lib/fbcon` / kernel path)

The framebuffer boot console cannot call a service (boot floor). It gets a
**small, compiled-in console atlas** — ASCII + Latin-1 + box-drawing only —
generated by `cargo xtask font-atlas` into a separate, small artefact the
kernel path embeds. The full-Unicode atlas is **not** in the kernel. The
generator emits both the small console subset (kernel) and the full atlas
(planted to `/System/Fonts`, §2.5) from the one set of committed faces, so
there is still exactly one source of truth (§2.2).

### 2.5 `/System/Fonts` — the one on-disk font store

The image builder plants the committed faces (and the generated full atlas)
under `/System/Fonts/` **once** (`tools/mkimage` + `tools/xtask` image
pipeline), read-only within the read-only `/System` (§16.2). `fontd` reads
them at startup under a scoped, one-shot read capability and then holds only
the parsed in-memory faces — the disk read authority is not retained for the
service's lifetime (fail closed, minimum authority, §19.5, §5.4).

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

## 3. Deliverables (staged; each lands coherent and gate-green)

- **FS-1 — Protocol.** `lib/abi/src/font_ipc.rs`: `FONT_ENDPOINT`, request/
  reply framings, encoders/decoders, bounds checks, round-trip + fuzz tests;
  C header regenerated; reserved-endpoint registration.
- **FS-2 — Atlas split + generator.** `cargo xtask font-atlas` emits the
  small kernel console atlas and the full `/System/Fonts` atlas from the one
  face set; `lib/fbcon`/kernel path uses the console subset. `lib/font`'s
  full atlas embed removed.
- **FS-3 — Service.** `userland/system/fontd`: loads `/System/Fonts`, hosts
  `FONT_ENDPOINT`, rasterises in the §19.5 sandbox, bounded glyph cache;
  unit + mock-IPC tests; fuzz harness over the request decoder and the face
  parser (§19.6).
- **FS-4 — Client.** `lib/font` render path → `FONT_ENDPOINT` client + local
  cache; TTF/atlas embeds deleted; public API preserved so GUI consumers are
  unchanged where possible.
- **FS-5 — GUI consumers.** Every `render`-feature consumer builds against
  the client; no app embeds font data. `readelf` shows GUI `Run` images
  losing the ~10 MB LOAD segment (regression assertion).
- **FS-6 — Image + discovery.** Image builder plants `/System/Fonts` and the
  `fontd.app` service bundle; `init`/service discovery starts `fontd` before
  the desktop; headless build unaffected (§17.3).
- **FS-7 — Profile fix (§2.6).** Thread image profile through
  `cross_compile_pie_elf`.
- **FS-8 — Docs, README, gate.** `docs/src/lib/` (font client), `docs/src/`
  service + security (sandboxed rendering), `docs/src/filesystem/` (/System/
  Fonts), README matrix; full §7 gate green over the whole workspace; an
  `installer` image built and a GUI-launch QEMU vertical shows the fast,
  small-image launch.

## 4. Status

- **Investigation — done.** Root cause and constraints measured (§1, §1.1).
- **Design — done** (§2). No "future work" left implicit.
- **FS-1 — done.** The `FONT_ENDPOINT` wire protocol lives in
  `lib/abi/src/font_ipc.rs`: a fixed 16-byte [`FontRequest`] —
  `Glyph { scalar: char, cell_height }` and `Metrics { cell_height }`, the
  scalar carried as a `char` so a surrogate/out-of-range code point is
  unrepresentable in an accepted request — a variable-length glyph-coverage
  reply (status word + width + height + advance + `width*height` 8-bit
  samples, bounded by `FONT_MAX_GLYPH_REPLY`), and a fixed metrics reply
  (`FontMetrics { cell_width, cell_height, baseline }`). One
  `glyph_coverage_len` bounds check governs both the encode and decode sides
  so producer and consumer cannot diverge; the status-word convention is the
  shared `crate::reply` frame. `FONT_ENDPOINT` (`0x464E_5400`) is registered
  in `is_reserved_endpoint` (privileged bind), the decoders are enrolled in
  the `fuzz_decode` harness, and every reply decode fails closed on a corrupt
  frame. The cell-height bounds `FONT_MIN/MAX_CELL_HEIGHT` (8..=512) are the
  canonical bounds the FS-4 client will adopt in place of
  `BitmapFont::MIN/MAX_PIXEL_HEIGHT` (§2.2). Font IPC is not part of the
  curated C-ABI surface, so the generated headers are unchanged. `docs/src`
  prose is deferred to FS-8 with the service/client, matching how the sibling
  `display_ipc`/`window_ipc` wire protocols are documented.
- **FS-2 … FS-8 — planned.**

## 5. Cross-references

- `AGENTS.md` §2.2, §2.3, §2.14, §5.2, §5.4, §16.2, §16.4, §16.5, §18.3,
  §19.5, §19.6, §17.3 — the rules this plan enforces.
- `plans/FIX-DESKTOP.md` — the async launch (done) and the demand-paged/CoW
  image build (DESK-4..7, planned); this plan removes the *payload* the
  launch path must move, complementary to shrinking the *per-page* cost.
- `plans/DISPLAY.md`, `plans/COMPOSITOR-WORK.md`, `plans/GUI-CONTROLS-DESIGN.md`
  — the text-drawing consumers of the font client.
- `lib/abi/src/{window,display,net}_ipc.rs` — the reserved-endpoint service
  protocol pattern `font_ipc.rs` follows.
- `lib/font`, `lib/fontface`, `lib/fbcon`, `tools/xtask` `font-atlas` — the
  crates this plan refactors.
