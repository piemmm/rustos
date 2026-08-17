# FIX-DISPLAY-ACCELERATION — Wire hardware-accelerated desktop composition end-to-end

Status: **planned** (staged below; no stage is optional and none defers
work as "future").

Binding under `AGENTS.md`. This plan turns the *already-designed but
dead* acceleration path into a working, first-class pipeline that
meaningfully speeds up desktop composition on QEMU (all bare-metal
Tier-1 targets) and on Raspberry Pi 4 hardware, while keeping the
software `Display` path as the mandatory, always-available fallback
(§17.3).

Read first (§15.18): `plans/DISPLAY.md` (seat model + `DISPLAY_ENDPOINT`
semantics), `plans/GUI-CONTROLS-DESIGN.md` and `plans/COMPOSITOR-WORK.md`
(what the WM composites), `plans/FIX-DESKTOP.md` (non-blocking launch /
responsiveness the vsync work feeds), `plans/PI.md` (Pi 4 bring-up,
`rpi_hvs`), `plans/USB.md`/`plans/NETWORK.md` only insofar as they show
the `lib/virtio` + `lib/drvrt` + `lib/dma-barrier` user-space-driver
precedent, `docs/src/drivers/display.md`.

---

## The defect this closes

Acceleration was *designed in* but is **not reachable at runtime** and,
where reachable, **does not actually save work**:

1. **The accelerated path never crosses the process boundary.** The
   runtime path is `userland/gui/session/run.rs` →
   `Compositor::composite()` (software blend into one back buffer) →
   `RemoteDisplay::present`/`present_rects` → `DisplayClient` `ipc_call`
   to `DISPLAY_ENDPOINT` → `DisplayServer` in the driver's `Run` binary →
   software `Display::present`. `lib/display` speaks a **software-frame-
   only** wire protocol (`DisplayRequest::{Query,Configure,Present}`,
   `lib/abi/src/display_ipc.rs`): one shm frame region indexed by frame
   number plus a `DamageList` of up to `MAX_DAMAGE_RECTS` rectangles.
   `RemoteDisplay` implements only `Display`, **not**
   `AcceleratedDisplay`. So `Compositor::present_accelerated`
   (which already exists) is exercised only by an in-process mock in
   `userland/gui/wm/src/tests.rs` — it is dead in production.

2. **`drivers/display/gpu_virtio` is an empty placeholder** (`#![no_std]`
   and nothing else — no `register`, no `BIND_KEYS`). This is *the* QEMU
   acceleration path on aarch64 `virt`, riscv64 `virt`, and x86_64 q35
   (all expose `virtio-gpu`, virtio device id 16), and it does not exist,
   so `devmgr` can never autoload it.

3. **Even the "accelerated" path composites in software and double-
   copies.** `Compositor::encode_layers` bakes each window into a CPU
   `LayerBuf` (per-pixel `sample_local`, including rounded corners and
   opacity), then `rpi_hvs::upload_plane` copies that buffer *again* into
   the plane MMIO window every frame. Apps already deliver their pixels
   in shared memory via the window channel (`lib/window`), so both copies
   are avoidable. `AccelLayer.pixels: &'a [u8]` is an in-address-space
   slice that *cannot* cross a process boundary — it structurally forces
   the copy.

4. **No damage on the accelerated path.** `present_layers` reprograms and
   re-uploads *all* planes every frame; an idle desktop with one blinking
   cursor re-uploads every window.

5. **No flip/vblank synchronisation.** There is no present-complete/vsync
   event; a real accelerated flip must be double-buffered and completion-
   signalled via `irq_bind`/`irq_wait` (never a busy-poll, §2.23), which
   also feeds `plans/FIX-DESKTOP.md` responsiveness.

