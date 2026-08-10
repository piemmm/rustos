# FIX-DESKTOP-SPEEDUP — Software-compositor and GUI redraw performance

Status: **planned**. Stages A–E need no hardware acceleration, no kernel
change, and no new syscall; F needs a `plans/FIX-HARDWARE-FEATURES.md`
correction; G is gated on a User decision (§15.7) because it is kernel work.

Binding under `AGENTS.md` (§3, §15.18). This plan closes the standing
performance defect that the desktop repaints **orders of magnitude more
pixels than a frame changes**, and does the remaining work with a
per-pixel scalar loop. It is the software half of desktop performance;
`plans/FIX-DISPLAY-ACCELERATION.md` is the hardware half. Neither
depends on the other, and the software path is the mandatory
always-available fallback on every target (§17.3), so this work is not
made redundant by acceleration — it is what runs when acceleration is
absent, refused, or falls back (a backdrop-blur frame always does).

**The order is binding: (0) measure, and check you measured the right
binary → (1) stop doing the work → (2) do the remaining work faster.**
Vectorising a loop that should not be running is forbidden; Stage F may
not land before Stages B–C.

---

## Read first (§15.18)

- `AGENTS.md` §2.16 (performance first-class, measure don't guess), §2.2
  (one definition, one blend path), §2.9/§5.4 (no panic, fail closed),
  §2.23 (no busy-poll), §17.1 (tickless, one-shot timers), §24.1/§24.4
  (grown capacities vs fixed security bounds), §26.2/§26.3 (contended,
  memory-pressured operating conditions), §27 (foundational primitives
  are complete).
- `plans/FIX-DISPLAY-ACCELERATION.md` — Stage B's `PresentLayers` request
  shape (Stage E here must evolve the *same* wire protocol, not a second
  one) and Stage D's per-layer damage (Stage C here produces it).
- `plans/GUI-CONTROLS-DESIGN.md` — the Reactive Alloy control model
  Stage C adds damage reporting to; the drawing recipes must not change.
- `plans/COMPOSITOR-WORK.md` — server-side window furniture, the chrome
  cache Stage D's frost cache is modelled on.
- `plans/FIX-DESKTOP.md` — the non-blocking-launch work; Stage E's frame
  pacing shares its "an interactive loop never stalls" rule.
- `plans/SMARTRAM.md` — `lib/reclaim`; every cache this plan adds is a
  reclaim client with a budget, never an unbounded retainer.
- `plans/FONT-SERVICE.md` — font/glyph ownership; Stage C's text-measure
  memo belongs in the font layer, not in `lib/controls`.
- `plans/FIX-HARDWARE-FEATURES.md` — `lib/cpuops`, the `lib/pagezero`
  candidate template Stage F copies, and the P3b axis correction Stage F
  depends on.
- `plans/NEW-SWITCHBOARD.md` — where Stage A's frame counters surface.
- `plans/WIRING.md`, `plans/ARCHSUPPORT.md` — Stage G's per-port work.

---

## The defect this closes

A single pointer-motion sample over a control-rich window costs a
full-window repaint and a full-window recomposite. Traced end to end:

| # | Stage | What happens now | Where |
|---|---|---|---|
| 1 | kernel → session | one `InputEvent::PointerMoved` per hardware sample, no coalescing | `userland/gui/session/src/device.rs` `DeviceInputSource::poll` |
| 2 | shell | **per sample**: router → taskbar `present()` → `sync_active_frame` → `refresh_cursor` | `session/src/shell.rs` `DesktopShell::handle` |
| 3 | fold | app-ward outcomes coalesce latest-wins (correct) — but not the work in (2) | `shell.rs` `fold_outcome` |
| 4 | app | the container fans the motion to **every** child; each does `Rect::contains` + a state write | `lib/controls` `Toolbar::on_pointer`, `Panel::on_pointer`, `paint::pointer_activation` |
| 5 | app | any hover flip fails the host's `PartialEq` render gate → **the whole window surface is repainted** | `lib/controls/src/state.rs` (`RenderInvariant` docs) |
| 6 | app paint | per control: `role_font()` builds a `BitmapFont` **on every paint**, text is re-truncated and re-measured, 1–4 `fill_round_rect`s, a temp `Surface` per signal bead | `lib/controls/src/paint.rs` |
| 7 | app present | `client.present(id, 0, DamageRect::full(mode))` — **whole window, always** | `files`, `terminal`, `viewer`, `wallpaper`, `widgets`, `switchboard` `run.rs` |
| 8 | compositor | full-client damage, widened by any blurred window to that window's **entire** bounds | `wm::compositor::widen_blurred_damage` |
| 9 | compositor | the frost is recomputed over the whole window every time, 2 box-blur passes, **4 integer divides per pixel per pass**, **never cached** | `wm::compositor::blur_backdrop`, `lib/raster/src/blur.rs` `blur_line`/`mean` |
| 10 | compositor | every damaged pixel: `WindowRow::sample` → `scale_alpha` → `Pixel::over` (4 × `div255`). **No opaque fast path, no occlusion culling** | `wm::compositor::compose_pixel`, `wm::window::WindowRow::sample` |
| 11 | present | up to `MAX_PRESENT_REGIONS` (8) separate `present_region` round trips, each rotating the 2-frame ring and copying a growing **bounding-box union** of stale damage | `lib/display/src/client.rs` `RemoteDisplay::push`/`copy_region` |

Rows 5, 7, 8 and 10 are the "plummets when the pointer crosses a
control-rich window" symptom. Rows 8+9 are the "slow when blur or
transparency is in use" symptom. Row 11 multiplies both.

Two build-level facts make any measurement taken today meaningless:

- **The debug and QEMU images are built at `opt-level = 1`.** Only
  `tairix-image`, `tairix-raster`, `tairix-svg` and `tairix-fontface`
  are raised to `3` (`Cargo.toml` `[profile.dev.package.*]`), so
  `tairix-wm`, `tairix-controls`, `tairix-font`, `tairix-window`,
  `tairix-display` and `tairix-desktop-session` — the per-pixel closure
  chains that live or die on inlining — run unoptimised.
- **There is no measurement of any kind**: no raster benchmark, no
  frame-time or work-count instrumentation anywhere in the compositor.

---

## Goal / invariants (bind every stage)

1. **One blend definition, one raster path (§2.2).** `Pixel::over` /
   `div255` in `lib/raster` stay the single blend. Stages B and F add
   *specialised loops* over that definition (an opaque run is a copy, a
   vector candidate reproduces the identical rounding) — never a second
   blend, a "fast" approximation, or a forked rasteriser.
2. **Bit-identical output, proven.** Every fast path, cache, and CPU
   candidate produces byte-identical frames to the portable reference for
   the same scene. Each stage lands a golden-frame test that composes the
   scene both ways and asserts equality. A change that alters output
   (half-resolution blur, a different rounding) is a *deliberate,
   documented rendering decision with a visual test*, never a silent
   tweak — and is called out as a decision below.
3. **Tests assert work, not wall-clock (§7, no flaky tests).** CI gates
   on deterministic counters — pixels blended, rects presented, controls
   repainted, IPC round trips, cache hits — which are load-independent. A
   wall-clock threshold in CI is forbidden; timings are evidence for the
   completion report, produced by the Stage A harness, never a pass/fail
   gate.
4. **No security or correctness trade (§2.17, §2.9).** `overflow-checks`
   stays `true` in both profiles; the fix for arithmetic cost is hoisting
   it out of the inner loop, never disabling the check. No `unwrap`/
   `expect`/`panic!` on a frame path. A client-supplied damage rect is
   validated and clipped by the receiver (§5.4) — a smaller present must
   never become a way to smuggle an out-of-bounds rect.
5. **Every cache is bounded, reclaimable, and keyed by an epoch
   (§24.1, §26.3).** The frost cache, the text-measure memo, and any
   run/layer cache are `lib/reclaim` clients with a budget derived from
   discovered memory, invalidated by an explicit epoch (scale, theme,
   backdrop generation), never a fixed `const` retainer and never
   proportional to screen count or window count.
6. **No busy-wait, no periodic tick (§2.23, §17.1).** Frame pacing is a
   **one-shot** timer armed for the next deadline; an idle desktop arms
   nothing and parks. Nothing polls for the next frame.
7. **Platform-neutral (§2.20/§2.21).** Everything in Stages A–E is
   arch-neutral. Stage F's ISA-specific candidates follow the
   `lib/pagezero` shape — a `build.rs`-emitted cfg, never
   `cfg(target_arch)` in source, so `cargo xtask cfg-check` stays green.
8. **Foundational primitives are complete (§27).** The region type
   (Stage C.0) and the control damage sink (Stage C.1) are implemented as
   the whole abstraction — not the slice the first caller happens to use.
9. **No speculative surface (§2.3/§2.4).** No ABI field, capability, or
   public method lands before the change that consumes it. No new
   capability is introduced by this plan; the frame counters ride the
   existing session→Switchboard feed.

---

## Stage A — Measure, and measure the right binary

Nothing else in this plan may be reported as an improvement until this
stage exists: §2.16 requires evidence, and §7 requires the numbers in the
completion report.

### A.1 Build the per-pixel crates at product speed in every profile
Extend the existing `[profile.dev.package.*]` override in the workspace
`Cargo.toml` — whose rationale ("pure, well-tested computation over byte
buffers … nothing a developer steps through") applies verbatim — to
`tairix-wm`, `tairix-controls`, `tairix-font`, `tairix-window` and
`tairix-display`. Overflow checks and debug assertions stay on. Record in
the same comment that the debug/QEMU images build user-space `Run`
binaries in the dev profile (`tools/xtask` `pie_build`), which is why the
override matters for a *userland* crate.

Independently of the override, **every published number is taken from a
`--release`/installer image**; a dev-profile timing is never quoted as
evidence.

### A.2 A host benchmark harness for the raster and composite families
No new external dependency (§2.12): reuse `lib/cpuops`'s existing
`BenchHarness` with a host time source injected through its
`CycleCounter` seam. Add `cargo xtask bench` running, over representative
sizes and alphas: `composite_span`, `Surface::blit`, `fill_round_rect`,
`box_blur`/`frost_region`, `resample`, `PixelOrder` encode, and the WM's
per-row composite. It prints ns/px and ns/frame; it is **not** a CI
pass/fail gate (invariant 3), but it is runnable in CI as a smoke check
that the harness itself works.

### A.3 Frame work counters in the compositor
`Compositor` accumulates a per-frame `FrameStats`: damaged px, blended
px, opaque-run px (Stage B), blur px, encoded px, dirty rects, present
calls, cache hit/miss per cache, and composite/blur/present nanoseconds.
Reset per frame, snapshot-readable.

- Surface it through the **existing** session→Switchboard tray feed
  (`plans/NEW-SWITCHBOARD.md`) — no new syscall, no new sysinfo query,
  no new capability (§5.2, §2.4).
- The single most useful number is **damaged px vs blended px vs screen
  px**: it turns "the desktop feels slow" into "we blended 4.2 M pixels
  to change 3 200".

### A.4 Tests + docs
- Unit: counters are exact for a scripted scene (a known rect count and
  pixel count), zero for an empty-damage frame.
- QEMU desktop vertical: hovering across a control-rich window asserts
  **counter bounds**, not timings (this is the regression gate every
  later stage tightens).
- Docs: `docs/src/desktop/` compositor page gains the counter meanings;
  `plans/NEW-SWITCHBOARD.md` gains the section; the `lib/raster` and
  `userland/gui/wm` READMEs state how to run the harness.

**Acceptance:** baseline numbers for the hover, drag, blur, and idle
scenarios recorded (in the completion report, not in this file — §13),
and the counter assertions green.

---

## Stage B — Stop blending pixels nothing can see

Compositor-local, no ABI change, no app change. Depends on A for
evidence.

### B.1 Opaque-run composite
`Window::row` already resolves the row-constant factors into a
`WindowRow`. Extend it to yield **runs** over a row rather than a
per-column `Option<Pixel>`: a run is either opaque (a contiguous
`&[Pixel]` at full alpha, no rounding coverage, no per-window opacity) or
blended. `recompose_rect`/`compose_span` then:

- copy an opaque run with `copy_from_slice` into the back buffer and
  encode it in one pass;
- send only genuinely translucent runs through the existing
  `compose_pixel` → `Pixel::over`.

This is a loop specialisation, not a second blend (§2.2): the blended
branch calls the same `Pixel::over`.

### B.2 Occlusion culling
`composite` currently collects `hits` by bounds overlap and paints
back-to-front. Walk the z-order **front to back** per dirty rect and stop
at the first window that (a) fully covers the rect, (b) is fully opaque,
(c) has no rounded-corner coverage over the rect, and (d) has no backdrop
blur. Everything below it — windows, the desktop layer, and the root fill
— is dropped for that rect. O(windows) comparisons remove O(pixels ×
windows) of blending.

The blur segmentation in `recompose_rect` (compose-below → frost →
resume) must be preserved exactly: a blurred window is a cull barrier.

### B.3 Run-at-a-time encode
`PixelOrder::encode` runs per pixel inside the composite loop. Add
`encode_run(&[Pixel], &mut [u8])` in `lib/abi/src/driver/display.rs`,
defined once over `encode` for the general case and degenerating to
`copy_from_slice` when the scan-out byte order already matches `Pixel`'s
layout. Replace the per-pixel call site (do not keep both, §2.14).
Regenerate the C header (`cargo xtask c-header --write`) and keep
`cargo xtask abi-check` green.

### B.4 Tests + docs
- **Golden-frame equality**: a scene with opaque, rounded, translucent,
  decorated, and blurred windows composites byte-identically with the
  fast paths on and off (a test-only reference walk).
- Counters: a maximised opaque window over the desktop blends ~0 pixels
  from the layers beneath it; a fully covered window contributes 0.
- Edge cases: zero-width run, run clipped by the furniture gutter,
  window partly off-screen, rect of one pixel, opacity 254 vs 255.
- Docs: `userland/gui/wm/README.md` + the compositor rustdoc state the
  cull conditions and that they are the *only* conditions.

**Acceptance:** identical frames, blended-pixel counter down on the
occlusion and opaque scenarios, `cargo xtask abi-check` green.

---

## Stage C — Repaint the control that changed, not the window

The largest single win, and pure userland. Depends on A.

### C.0 One region type, in one place (§2.2, §27)
`userland/gui/wm/src/damage.rs` holds a WM-private `DamageRegion` over
`tairix_geometry::Rect`; `lib/controls` needs the same concept, and Stage
E needs a better one. Hoist it into **`lib/geometry`** as `Region` (the
crate already owns `Rect`/`Point`/`Scale`; update its module rustdoc,
which currently says it holds no compositing arithmetic — a dirty-rect
set is geometry, not rendering). Delete the WM copy (§2.14).

Complete from the start (§27): `add`, `clear`, `bounds`, `is_empty`,
`iter` (disjoint), `contains`, `intersect`/`clip`, `translate`,
`subtract`, and a **bounded** coalescing policy (a rect budget above
which the region degrades to its bounding box — a growable capacity
policy, not a hand-picked ceiling that a busy frame silently falls off,
§24.1). Row-banded internally so `add` is not an O(n²) rescan.

### C.1 A damage sink in `lib/controls`
Controls today are host-composed with no tree and no dirty concept; the
host's only signal is a `PartialEq` render gate that fails whole-surface.
Add the missing seam: an input/update call takes a damage sink into which
a control pushes **its own bounds** when a render-relevant state field
changes (hover enter/leave, press, focus, selection, validation,
authority, activity, value, content). `RenderInvariant` fields keep
reporting nothing, exactly as they keep failing to trip the render gate.

Complete (§27): every control family in the crate reports, containers
propagate their children's rects, and a control that changes nothing
reports nothing. The host renders only the reported rects and presents
their union.

### C.2 Enter/leave hover routing in containers
`Toolbar`, `Panel`, `Menu`, and the collection families fan every motion
sample to every child. Track the hovered index in the container: on
motion, hit-test once and deliver leave/enter to at most two children.
An armed (dragging/pressed) child keeps receiving the stream regardless
of position — that is the pointer-grab semantic and must not regress.
This is the §27 point-2 pattern: an O(n) linear scan on a load-bearing
path in a foundational primitive.

### C.3 Apps present the rect they changed
Every app presents `DamageRect::full(mode)` today — `files`, `terminal`,
`viewer`, `wallpaper`, `widgets`, `switchboard`. The window ABI already
carries a per-present `DamageRect` (`lib/window` `WindowClient::present`),
so **no ABI change is required**: each app unions its control damage and
presents that rect. Where an app's frame genuinely changed everywhere
(resize, theme change, first paint) it still presents full.

Receiver side: the session/WM already maps client-local damage to screen;
confirm and, if missing, add explicit validation — a rect outside the
client's own surface is clipped or refused, never trusted (§5.4).

### C.4 Hoist the font, memoise the measurement
`role_font()` constructs a `BitmapFont` on every control paint. Resolve
the role→face **once per render pass** in the host and thread it down.
Text measurement (`truncate_to_width`, `elide_to_width`, advance widths)
is recomputed for the same string every paint: add the memo to
**`lib/font`**, beside the glyph-bitmap `ReclaimCache` it already owns
(one home for text caching, §2.2; `plans/FONT-SERVICE.md`), keyed by
(string, face, scale) and reclaim-budgeted (invariant 5).

### C.5 One shell present per drained batch
`DesktopShell::handle` runs `present()` + `sync_active_frame` +
`refresh_cursor` for **every** queued motion sample; `pump` already folds
app-ward outcomes latest-wins. Apply the same discipline to the shell's
own work: `handle` updates state per sample, and `pump` does the
present/active-frame/cursor refresh **once** per drained batch, before
the single `present()` the run loop already performs.

### C.6 Tests + docs
- Damage sink: hover enter/leave reports exactly two rects; motion within
  one control reports none; a press reports one; a container reports the
  union of what its children reported and nothing more.
- Routing: with N children, one motion touches at most two; an armed
  child still receives every sample; the resulting `ControlState` is
  identical to the fan-to-all behaviour (a differential test over a
  scripted pointer path).
- App: a hover flip presents a rect of the control's size, not the
  window's; a resize still presents full.
- Shell: a batch of N motion samples produces one taskbar present and one
  cursor refresh, with the same final state as N individual handles.
- Region: disjointness, subtract/split, budget degradation, translate,
  clip, and property tests against a naive reference model.
- Docs: `plans/GUI-CONTROLS-DESIGN.md` gains the damage contract,
  `lib/controls/README.md` + `lib/geometry` rustdoc updated,
  `docs/src/desktop/` control page updated.

**Acceptance:** the QEMU hover vertical's damaged-pixel counter drops
from window-area to control-area; every existing control and WM test
still passes unchanged.

---

## Stage D — Make blur cost what it changes

Depends on A and B (the segmentation and cull interact with frosting).

### D.1 Split backdrop damage from content damage
`widen_blurred_damage` widens *any* damage touching a blurred window to
that window's whole bounds, because the frost must be recomputed. But the
dominant case — the pointer moving **inside** a translucent window —
changes only the window's own content; the backdrop underneath is
unchanged. Track the two separately: only damage *below* a blurred window
(or a move/resize of it) invalidates its frost; damage to its own content
composites over the retained frost.

### D.2 Cache the frosted backdrop
Keep a per-blurred-window frosted `Surface` in a `lib/reclaim`
`ReclaimCache`, keyed by window id with an epoch of (bounds, radius,
scale, backdrop generation) — the same shape as the existing `chrome`
cache in the same file, which is the in-tree precedent. A cache miss or a
pressure refusal recomputes exactly as today (fail-soft, never a panic,
§2.9). Budget derived from discovered memory, shrinking under pressure
(§26.3) — a 1 GiB machine must not retain a screenful of frost per
window.

### D.3 Cheaper, bit-identical blur arithmetic
`blur_line` does a `.get()`-checked read per sample and **four integer
divides per output pixel per pass** in `mean`. Two changes, both
output-preserving:

- resolve the source row/column slice once per line (edge replicate
  handled at the ends), removing the per-sample bounds check;
- replace the divide-by-`count` with a precomputed fixed-point reciprocal
  multiply — the window size is constant for the whole pass. The magic
  must reproduce the current round-half-up mean **exactly** for every
  reachable (sum, count); prove it by exhaustive test over the count
  range and property test over sums.

### D.4 Tests + docs
- Existing `blur_tests.rs` must pass **unchanged** (that is the
  bit-identity proof for D.3).
- Frost cache: a content-only repaint of a frosted window recomputes no
  frost; a change behind it recomputes exactly once; a move, resize,
  radius, scale, or theme change invalidates; a pressure eviction still
  produces the identical frame.
- Counters: blur px per frame for the hover-inside-a-frosted-window
  scenario goes to zero after the first frame.
- Docs: `lib/raster/src/blur.rs` rustdoc (the reciprocal and why it is
  exact), the compositor rustdoc (the two damage kinds), `plans/SMARTRAM.md`
  gains the frost cache as a reclaim client.

**Acceptance:** identical frames, blur recomputation only on genuine
backdrop change, blur ns/px measurably lower on the Stage A harness.

### D.5 Decision (not silently taken)
Blurring at half resolution and upsampling is ~4× less area but
**changes the output**. It is therefore a rendering decision for the
User, with a visual comparison, not an optimisation to slip in. Left out
of D unless approved.

---

## Stage E — One present per frame, and a frame deadline

Depends on B and D. Touches the display wire protocol, so it must be one
evolution with `plans/FIX-DISPLAY-ACCELERATION.md` Stage B, not a second
shape (§2.2, §2.13).

### E.1 Keep the damage region disjoint
`DamageRegion::add` coalesces overlapping rects to their **union**, and
more than `MAX_PRESENT_REGIONS` rects collapse to one bounding box — two
small rects at opposite corners become the whole screen. Use the Stage
C.0 disjoint `Region` (subtract-and-split, row-banded) with its bounded
budget, so a scattered frame stays scattered.

### E.2 One present per frame, carrying a list of rects
`RemoteDisplay::push` rotates the 2-frame ring **per `present_region`
call** and refreshes `union(stale, damage)` as a **bounding rect**: with
8 rects that is 8 IPC round trips, each copying a growing box — in the
worst case ~8 near-full-screen copies for a frame that changed a few
thousand pixels. Change the `Present` request in place to carry a
bounded **list** of rects (count bounded by a discovered/negotiated
limit, not a magic const, §24.1); the ring rotates once per frame and the
per-frame stale set is tracked per rect rather than as one box. Align the
request shape with `PresentLayers` so the accelerated path reuses it.

### E.3 One-shot frame pacing in the session
There is no pacing today: the session composites once per wake, as fast
as input arrives, with no vsync and no cap. Arm a **one-shot** timer for
the next frame deadline (tickless, §17.1 — never a periodic tick), let
damage accumulate between deadlines, and composite once per deadline. An
idle desktop arms nothing and parks (§2.23). This bounds worst-case work
under an input flood and is the seam
`plans/FIX-DISPLAY-ACCELERATION.md` Stage E hangs real vsync off.

### E.4 Tests + docs
- Region: two far-apart small rects present two small rects, never the
  screen; a pathological scatter degrades to the documented budget, not
  unbounded round trips.
- Present: N dirty rects produce **one** transport call and one ring
  rotation; stale tracking still guarantees no stale pixel is shown (the
  existing double-buffer tests must pass unchanged).
- Pacing: a flood of M motion samples inside one deadline produces one
  composite; an idle session arms no timer and consumes no CPU; the
  deadline never busy-waits (assert on the wait call, not on timing).
- Docs: `docs/src/drivers/display.md` protocol table; `lib/display`
  rustdoc; `plans/FIX-DISPLAY-ACCELERATION.md` cross-reference (§13).

**Acceptance:** present calls per frame == 1, no stale-pixel regression,
CPU at idle unchanged from parked.

---

## Stage F — CPU-dispatched raster kernels (`lib/cpuops`)

This is the honest answer to "can CPU feature detection help?": yes, and
it is the *last* 20%. It may not land before B–C (invariant: do not
vectorise a loop that should not run).

### F.0 Prerequisite — correct the `ByBenchmark` axis (decision, §15.7)
`plans/FIX-HARDWARE-FEATURES.md` P3b lists `lib/raster` blit/blend/fill
under `ByBenchmark` and marks it **blocked**, because the bounded
microbenchmark measures over the kernel-only `CpuCycles` counter and
raster is userland. That classification is wrong for the same reason the
plan already corrected page-zero in P3a: a packed-SIMD premultiplied
`over` is *unconditionally* faster than four scalar `div255`s and is
bit-identical when the vector form implements the same rounding — so it
is a **capability** decision (`ByPriority`), never a performance
measurement.

The capability axis is already wired to userland: `lib/rt`'s startup
delivers the kernel-folded common `CpuFeatureSet` (`cpu_features()`), and
`lib/cpuops` is a plain `lib/*` crate with no kernel edge. So
`ByPriority` selection in `lib/raster` works **today** — zero new kernel
mechanism, zero ABI change, with the existing self-verify + fail-closed
baseline + pin + audit machinery.

**Amend P3b in place (§2.13)** to move the raster families onto
`ByPriority`, mirroring the P3a correction. Confirm with the User before
editing that plan.

### F.1 Make the loops vectorisable before reaching for intrinsics
NEON is baseline on `aarch64-unknown-none`, so a large part of the win
needs no dispatch at all: operate on `chunks_exact` of packed pixels
instead of a per-pixel `Option`-returning sample, hoist the row-constant
factors, and let LLVM vectorise. Measure this step on its own (Stage A
harness) before adding candidates — it may be most of the win.

### F.2 Candidates, following `lib/pagezero` exactly
Same shape, because it has already passed review once: a `build.rs`-
emitted per-ISA cfg (never `cfg(target_arch)` in source, so
`cargo xtask cfg-check` stays green), a portable baseline registered
**last** that is always feature-legal, the mandatory self-verify against
that baseline over a fixed size/alignment/alpha vector, `ByPriority`
selection, host fuzzing, and the pin for determinism.

### F.3 Families, in order
1. `composite_span` — one source over a span.
2. `Surface::blit` — src-over-dst row zip.
3. the WM's opaque/blended run loop (Stage B.1).
4. `blur_line` add/sub/mean — after the D.3 reciprocal.
5. `encode_run` — a byte-order shuffle (Stage B.3).
6. `resample` `filter_row`/`write_row` — icon and wallpaper scaling.

All are secret-free and bit-identical, so all are legal on the capability
axis (`plans/FIX-HARDWARE-FEATURES.md` invariant 8). None of them may be
benchmark-selected.

### F.4 What is actually available per target

| Target | User-space vector state | Verdict |
|---|---|---|
| `aarch64` | full `q0`–`q31` + `FPCR`/`FPSR` saved on user trap entry/exit; `d8`–`d15` in the kernel switch | **Green today.** NEON candidates are a pure userland change. |
| `x86_64` | none — no `fxsave`/`xsave` in `kernel/`; the target is a soft-float, SSE-disabled kernel target and is reused for user PIE bundles | **Blocked on Stage G.** |
| `riscv64` | none found — no `fsd`/`fld` in `trap.s`/`context.s`, no `mstatus.FS` handling | **Blocked on Stage G**, and see G.0 — this may be a latent defect. |
| `wasm32` | `simd128` not in the baseline | Baseline only. |

### F.5 Tests + docs
- Self-verify vectors per family (sizes, alignments, alpha extremes,
  overlapping/short spans).
- Differential fuzz: candidate vs baseline over random buffers
  (`cargo xtask fuzz`), added to the regression corpus (§19.6).
- The pin makes CI deterministic; the audit records the selection.
- Docs: `lib/raster/README.md` (families and their gates),
  `plans/FIX-HARDWARE-FEATURES.md` P3b corrected, README support matrix.

**Acceptance:** bit-identical output on every candidate, baseline chosen
when features are masked off, measured improvement quoted from the Stage
A harness.

---

## Stage G — User-space vector/float enablement (kernel work; User decision)

Not started, and not startable without a decision (§15.7). Recorded here
so it is not lost, with the two findings that motivate it.

### G.0 Two findings, one confirmed and one to confirm
- **x86_64 user space has no FPU/SSE.** `x86_64-unknown-none` is the
  *kernel's* soft-float, SSE-disabled target and it is also used to build
  user-space PIE bundles. Userland is not the kernel: it should have SSE2
  and hardware float. Enabling it needs `fxsave`/`xrstor` (or `xsave`) in
  the x86_64 trap/switch path plus `CR0`/`CR4` setup, then a user-space
  target feature set. Real kernel work, and a decision — not something to
  slip into a GUI change.
- **riscv64 appears to save no float state at all** — no `fsd`/`fld` in
  `kernel/arch/riscv64/src/trap.s` or `context.s`, and no `mstatus.FS`
  handling found — while `riscv64gc` mandates the D extension and
  `lib/raster`'s gradient path uses `f64`. Either FP traps, or two tasks
  corrupt each other's float registers. **This is a defect noticed by
  reading the code (§2.18) and must be confirmed and then fixed or
  explicitly ruled out, regardless of any GUI work**; it is tracked in
  `PLAN.md` and carries a regression test when the fix lands (§7).

### G.1 If approved
Per-port lazy-or-eager FP/vector context save/restore behind the Arch HAL
context-switch slice (§17.2), the user-space target feature floor raised
in `tools/xtask`'s per-image floor (`plans/FIX-HARDWARE-FEATURES.md` P0),
Arch-HAL conformance coverage proving two tasks cannot observe each
other's FP state, and only then the Stage F SSE2/AVX2 candidates.
Cross-referenced from `plans/WIRING.md` and `plans/ARCHSUPPORT.md`.

---

## What this plan refuses

Stated so a later change cannot quietly take a shortcut:

- **No SIMD before the algorithm.** Stage F may not land before B and C.
- **No performance claim without a number** from Stage A, and never a
  number taken from a dev-profile image.
- **No second blend, raster, or region implementation** "for speed"
  (§2.2). One path, specialised loops.
- **No raising a constant instead of fixing the algorithm** (§2.17) — a
  bigger cache, more frame buffers, or a larger present limit is not a
  fix.
- **No disabling `overflow-checks`** (§2.9, §2.17). Hoist the arithmetic.
- **No output change without a decision** (invariant 2): approximate
  blends, half-resolution blur, or altered rounding are User decisions
  with visual evidence.
- **No wall-clock threshold as a CI gate** (§7): counters only.
- **No unbounded cache** (§24.1, §26.3): every cache is a reclaim client.

---

## Stage dependencies

| Stage | Content | Depends on | Touches ABI? | Touches kernel? |
|---|---|---|---|---|
| A | build profile, bench harness, frame counters | — | no | no |
| B | opaque runs, occlusion cull, `encode_run` | A | `PixelOrder::encode_run` only | no |
| C | region hoist, control damage, hover routing, font hoist, batch shell work | A | no | no |
| D | frost cache, backdrop/content damage split, blur reciprocal | A, B | no | no |
| E | disjoint region, one present per frame, one-shot pacing | B, C, D | `Present` rect list (with FIX-DISPLAY-ACCELERATION Stage B) | no |
| F | `lib/cpuops` `ByPriority` raster candidates (aarch64 first) | B, C, (D, E), F.0 decision | no | no |
| G | user-space FP/SSE enablement | User decision | target floor | yes |

A–E are expected to dominate F entirely.

---

## Decisions required (§15.7)

1. **Amend `plans/FIX-HARDWARE-FEATURES.md` P3b** to move the raster
   families from the blocked `ByBenchmark` axis to the unblocked
   `ByPriority` capability axis (Stage F.0). Blocks Stage F.
2. **Half-resolution blur** (Stage D.5): approve or refuse the output
   change. Blocks nothing; D lands without it.
3. **Stage G**: whether to do the x86_64 user-space FPU/SSE kernel work
   at all, and when. Blocks Stage F on x86_64 and riscv64 only.
4. **The riscv64 float-state finding** (G.0) is a suspected defect that
   must be confirmed and fixed independently of this plan's schedule; it
   is not a GUI decision, it is a correctness one.

---

## Definition of done (whole plan)

- Every stage's code, tests, and docs land together (§7, §13); no stage
  ships a stub, a no-op, or a "later" (§2.19).
- Output is byte-identical to the pre-change reference for every scene
  (invariant 2), proven by the golden-frame tests, except where a User
  decision above explicitly approved a rendering change.
- The QEMU desktop verticals assert work counters, not timings, and the
  counters for hover, drag, blur, and idle are at or below the bounds
  each stage set.
- Every cache added is a bounded `lib/reclaim` client with an epoch and a
  pressure path (§24.1, §26.3).
- §23 self-review applied: security (client damage rects validated and
  clipped, no ambient authority, no new capability), correctness and
  multi-arch (no `cfg(target_arch)` outside the allow-list, no arch-only
  copy of shared logic), no-compat/no-dead-code (the WM's private
  `DamageRegion` and the per-pixel encode are **deleted**, not left
  beside their replacements), tests/docs.
- Whole-project gate green: `cargo fmt --all`, `cargo xtask ci` (once),
  `cargo xtask fuzz --secs 5` (the new run/encode/region/candidate
  decoders get harnesses, §19.6), and `tools/ci/soak.sh both --secs 20`.