6. **One backdrop-blurred window disables acceleration for the whole
   frame**, and the desktop now always has one. `Compositor::encode_layers`
   refuses the layer path outright when any visible window asks for a
   backdrop blur, because a hardware plane cannot read what is beneath it;
   the frame falls back to the software composite. The taskbar and every
   popup it opens are floating chrome over a blurred backdrop
   (`plans/GUI-CONTROLS-DESIGN.md`, "Surface ground"), so on a desktop
   session that refusal is permanent, not occasional: the accelerated path
   as it stands can only serve a headless or bar-less configuration. This
   is a real cost of the desktop's look, not a bug in either half — the
   software path is the mandatory always-available fallback (§17.3) and
   `plans/FIX-DESKTOP-SPEEDUP.md` is what keeps it fast. Closing it means
   compositing *only* the frosted surfaces in software and handing the
   hardware the rest: the blurred region is bounded (the bar and one popup),
   its backdrop is what the layers below already contain, and a baked layer
   is exactly the shape Stage A gives such a surface. Until that lands, the
   accelerated path's win is measured on scenes without chrome, and a
   measurement quoted from one must say so.

---

## Goal / invariants (bind every stage)

- **Compositing policy stays in userland** (§4 microkernel-leaning,
  §17.3): the kernel only issues framebuffer / MMIO / DMA / IRQ
  capabilities. `gpu_virtio` is a user-space driver built on `lib/virtio`
  + `lib/drvrt` + `lib/dma-barrier`, exactly like the existing virtio/USB
  user-space drivers.
- **Generic code stays board-neutral** (§2.20): `gpu_virtio`,
  `lib/display`, `lib/virtio`, `lib/abi`, and the WM carry no SoC/board
  name, constant, or `cfg`. `rpi_hvs` may know its hardware because it is
  the device's own leaf driver, reached only via discovery-match (§18.3).
- **One raster/blend path** (§2.2): the software fallback remains
  `lib/raster`; no second blend is forked. The WM composites in software
  *only* the layers hardware cannot source directly; those baked results
  become their own layer.
- **The engine is never handed a blend that would band**
  (`plans/FIX-DESKTOP-SPEEDUP.md` B.5). A hardware layer is blended in the
  scan-out's own 8 bits with one fixed rounding, so a *translucent field*
  over a picture arrives in the `256 - a` levels that leaves and steps a
  smooth wallpaper into plateaus; the software composite spends that
  missing resolution across the area with a per-pixel ordered dither, which
  no layer stack can express. `Compositor::has_translucent_window`
  therefore refuses the layer path for a window-wide translucency exactly
  as a backdrop blur does. A layer's own antialiased *edge* is not this
  case — a few pixels of partial coverage have no gradient to band — so
  ordinary rounded windows keep the hardware path. A stage that wants the
  hardware to blend translucency must first give the engine an honest,
  *proven* way to say it blends without banding (a high-precision or
  dithered blend); until such a capability has a live producer and
  consumer it is not added (§2.4), and translucency is baked or refused.
- **Roll our own** (§2.12): the virtio-gpu protocol is implemented in-
  tree on `lib/virtio`, not an external crate.
- **Fail closed, seat-gated, no ambient authority** (§5.4): every
  accelerated present checks the live `SeatLease` **first** (already true
  for `rpi_hvs::present_layers`); a layer stack that exceeds `AccelCaps`
  falls back to the software full frame — never a partial hardware frame.
- **No busy-poll** (§2.23): queue and flip completion are IRQ-driven via
  `irq_bind`/`irq_wait`, with a bounded, fail-closed hardware-handshake
  wait only where the silicon dictates.
- **Evolve the ABI in place** (§2.13/§9): `AccelLayer`/`AccelCaps` and the
  `lib/display` wire protocol change directly; every caller (WM,
  `rpi_hvs`, `lib/display`, session, tests, the generated C header via
  `cargo xtask c-header`, `cargo xtask abi-check`) is updated in the same
  change. No `v2` beside `v1`, no shim, no dead placeholder (§2.14).
- **Tests + docs in the same change** (§7, §13): host unit tests with a
  mock host for every new engine, a QEMU integration vertical for
  `gpu_virtio` (it is emulable), and updated `docs/src/drivers/display.md`
  + the `gpu_virtio`/`rpi_hvs` READMEs + the README support matrix.
- **Scalability / very-large-surface discipline** (§24, §26): every
  buffer count, resource-id table, and layer cap is a discovered/grown
  capacity, not a hand-picked `const` ceiling; per-frame work scales with
  damage, not surface size.

**Non-goal (explicitly out of scope, not deferred work):** a virgl/3D
(GPU-rendered) path. 2D transfer/flush + blob/`dmabuf` zero-copy layer
scanout is the complete, meaningful desktop-composition win; a
programmable-3D pipeline is a separate, larger effort with its own future
plan and is deliberately **not** part of this deliverable. Everything
*this* plan lists is done in full, with no no-ops.

---

## Stage A — Zero-copy layer ABI (`AccelLayer`/`AccelCaps` in place)

This is the foundational change: it makes an app's window shm frame usable
as a hardware source plane, which every later stage depends on. Landing it
first means the ABI is settled before the IPC and driver work builds on it.

### A.1 Replace `AccelLayer`'s in-process slice with a shared-memory reference
In `lib/abi/src/driver/display.rs`, change `AccelLayer` in place (§2.13):
drop `pixels: &'a [u8]` and the lifetime; add a **source reference** that
can cross a process boundary and be mapped once by the driver:
- `source: AccelSource` — an enum/struct naming a granted shm region by
  handle + byte offset (the same grant kind the window channel and
  `DisplayServer::Configure` already use), so the driver maps the client
  region **once** and the hardware sources it directly.
- keep `width_px`, `height_px`, `stride_bytes`, `dst_x`, `dst_y`,
  `opacity`.
- add `src_crop: Option<DamageRect>` — the sub-rectangle of the source
  that is valid/changed this frame (Stage D uses it for damage; a `None`
  means the whole layer). Only add it in this change because Stage D is
  the live consumer in the same plan — no speculative field lands without
  a consumer (§2.4); if Stage D slips out of a single change, the field
  lands with Stage D, not here.
- `required_len`/validation stays, re-expressed against the mapped region
  length rather than a slice, still summing extents in `u64` so a hostile
  stride/height cannot wrap (fail closed, §5.4).

### A.2 `AccelCaps` — only fields with a live producer and consumer
Add capability bits **only** as the stages that consume them land, never
ahead (§2.4). This plan's consumers are:
- `hw_scaling: bool` (Stage E) — source rect ≠ dest rect, so DPI/window
  scaling is a hardware op instead of a CPU resample. Landed in Stage E
  with the WM producer and both driver consumers, not here.
Stage A adds no speculative caps; `max_layers`, `max_width_px`,
`max_height_px`, `per_layer_opacity` stay.

### A.3 Update every caller in the same change
- `rpi_hvs`: `upload_plane` becomes "map the client region once, point the
  HVS plane at it" for a directly-sourceable layer; it keeps a copy path
  only for a WM-baked layer (rounded corners / translucency) that has no
  client region. `present_layers` sources directly where it can.
- WM `Compositor::encode_layers`/`encode_layer`: an opaque, unclipped,
  unscaled window is encoded as a **direct** `AccelSource` referencing its
  window shm frame (no `LayerBuf`, no `sample_local`); only layers the
  hardware cannot source (rounded corners, per-region alpha, translucency
  beyond `per_layer_opacity`) are baked in software into their own layer.
  A baked layer keeps the software path's own rounding: `sample_local`
  reads the ordered dither at the pixel's **screen** position, so the baked
  pixels are the ones the software composite would have written there. A
  translucency the *engine* would blend is refused outright, per the
  no-banding invariant above — baking cannot help there, because the
  banding would happen in the engine's blend, not in the bake.
- `lib/abi` C header (`cargo xtask c-header --write`) regenerated;
  `cargo xtask abi-check` green.

### A.4 Tests + docs
- `lib/abi` unit tests: `AccelSource` validation, `required_len` against
  a mapped-region length, overflow/short-region fail-closed, `src_crop`
  bounds.
- `rpi_hvs` unit tests (mock `MmioMapper`/host): direct-source layer maps
  once and does not copy; baked layer still copies; over-caps stack
  rejected; lease check precedes engine access.
- WM unit tests: opaque window ⇒ direct layer; rounded/translucent window
  ⇒ baked layer; scene exceeding caps ⇒ `None` (software fallback).
- Docs: `docs/src/drivers/display.md` (the new source model),
  `lib/abi/src/driver/display.rs` rustdoc.

---

## Stage B — Carry `AcceleratedDisplay` across the display service (`lib/display`)

Unlocks the code Stage A prepared: the WM's `present_accelerated` becomes
reachable in production, not just via the in-process mock.

### B.1 Extend the `DISPLAY_ENDPOINT` wire protocol
In `lib/abi/src/display_ipc.rs`, add two requests alongside
`Query`/`Configure`/`Present` (in place, §2.13):
- `QueryAccel { seat_id }` → replies `AccelCaps` (or a typed "no accel"),
  so the client learns the back-end's hardware caps once at bring-up.
- `PresentLayers { seat_id, layers: [wire AccelLayer; N], count }` — a
  fixed-width, bounded, self-validating layer list referencing shm grants
  the caller has already `Configure`-registered. `count` is bounded by a
  discovered `AccelCaps::max_layers`, not a magic const (§24.1). Each
  wire layer names its source region by the same grant mechanism
  `Configure` uses.
- new op discriminants `OP_QUERY_ACCEL`/`OP_PRESENT_LAYERS`; `WIRE_LEN`
  and reply lengths updated; `to_le_bytes`/`from_bytes` round-trip tested.

### B.2 Server side (`lib/display/src/server.rs`)
- `DisplayServer` handles `QueryAccel`/`PresentLayers`, gated on the
  caller's live seat lease **exactly** like `Present` (record the granting
  lease generation on `Configure`; refuse a `PresentLayers` whose lease is
  not the live one — §5.4).
- Map each layer's shm source **once** at `Configure`/first use, never per
  present (the hot path stays copy-free); forward the mapped regions to
  the driver's `AcceleratedDisplay::present_layers`.
- A back-end that is not an `AcceleratedDisplay` answers `QueryAccel` with
  the typed "unsupported" so the client falls back — never a panic (§2.9).

### B.3 Client side (`lib/display/src/client.rs`)
- `DisplayClient` gains `query_accel()` and `present_layers(...)`.
- **`RemoteDisplay` implements `AcceleratedDisplay`** in addition to
  `Display`: `accel_caps()` returns the value cached from the bring-up
  `query_accel()`; `present_layers()` marshals to `PresentLayers`. The
  software `Display` path stays as the mandatory fallback.

### B.4 Session wiring (`userland/gui/session/run.rs`)
- At bring-up, `query_accel()` once. If the back-end reports usable accel
  caps, the present loop calls `Compositor::present_accelerated(&mut
  remote_display)`; otherwise it uses the existing software
  `Compositor::present`. Both remain live; the choice is made once from
  discovered caps (no per-frame probing).
- The seat-lease gating is unchanged and now covers the layer present too.

### B.5 Tests + docs
- `lib/display` host unit tests (mock `DisplayTransport` + mock
  `AcceleratedDisplay` host): `QueryAccel` round-trip; `PresentLayers`
  maps once and forwards; a revoked lease refuses `PresentLayers` with the
  distinct `SeatRevoked`; an over-caps list falls back; a non-accel
  back-end answers unsupported.
- Session `tests.rs`: accel-capable back-end drives `present_accelerated`;
  non-accel back-end drives software `present`.
- Docs: `docs/src/drivers/display.md` protocol table; `plans/DISPLAY.md`
  cross-reference updated (§13).

---

## Stage C — The virtio-gpu driver (`drivers/display/gpu_virtio`)

The QEMU acceleration story on aarch64 / riscv64 / x86_64 — one generic,
platform-neutral user-space driver, no per-arch code (§2.20).

### C.1 Discovery + registration
- Replace the placeholder with a real crate: `pub fn register(host: &dyn
  DriverHost) -> Result<DriverHandle, DriverError>` (nothing else public,
  §8), gated on `CAP_DRV_LOAD`.
- Manifest `BIND_KEYS` matching **virtio device id 16 (GPU)** over
  virtio-pci and virtio-mmio, so `devmgr` autoloads it (§18.3); it
  receives only the MMIO/DMA/IRQ capabilities its matched hardware-tree
  node requested (§4, no ambient authority).
- README: supported hardware, transports, required capabilities,
  runtime load/unload (§8).

### C.2 virtio-gpu 2D control path (rolled on `lib/virtio`)
Implement the 2D control queue over `lib/virtio` split-virtqueues +
transport, with `lib/dma-barrier` ordering device-shared writes before the
doorbell and reads of device-written ring entries:
- `GET_DISPLAY_INFO` → active mode ⇒ `Display::mode_info`.
- `RESOURCE_CREATE_2D`, `RESOURCE_ATTACH_BACKING` (DMA-allocated backing
  via `lib/drvrt`/`dma_alloc`), `SET_SCANOUT`.
- `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`.
- `Display::present` = transfer whole frame + flush;
  `Display::present_rects` maps each `DamageRect` of the list **straight**
  onto `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH` of that rect (host-side
  damage — a real win over the whole-frame blit even before layers), so a
  scattered frame is one call's worth of small transfers.

### C.3 `AcceleratedDisplay` via blob resources / multi-scanout
- Where the QEMU build supports it, wrap each `AccelLayer` shm region as a
  **blob resource** (`RESOURCE_CREATE_BLOB`) / `dmabuf` and program
  multi-scanout for zero-copy layer scanout.
- `accel_caps()` reports real device limits (scanout count ⇒
  `max_layers`, mode extents ⇒ `max_width/height`, `per_layer_opacity`
  from what the device supports). A device without blob/multi-scanout
  reports `max_layers = 1` (and the WM falls back to software compositing
  into the single scanout) — never a fake capability (§5.4).
- Resource-id and buffer tables are grown-on-demand, not fixed `const`
  ceilings (§24.1).

### C.4 Completion via IRQ (no busy-poll)
- Bind the used-ring / config-change interrupt through the capability-
  gated `irq_bind`/`irq_wait` (§3 `kernel/irq`); the driver parks on the
  interrupt for command completion and config-change (hotplug/mode change)
  rather than spinning (§2.23). Only a bounded, fail-closed hardware-
  handshake wait (controller readiness) may briefly spin, documented.

### C.5 Tests + docs
- Host unit tests: control-queue command encode/decode against a mock
  `lib/virtio` transport; damage → transfer/flush mapping; blob-resource
  wrap; caps derivation; fail-closed on device-reported errors.
- **QEMU integration vertical** (`tests/integration/`): boot the aarch64
  (and each bare-metal target's) `virt`/q35 machine with `virtio-gpu`,
  autoload via `devmgr`, present a frame, present a damaged region, and —
  where the emulated device supports it — present a layer stack; assert
  the scanned-out result. IRQ-driven completion exercised.
- README + `docs/src/drivers/display.md` + README support-matrix row.

---

## Stage D — Damage on the accelerated path

Stop re-uploading/retransferring unchanged planes every frame.

### D.1 Per-layer source damage
- `present_layers` honours `AccelLayer::src_crop` (Stage A): a layer whose
  source is unchanged since the last present is **not** re-uploaded
  (`rpi_hvs`) / not re-transferred (`gpu_virtio` — only its damaged rect
  is `TRANSFER_TO_HOST_2D`'d). An unchanged layer whose *position* changed
  is re-programmed (plane origin / scanout rect) without re-uploading
  pixels.
- The WM computes per-window damage from its existing `damage` module and
  threads it into each layer's `src_crop`; a fully-unchanged frame
  (blinking cursor only) re-uploads only the cursor layer.

### D.2 Tests
- `rpi_hvs`/`gpu_virtio` unit tests: an unchanged layer is not re-
  uploaded/re-transferred; a moved-only layer is re-programmed without
  pixel upload; a damaged sub-rect transfers only that rect.
- WM test: idle scene with a moving cursor produces one cursor-layer
  damage and no window re-upload.

---

## Stage E — Double-buffered, vsync-synchronised flips + hardware scaling

### E.1 Double-buffer + flip-complete
- Both accelerated back-ends present into an off-screen buffer/resource
  and flip; flip-complete is signalled through the driver's IRQ
  (`gpu_virtio` used-ring; `rpi_hvs` HVS end-of-frame/vsync line via its
  bound interrupt). The session parks until the flip lands
  (`irq_wait`) — never a busy-poll (§2.23) — feeding the non-blocking
  desktop responsiveness in `plans/FIX-DESKTOP.md`.
- No tearing: a present that arrives while a flip is in flight returns
  `Busy` (already an ABI error) and the compositor coalesces, rather than
  scribbling a half-composited frame.

### E.2 Hardware scaling (`AccelCaps::hw_scaling` + source rect)
- Add `hw_scaling` to `AccelCaps` and a destination-extent field to
  `AccelLayer` **in this change**, with live producer (WM DPI/window
  scaling) and both consumers (`rpi_hvs` HVS scaler, `gpu_virtio` where
  supported) — so DPI scaling (`tairix_geometry::Scale`) and window
  scaling become hardware operations instead of CPU resamples. A back-end
  reporting `hw_scaling = false` requires source extent == dest extent and
  the WM resamples in software into a baked layer (the fallback).

### E.3 Tests
- Unit: flip-in-flight ⇒ `Busy`; scaled layer (`src != dst`) accepted only
  when `hw_scaling`; rejected/fell-back otherwise.
- QEMU vertical extended: double-buffered flip completes via IRQ; a scaled
  present matches the software-resampled reference within tolerance.

---

## Per-target acceleration delivered

| Target | Accelerated path |
|---|---|
| aarch64 / riscv64 / x86_64 (QEMU) | `virtio-gpu` 2D transfer/flush with host-side damage; blob/`dmabuf` zero-copy layer scanout; IRQ completion. One generic driver, no per-arch code (Stage C). |
| Raspberry Pi 4 (hardware) | VideoCore **HVS** hardware layer compositor (`rpi_hvs`), now reachable end-to-end and copy-free for directly-sourceable layers (Stages A/B/D/E). |
| wasm32 / plain framebuffer / VESA | No hardware layers; software `Display` — the mandatory, unchanged fallback (§17.3). Headless stays first-class. |

---

## Definition of done (whole plan)

- Every stage's code, tests, and docs land together (§7, §13); no stage
  ships a no-op, a stub, or a "later".
- `AccelLayer`/`AccelCaps` and the `lib/display` wire protocol are the
  single living definition — no `v1`/`v2` duplication, no dead placeholder
  (`gpu_virtio` is a real driver, §2.14).
- The software `Display` path remains the always-available fallback on
  every target; the accelerated path is never a bypass around the seat
  lease (§5.4).
- §23 self-review applied (security: lease-first + validated shm sources +
  fail-closed caps; correctness/multi-arch: `gpu_virtio` board-neutral,
  one blend path; no-compat/no-dead-code; tests/docs).
- Whole-project gate green: `cargo fmt --all`, `cargo xtask ci` (once),
  `cargo xtask fuzz --secs 5` (the new `PresentLayers`/virtio-gpu decoders
  get fuzz harnesses, §19.6), and `tools/ci/soak.sh both --secs 20`.
