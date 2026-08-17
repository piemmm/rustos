# FIX-DESKTOP-SPEEDUP — Software-compositor and GUI redraw performance

Status: **A done, B done, C mostly done, D done** (B.5 dithers every blended
pixel and B.6 composes a segment a layer at a time; C.0, C.1, C.2, C.4b, C.4c,
C.5 landed — every control family reports what it repaints from both input
paths, and a host now reports its own two kinds of change through the same two
guarded writes; C.3 has landed for `userland/apps/widgets` with its differential
proof and five apps remain; C.4a withdrawn as a performance item after
measurement; D.1–D.4 and D.7–D.9 landed — a frosted window that moves now keeps
the frost the move cannot reach, the layers a frost covers are no longer
composed, and *any* window that reads its backdrop retains one, so a translucent
drag went from the desktop's slowest to cheaper than a blurred one — D.5 is a
User decision and D.6 a follow-up the measurements exposed).
E planned.
Stages A–E need no hardware acceleration, no kernel change, and no new
syscall; F needs a `plans/FIX-HARDWARE-FEATURES.md` correction; G is gated on
a User decision (§15.7) because it is kernel work.

Still open in A: the QEMU hover vertical (A.4's counter-bounds gate). The
counters, their exact-count unit tests, the build-profile fix, the bench
harness and the Switchboard surfacing are in.

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
| 6 | app paint | per control: text is re-truncated and re-measured, 1–4 `fill_round_rect`s, a temp `Surface` per signal bead. (`role_font()` building a `BitmapFont` per paint was listed here and is **not** a cost — see C.4a) | `lib/controls/src/paint.rs` |
| 7 | app present | `client.present(id, 0, DamageRect::full(mode))` — **whole window, always**, after an unpremultiply-and-copy of every pixel into the shared frame | `files`, `terminal`, `viewer`, `wallpaper`, `widgets`, `switchboard` `run.rs` |
| 8 | session | converts and **diffs every declared pixel** against the window's surface — a whole-window pass per sample | `session::windows::convert_damage` |
| 9 | compositor | the frost is recomputed over the whole window every time, 2 box-blur passes, **4 integer divides per pixel per pass**, **never cached** | `wm::compositor::blur_backdrop`, `lib/raster/src/blur.rs` `blur_line`/`mean` |
| 10 | compositor | every damaged pixel: `WindowRow::sample` → `scale_alpha` → `Pixel::over` (4 × `div255`). **No opaque fast path, no occlusion culling** | the per-column walk since replaced by `wm::compositor::compose_segment` |
| 11 | present | up to `MAX_PRESENT_REGIONS` (8) separate `present_region` round trips, each rotating the 2-frame ring and copying a growing **bounding-box union** of stale damage | `lib/display/src/client.rs` `RemoteDisplay::push`/`copy_region` |

Rows 5–10 are the "plummets when the pointer crosses a control-rich window"
symptom. Rows 9+10 are the "slow when blur or transparency is in use"
symptom. Row 11 multiplies both.

**Where that cost actually falls, measured rather than assumed.** The
compositor is *not* the bottleneck for an app hover: `convert_damage`
(row 8) compares each presented pixel with the one already in the window's
surface and returns only the sub-rectangle that genuinely changed, so a
full-window `DamageRect` already reaches the compositor as the few rows a
highlight moved. What a single pointer sample really costs is **three
whole-window passes above it** — the app's re-render (row 5–6), the app's
unpremultiply-and-copy into the shared frame (row 7), and the session's
convert-and-diff (row 8) — none of which knows what changed. That is what
Stages C.1 and C.3 remove, and it is why C.3's win is in the app and the
session rather than in the composite.

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
   tweak — and is called out as a decision below. Exactly one such change
   has been made: B.5's per-pixel dither on a *blended* pixel, whose bound
   (mean unchanged, never more than one level) is stated there; a copied
   or opaque pixel is still byte-identical.
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

## Stage A — Measure, and measure the right binary  **[done, less A.3's Switchboard surfacing and A.4's QEMU vertical]**

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
`BitmapFont::draw_text`/`text_width` over a label and a row,
`box_blur`/`frost_region`, `resample`, `ChannelOrder` encode, and the WM's
per-row composite. The text family draws through the production entry point
with a glyph cache installed and warm — a figure taken without one would
describe the mock service's reply encoding instead. It prints ns/px and
ns/frame; it is **not** a CI pass/fail gate (invariant 3), but it is runnable
in CI as a smoke check that the harness itself works.

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
- **A monitor must not measure its own act of displaying.** The session
  suppresses a `FrameReport` when the only served content since the last
  decision came from the live Switchboard's own window(s). Without that
  gate, the panel rebuild from a report is itself a frame whose counters
  differ, which sends another report, forever — a self-exciting push loop.
  Rate-limiting or quantising the counters is not a fix; the content gate
  is. Real desktop work and chrome/idle settles still report live.

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

## Stage B — Stop blending pixels nothing can see  **[done]**

Compositor-local, no ABI change, no app change. Depends on A for
evidence.

### B.1 Opaque-run composite
`WindowRow::opaque_run` yields the longest run of source pixels from a
column that each replace what is beneath them exactly; `compose_row` (the
row loop, lifted out of `compose_span`) copies such a run into the back
buffer with `copy_from_slice`, encodes it with one `encode_run`, and takes
every other column through the unchanged blend path (now `compose_segment`).

A loop specialisation, not a second blend: the blended branch calls the
same `Pixel::over`, and *over* with a fully opaque source **is** the
source, which is why the copy is exact.

### B.2 Occlusion culling — the *same* mechanism as B.1, not a second one
Culling is decided **per run, inside the row loop**, not per window before
it. `compose_row` asks the segment's front-most window for its opaque runs
first; a run that copies has skipped every layer below it — the windows
beneath, the desktop layer and the root fill — for exactly those columns.

This is strictly better than a per-rect window walk and is why there is no
separate cull pass:

- **Sound without trusting a client.** "Fully opaque" is read from the
  source pixels themselves (alpha 255, full window opacity, no rounding
  coverage on the row), so a window whose *content* is translucent — a
  frosted terminal — can never cull what shows through it. A window-level
  `opacity == 255` test would have been wrong, and tracking a per-window
  "content is opaque" fact would have cost a scan on every present.
- **Finer.** A window covering part of a dirty rect, or opaque only in
  places, still saves exactly the blending it can.
- **One condition set, stated once** in `compose_row`'s rustdoc and
  `WindowRow::opaque_run`.

The blur segmentation in `recompose_rect` (compose-below → frost →
resume) is untouched, and a blurred window remains a cull barrier: runs are
sought only within a segment, so nothing a frost reads is ever skipped.
A fade in flight (`set_reveal`) and the rows the cursor draws on take the
general path, because both change the bytes a copy would have written.

### B.3 Run-at-a-time encode
The per-pixel scan-out encode is `ChannelOrder::encode` in
`lib/display/src/scanout.rs` — **not** `lib/abi`, which cannot name a pixel
type without closing the cycle `abi → raster → theme/reclaim → abi`. Add
`encode_run(self, &[Pixel], &mut [u8]) -> usize` beside it, defined once
over `encode` and returning the whole pixels written, so a short `out`
truncates instead of panicking and a partial trailing group is never
written. There is **no** bulk-`memcpy` case: `Pixel` carries no layout
guarantee to copy through, and a matching byte order is already a four-byte
move per pixel. `encode` stays — it is the definition, and the general
column path encodes one pixel at a time as it computes it. No C header and
no `abi-check` involvement: this is not ABI surface.

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
occlusion and opaque scenarios.

### B.5 Blended pixels are dithered  **[done]**

A blend into the 8-bit back buffer admits only `256 - a` of the 256 levels the
picture beneath it held, so one fixed rounding stepped a smooth wallpaper into
plateaus under a translucent window or a frosted panel — invisible on a small
QEMU screen, unmistakable on a 1080-row Pi display. Every blended pixel now
rounds at its own bias from `tairix_raster::DitherRow`, resolved once per
screen row in `compose_span` and indexed by the screen column;
`WindowRow::sample` scales opacity and corner coverage at the same bias, and
`Surface::frost_region`'s mix-back does the same.

This is a **deliberate rendering change** under invariant 2, and the bound it
keeps is stated rather than assumed: the dither's tile mean is exactly
`ROUND_NEAREST`, so nothing lightens or darkens, and no pixel moves more than
one level from the undithered answer. It is not a second blend (invariant 1):
`div255` *is* `(value + 127) / 255`, so the biased divide is the same
arithmetic with its rounding point named, and every unbiased operator
delegates to it — which is why all 251 pre-existing WM frame tests stayed
byte-identical.

Cost is confined to pixels that were already blending: one mask and one load.
The B.1 opaque run is untouched — it copies — so the fast path pays nothing,
and the counters are unchanged.

The hardware path keeps the same guarantee by *not* taking work it cannot do:
an engine blends a layer in the scan-out's own 8 bits with a fixed rounding
and no layer stack can express a per-pixel dither, so
`Compositor::has_translucent_window` sends a window-wide translucency through
software exactly as a backdrop blur does, and a baked layer
(`Window::sample_local`) reads the dither at the pixel's *screen* position so
it holds what the software composite would have written
(`plans/FIX-DISPLAY-ACCELERATION.md` A.3).

### B.6 A segment is composed a layer at a time, not a pixel at a time  **[done]**
B.1's opaque runs are copied; everything else went through a per-*column* walk
that, for each pixel, asked the desktop layer, then every window row, then the
cursor, what it contributed. Decomposition on the bench (each stage disabled in
turn, same binary) put **1.7 ns/px** in the row loop, write and encode, and
**18.2 ns/px** inside that walk at ~2.4 layer contributions a pixel — so the
colour arithmetic was under half of a blended pixel and the dispatch around it
the rest. (Cross-crate call overhead was tested and ruled out: marking every
`lib/raster` colour primitive `#[inline]` moved the drag by under 1%.)

The columns between two copyable runs are now one **segment**, composed a layer
at a time across its whole width: the base fill, the desktop row, each window
row back to front, then the cursor. Each is a straight run of source pixels at a
screen column and a constant opacity, laid through `lib/raster`'s one span
composite, `blend_span` — which `Surface::blit` also takes, so a blended pixel
is the same arithmetic wherever it comes from and there is no second blend loop
(§2.2). `compose_pixel` is deleted, not left beside it (§2.14).

What keeps it exact: a window row is three straight runs (two furniture strips
and the client's drawable pixels) *except* where the shape cuts it, and there
`WindowRow::blend_into` keeps the column-by-column walk, as does the cursor. The
dither is read at each pixel's own surface column, so a run split anywhere
writes what the whole run wrote and a segment boundary that moves with a window
cannot seam.

Measured: full-screen opaque **2.98 → 0.61 ns/px**, 64×24 opaque **2.54 → 0.96**,
drag opaque **1.24 → 0.47**, full-screen translucent **10.04 → 5.99**, 64×24
blur **9.74 → 6.30**. All 256 existing compositor frame tests stayed
byte-identical, which is the correctness argument; `color_tests` adds the
run-versus-per-pixel differential and the split-at-every-boundary equality.

---

## Stage C — Repaint the control that changed, not the window  **[C.0, C.1, C.2, C.4b, C.4c, C.5 done; C.3 landed for `widgets`, five apps remain; C.4a withdrawn]**

The largest single win, and pure userland. Depends on A.

### C.0 One region type, in one place (§2.2, §27)  **[done]**
`tairix_geometry::Region` (`lib/geometry/src/region.rs`) is the one region
type; the WM-private `DamageRegion` is deleted. It holds a set of pixels as
pairwise-**disjoint**, band-ordered rectangles in a canonical form, so equal
sets compare equal, no pixel is ever composited or presented twice, and two
far-apart updates stay two small rectangles instead of collapsing into the
box between them.

Surface: `new`, `with_budget`, `budget`, `is_empty`, `rects` (the disjoint
iteration), `bounds`, `clear`, `add`, `subtract`, `clip`,
`translate`, `contains`, `intersects`, `From<Rect>`. `add`/`subtract`/`clip`
are one linear band-stripe merge walk over a shared `combine`, not an O(n²)
rescan, and the walk's two buffers are reused so a frame's edits allocate
once. `translate` collapses to the clamped bounding box rather than wrap or
drop a rectangle when a coordinate would leave the `i32` range — over-cover
is safe, silent loss is not. `with_budget` degrades to the bounding box past
its rectangle count; `new` stays exact and grows. A `contains_rect` and a
by-value `clipped` are deliberately **absent**: no consumer needs either, and
a region method without a caller is the speculative surface invariant 9
forbids.

The compositor consumes it through a **compose plan** rather than the old
damage-widening: any damage touching a backdrop-blurred window promotes that
window's whole screen-clipped rectangle into one plan rectangle (overlapping
blurred windows merge, because each reads what the other wrote) and
*subtracts* it from the disjoint residual. The frost still sees a whole
rectangle, so it cannot seam, while damage elsewhere stays exactly as tight
as it was marked — strictly better than widening it to a union box.

Proof: a differential sweep applies random `add`/`subtract`/`clip` to the
region and to a plain pixel grid, comparing the covered set and the
canonical-form invariants after every step.

### C.1 A damage sink in `lib/controls`  **[done]**
Controls had no dirty concept; the host's only signal was a `PartialEq`
render gate that fails whole-surface. `lib/controls/src/damage.rs` is the
seam: `sink()` hands out a `Region::with_budget(8)` and two guarded writes
decide when a change is worth reporting, so no family invents its own rule —
`damage::set(field, value, bounds, damage)` for one drawn field, and
`damage::move_mark` for an index-valued mark a container draws on one child at
a time. `RenderInvariant` fields report nothing, exactly as they fail to trip
the render gate.

The budget is 8 because a host pays twice per reported rectangle — once to
re-render clipped to it, once to present it — and the compositor already
refuses more than eight present round trips per frame; a ninth rectangle
could never buy a separate present. One routed pointer event produces at
most four (the child left, the child entered, a child holding a press, the
container's chrome), so an interactive frame stays exact while a
whole-model refresh degrades to the one box it may as well have been.

`paint::pointer_activation` is where most of the defect lived: it writes
only `state.pointer`, so it computes the next pointer state once and reports
through `damage::set` — which gives correct hover-enter/leave and press
damage to every clickable family at once, and reports *nothing* for motion
inside one control. Containers pass each child its own rect, so a container's
report is exactly the union of what its children reported.

The window furniture is complete, and it is the worked example for the rest:
`WindowControl::rest` guards both fields it clears (the highlight *and* the
focus ring), `ResizeGrabber` guards its pointer look *and* its `dragging`
flag — which is drawn in the teeth, so an Escape-cancel away from the corner
reports even though the pointer look was already at rest — and
`TitleBar::on_key` takes the `Scale`/`Theme` its caller already holds so it
can lay its controls out and hand each its own rect, exactly as
`TitleBar::on_pointer` does.

**The `old ∪ new` question is answered: no per-control `last` rect.**
`TitleBar::move_focus` is precisely that case — the ring leaves one control and
arrives at another — and it names both rectangles by laying its children out,
not by remembering one. That generalises: a container owns its children's
geometry, so given a scale and theme it can always name both, and the fix for a
container that reports its own bounds today (`Rail::set_focus`,
`Toolbar::set_focus`, and the Switchboard gap below) is to thread the layout in,
not to add a remembered rectangle. A control's *own* bounds moving is a host
layout decision, and the host that moved it is the only party that knows both
rectangles — it reports them (C.3: a resize, theme change, or first paint still
presents full). A `last` field per control would be a second, staler copy of
the host's layout with no render path reading it (§2.3).

**The keyboard and value families are in.** `Slider`, `ScrollBar`, `Tabs`,
`Menu`, `ComboBox`, `Breadcrumb`, `TableHeader` and both text fields now report
from every input path, so a keystroke or a value drag costs what it changed:

- A second guarded write, `damage::move_mark`, is what keeps the five
  index-marked families from each inventing a rule. A container draws such a
  mark on one child at a time, so the two rectangles that change are the child
  it leaves and the child it arrives on — the menu row, the tab, the crumb's
  *cell*, the header column — never the strip or popup around them. It hands the
  write back to its caller because the child rectangles are resolved from the
  very container the mark is a field of.
- The breadcrumb is the case that proves the rule needs the container: an
  ancestor elided out of individual view is drawn on the *ellipsis*, so its ring
  is reported there. `cell_shows` is now the one definition of "which cell shows
  crumb *i*", read by both the render path and the report.
- `ScrollBar` reports its whole bar, deliberately: its awake look (`dragging`,
  `held`, hover, focus) is the whole bar, not the part under the pointer. Its
  press and auto-repeat now share one `step_for` mapping, so a held button
  cannot repeat a step other than the one it started with.
- The text fields never compare their buffer to decide: the edit answers for
  the text and the caret/anchor pair answers for the rest, so a secret field's
  characters are not copied even into a temporary a comparison would drop.
- A `ComboBox` reports its field and its popup separately, because they change
  for different reasons — the popup on appearing or vacating, the field only
  when the label it shows changes.

**Where a report is the host's, and how a host makes one.** A control reports
every drawn change it makes itself; two kinds of change are the *host's*, because
only the host knows where it put the controls. Both go through the very same two
guarded writes, which are therefore **public**: a host guards its own field with
`damage::set` / `damage::move_mark` rather than hand-rolling a comparison beside
every setter (the defect the greeter had).

- **A value the host commits back into a control.** The Reactive Alloy rule is
  that a control never mutates its own committed value: it reports an action and
  the owner writes it in (`Toggle::set_on`, `Slider::set_value`,
  `Radio::set_selected`, …). That write is an unreported drawn change at ~40
  sites across the tree, and it — not the three container setters — was the real
  blocker. The owner holds that control's rectangle at exactly that moment and
  the value is drawn inside it, so the owner reports it; nothing narrower is
  available and nothing wider is needed. Growing `(bounds, damage)` onto every
  value setter instead would have changed ~38 signatures and needed ~23
  non-reporting siblings, for no more precision.
- **A mark of the host's own that moves between two controls.** Keyboard focus is
  the one every host has, and `set_focused` on 15 families with 63 call sites
  reports nothing. It does not need to: each control's ring is a function of the
  host's own focus field, so `damage::move_mark` over that field names the control
  the ring left and the control it arrives on, and the flags are then written
  unconditionally — if the field did not move, no ring moved either. A focus that
  lands on the host's own chrome maps to `None` and the chrome reports itself.

The container-mark setters are **closed**: `Tabs::set_current` / `set_selected`,
`Menu::set_current` and `TableHeader::set_sort` take the layout the host already
renders and hit-tests with, and report the children whose plates change — with
`Tabs::adopt_current` / `adopt_selected`, `Menu::adopt_current` and
`TableHeader::adopt_sort` for a rebuild, each sharing its sibling's admission
rule. Two shapes fell out of doing them:

- `set_selected` sweeps *every* tab and reports each whose selection actually
  changed, rather than the two it assumes moved: the owner sets each tab's
  initial selection, so nothing here may assume only one was ever lit.
- `move_mark` is generic over the mark, because a sort carries its direction:
  re-sorting the column already sorted turns its caret over, and comparing
  column indices alone would have reported nothing.

`ComboBox` adopts internally instead of reporting: every path that moves its
menu's highlight while the popup is on screen already reports that whole popup
appearing or vacating, and a host committing a choice has no popup laid out.

The Switchboard gap is closed rather than papered over, and closing it settled a
question the plan had left open. Its keyboard path now threads the same layout
its pointer path uses, so `Switchboard::on_key` and its focus callers — including
the `ActionRail` sites that passed `Rect::EMPTY` — report real rectangles.

Two dead ends are worth recording, because both look reasonable from a call site:

- **A fabricated layout.** A first attempt handed the layout-less sites an inert
  `(Rect::EMPTY, Scale::ONE, Theme::dark())`. It compiled and reported nothing
  (each family resolves a child rectangle from the bounds, so the theme beside it
  could not matter *today*) — and is exactly one read away from being silently
  wrong. Rejected.
- **Plumbing a live theme to the panel.** `apply_focus_marks` is also reached
  from `Switchboard::new` and from a model refresh, where no window layout
  exists: the panel's `RenderInputs` seam deliberately carries a `theme_id`, not
  a `&Theme`, because it is a comparable snapshot. Threading a live theme through
  that seam would have touched the service contract and ~150 call sites to report
  rectangles **nothing consumes** — a construction or a rebuild presents the
  panel whole by design.

The answer is that a rebuild has nothing to report and should *say so*:
`Breadcrumb::adopt_focus` / `TableHeader::adopt_focus` adopt the mark without a
layout and without reporting, sharing the one admission rule with their reporting
sibling so a rebuild cannot admit a focus the interactive path would refuse.
`Switchboard::new` therefore keeps its signature and the initial ring still
appears exactly when it did, so composition equality — and the repaint decision
resting on it — is untouched.

### C.2 Enter/leave hover routing in containers  **[done]**
`Toolbar`, `Panel`, `Rail`, `Decision` and the collection families now track
the hovered and armed child and route through the shared `route_pointer` /
`grab_after` policy in `lib/controls/src/paint.rs` — one hit test per event,
then delivery to at most the child left, the child entered, and any child
holding a press. The grab is deliberately *wider* than the child's own latch,
because a container cannot see whether a disabled or denied child caught the
press; over-grabbing only routes further events to a child that ignores them,
which is what fan-to-all already did. A `#[cfg(test)] fan_pointer` oracle
keeps the old delivery as the differential reference.

### C.3 Apps present the rect they changed  **[`widgets` landed; five apps remain]**

No ABI change is required: the window ABI already carries a per-present
`DamageRect` (`lib/window` `WindowClient::present`). The decision an app makes
with it is shared, not per-app — `tairix_window::present_damage` over
`Repaint::{Nothing, Reported, Whole}`, with `damage_in` clipping a reported
client-space rectangle onto the window (the app's own fail-closed step, since the
session refuses one outside the surface).

**`userland/apps/widgets` is done and is the recipe.** Three whole-window passes
per pointer sample became one control-sized one:

1. **Retain the surface.** `present_frame` allocated *and zeroed* a
   window-sized `Surface` every present — a whole-window pass in its own right,
   ~2 MB for the gallery's window — and then drew into all of it. The surface now
   lives for the life of the window. Every app in the list below does the same
   thing per frame and needs the same fix.
2. **Clip the draw to the damage**, which is sound *because* the surface is
   retained: every pixel outside the clip is the one already on screen.
   `Surface::with_clip` confines writes centrally (every primitive reaches pixels
   through `row_span_mut`), so no control needs changing.
3. **Convert and present only that rectangle**, which is where the app's
   unpremultiply-and-copy and the session's convert-and-diff both stop being
   whole-window.

A round that changed the view but reported nothing presents the **whole window**
— not nothing. Over-covering costs pixels; under-covering leaves a stale frame,
because the session copies only what a present declares. That is the safety net,
and it is reachable: a focus step that finds nowhere to move reports nothing yet
answers "changed".

**The proof, and what every remaining app must land with it.** The safety net is
not a licence to under-report, so the invariant is *tested*, not audited: a host
test renders the app before and after every event of a scripted walk (all nine
panels; hover, press and release on every widget; the whole focus ring actuated
from the keyboard) and asserts every pixel that changed lies inside what that
round reported. Two further tests hold the *tight* direction, or the whole thing
would pass by presenting everything: a hover reports exactly the widget entered,
a second sample inside it reports nothing, and a tab switch reports the content
band it redraws. Fixing three real defects in the gallery was part of getting
there — a tab switch redrew a different panel while reporting nothing, a radio
group's cleared siblings reported nothing, and after switching tabs the focus
marks were resolved against the *previous* panel's rectangles.

Still to do, in this order per app: `viewer`, `wallpaper`, `terminal`, `files`,
`switchboard`. Each needs its host-owned reports (a committed value, a focus mark
— see C.1), its surface retained, and its own differential test; a model refresh
that is not a control round (a clock tick, an animation, new service data, a
resize) keeps presenting whole, which is correct and needs no report. The
terminal's settings sheet is the one part already threaded (its strip reports on
both paths); its radios, sliders and buttons still need the `move_mark` focus
report.

Receiver side is already fail-closed and needs no change: the session's
`window_presented` refuses a `DamageRect` outside the client's surface, or a
frame shorter than the damage needs, with `Errno::OutOfRange`, and the
compositor's `present_window_content` intersects the translated rectangle
with the window's own client rectangle, so an over-large or negative one is
clipped and can never reach a neighbouring window.

### C.4 Draw and measure text once  **[C.4b, C.4c done; C.4a withdrawn as a performance item]**

**C.4a — withdrawn as a performance item; a §2.3 tidy at most.** The premise
above (row 6) was wrong, and measuring it is what showed that:
`BitmapFont::for_role` reads the theme's spec for the role, scales its size,
and fills in three fields — **no lock, no client call, no cache lookup, no
allocation** — so `role_font()` per control paint is arithmetic, and hoisting
it into a `Faces` table cannot buy measurable time. It would also *add*
surface (a table beside the one resolver every caller already shares) and
change every `render` signature and in-crate call site at once, which is why a
first attempt at it sank.

What is genuinely left is unrelated to speed and much smaller: the dead
parameters that attempt exposed — `TitleBar::icon_side` and `split_identity`
take a `font` they do not use, as do `decision::action_row_rects`/`step_rect`,
and `WindowFrame::layout`/`ActionRail::gap`/`Panel::action_rects` need none.
Removing those is a bloat fix, not a Stage C item, and needs no `Faces` table.

**C.4b — done.** Text measurement is memoised in **`lib/font`**, beside the
glyph-bitmap `ReclaimCache` it already owns, so text caching has one home.
The memo is the string's per-character **cumulative advance array**, the
single representation all three queries read: `text_width` is its last entry,
and `truncate_to_width`/`elide_to_width` are a `partition_point` over it
(sound because saturating sums are non-decreasing). Key: the face identity
`GlyphKey` already uses (family, pixel height, weight) plus the text's length
and CRC-32C; the text itself lives in the *value* and is compared on every
hit, because the cache takes its key by value (an owned-string key would
allocate per lookup) and wipes values but merely drops keys (a `Box<str>` key
would leave titles and filenames in reused heap). A fingerprint clash costs a
re-walk, never a wrong width. Epoch: the advance-source generation, bumped
when the font transport is installed — face and scale are in the *key*, not
the epoch, because an epoch change empties the whole cache and one frame
measures several roles at several sizes. Budget: the glyph cache's own
RAM-derived policy, reused verbatim. The monospace path is untouched and pays
**no** memo lookup, because its advance is arithmetic with nothing to save.

**C.4c — done.** A drawn run pays **one glyph lookup per character**, not two.
A glyph's coverage reply already carries that glyph's own advance, so
`draw_text` reads the pen step from the very bitmap it is about to composite
instead of asking the cache for the same glyph again to learn how far to move;
and whether a face is fixed-pitch is a property of the *face*, so it is
resolved once for the whole run rather than per character. `draw_text` is now
one `with_client` borrow over a `draw_on` seam (the shape `width_on`/
`elision_on` already use), which is also what lets a test count lookups on its
own client rather than the process-global one.

Correctness is proven by counts, not timings: an *n*-character run costs *n*
lookups, checked against a test-only reference walk that draws the old way and
must produce identical pixels and an identical final pen position, over both
faces, five strings, and origins on, straddling, and past the surface.

**The fixed-pitch and proportional runs are deliberately written out as two
loops**, not one loop sharing a glyph-blitting call. A fixed-pitch run must not
pay for an advance it discards, and sharing the call gives both runs a closure
that returns one — which measured ~7% worse on *both* faces, `#[inline]` or
not. The first attempt (one loop, a per-character branch on the shared cell)
was worse still: it *regressed* the fixed-pitch path ~4% against the code it
replaced, which is the terminal's own path. Written out, both faces improve.

**Measuring this exposed a trap worth recording.** The harness's default
budget (16 iterations × 5 rounds) leaves ±15% run-to-run spread on a 10 k-pixel
case — the same order as the effect — so a single default-budget pair is *not*
evidence and must not be quoted. At `--iters 400 --rounds 25` the spread falls
to ~1% and the pair is decisive. A small case needs a large budget; the
megapixel composite cases do not, which is why the defaults stay low.

### C.5 One shell present per drained batch  **[done]**
`DesktopShell::handle` split into `apply` (route the event, mutate state) and
`settle` (taskbar `present()`, then `sync_active_frame`, then
`refresh_cursor`). `handle` is still both, so a single event is unchanged;
`pump` runs `apply` per drained event **in order** and `settle` **once**, and
not at all when nothing was drained. Every order-sensitive effect still
applies in sequence — only the screen-settling pass is folded.

Each folded item is level-triggered, which is why folding is exact rather
than merely cheaper: the taskbar `present()` drains a per-surface repaint
latch (set-like and idempotent, so N samples leave the same latch as one);
`sync_active_frame` reconciles the *current* focus and early-returns when it
already matches; `refresh_cursor` re-runs the shape policy against the
current pointer. Intermediate values were never observable because no frame
was published between samples. `mirror_focus`'s conditional second present
was **deleted**, not moved — it repainted a surface the single settle already
drains. A source that faults mid-drain still settles what it delivered, so
the screen can never lag the model.

The same shape appeared twice more and both are folded: the keyboard drain
and the pinboard backdrop menu each called `handle` per event.

### C.6 Tests + docs
- Damage sink: hover enter/leave reports exactly two rects; motion within
  one control reports none; a press reports one; a container reports the
  union of what its children reported and nothing more.
- Routing: with N children, one motion touches at most two; an armed
  child still receives every sample; the resulting `ControlState` is
  identical to the fan-to-all behaviour (a differential test over a
  scripted pointer path).
- App (per app, and the gate on C.3 landing for it): the differential
  proof — render before and after every event of a scripted walk over the
  app's own controls, and assert every changed pixel lies inside what that
  round reported — plus the tight direction (a hover flip presents a rect
  of the control's size, not the window's) and a whole present for a
  resize, a theme change, or a round that reported nothing.
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

## Stage D — Make blur cost what it changes  **[D.1–D.4, D.7, D.8 done; D.5 is a User decision]**

### D.1 Damage below a frost invalidates it; the window's own content does not  **[done]**
Every `damage.add` in the compositor is gone, replaced by three funnels that
say *what kind* of change is being marked, because that is exactly what decides
which retained frosts survive:

- `mark(rect)` — the conservative one, for a change that is not confined to a
  single layer: the root fill, the desktop layer, the density or theme every
  window is drawn with, and restacking, which changes *which* layers a frost
  sees rather than what one of them holds. Drops the frost of every window
  whose bounds it reaches. `raise` and `remove` narrow even this to the index
  the restack actually disturbed, since the windows already below it see the
  same stack as before.
- `mark_layer(id, rect)` — a change confined to one window's own layer: its
  content, position, size, shape or furniture. Drops the frosts of windows
  stacked *above* that one, and of neither its own nor any below. A frosted
  window is blended over a blur of the layers **below** it, so nothing at or
  above its own layer is part of its frost. This covers two of the three
  dominant interactions — the pointer moving inside a frosted terminal, and a
  window dragged across one — and neither costs a re-blur. The third, the
  frosted window's *own* drag, is D.8: `mark_layer` spares its frost too, but
  its rectangle has moved, so what survives is decided per pixel rather than
  wholesale.
- `mark_overlay(rect)` — a change no frost can read: the cursor, composed after
  every window, and the screen reveal, applied only as a pixel is encoded for
  scan-out. Drops nothing, so a pointer sample and a fade step keep every frost.

Losing a frost costs a re-blur and never a wrong pixel, so marking too widely
is the safe direction — but *needlessly* widely is the defect that left a drag
over a frosted window re-blurring it every sample, so a mutation uses the
narrowest funnel whose reasoning is exact.

`compose_plan` no longer promotes a blurred window whose frost is *reusable*:
there is no neighbourhood to spread, so the damage stays the rectangle it was
marked as. It still promotes one that must be recomputed, and recomputing one
frost drops any overlapping frost above it — a blur spreads the change far past
the rectangle that caused it, so the window above reads different bytes even
where the damage never reached.

### D.2 The frosted backdrop is retained  **[done]**
`userland/gui/wm/src/frost.rs`: `FrostedBackdrop` (the rectangle's frosted
pixels plus the rectangle, physical radius and window shape they are a function
of) in a `ReclaimCache` keyed by `WindowId`, built by `frost_cache` from
`lib/reclaim`'s shared desktop policy — which was generalised from
`window_chrome_cache` to `screenful_ui_cache`, since "no more of this can be
visible at once than fills the screen" is the furniture argument word for word
and a second near-identical factory would be duplication.

The rectangle recorded is the window's **whole** one, not the on-screen part of
it: a window pushed off an edge is frosted from the row and column the screen
begins at while its shape is read from its own top-left, so two positions that
clip alike are still two different frosts.

The epoch is `(scale, screen extent)`, deliberately **not** the theme: a palette
change repaints the layers below and marks them damaged, which drops the frosts
that read them. Both epoch components are already caught per entry (the scale
through the radius, the screen through the rectangle), so the epoch is not what
keeps a stale frost off the screen — it is what stops a superseded one staying
*charged* until it is next looked up. A window that stops frosting is never
looked up again, so `set_backdrop_blur(_, 0)` releases its entry outright.

**One counted lookup per frosted window per frame.** The plan and the composite
that follows it both need to know whether a frost may be reused, so the answer
is taken once (`frost_reusable`) and remembered for the frame: two lookups
could disagree, which would leave a window the plan did not widen for being
blurred over a rectangle whose lower layers the frame never composed. The
lookup goes through `get_or_build`, so a reuse is recorded as a **hit** and
refreshes the entry's recency — the frost every frame serves must not be the
first one a squeezed cache gives back — and an entry whose geometry no longer
matches is released before the lookup so the miss is counted once and the stale
pixels stop being charged at once.

That accounting is not decoration: the session registers this cache's ledger
with the process cache report, so its hit ratio is what `sysmon`'s reclaim page
renders. Admitting a frost through `get_or_build(|| Some(v))` recorded a miss
and could never record a hit, so that column read 0% however well the cache was
working — the reading was arithmetically incapable of being right. `FrameStats`
deliberately gains **no** frost hit/miss pair beside `chrome_hits`/
`chrome_misses`: `blur_px == 0` already *is* the per-frame statement that a
frost was reused, and a second tally of it would be duplication. Furniture has
no equivalent pixel signal, which is why it has counters.

The cache is **read-only for the whole of a composite pass** and written at the
end of it (`retain_pending_frost`, through `ReclaimCache::retain`, which counts
no lookup because the frame already counted the one that found nothing):
admitting one mid-pass could evict an entry the pass had already decided to
reuse, and that reuse would then blur a rectangle whose lower layers the frame
only composed where the damage fell.

### D.3 Cheaper, bit-identical blur arithmetic  **[done]**
The divisor is constant for a whole pass (replicated edges keep it at
`2·radius + 1`), so it is resolved once into a fixed-point reciprocal instead
of four integer divides per pixel per pass. It is *exactly* the divide, not an
approximation: `Reciprocal`'s rustdoc carries the proof, and the cutoff
(`count <= 65536`) is where the proof stops holding rather than a comfortable
guess — above it the divide stays.

The output slot and the two samples the sliding window trades are each monotone
along the line, so all three are walked as strided iterators and the furthest
offset any can reach is bounds-checked **once per line** instead of per sample.
No indexing, no `unwrap`, no panic path.

### D.4 Tests + docs  **[done]**
- Every existing `blur_tests.rs` assertion passes unchanged, and
  `tairix-controls`, `tairix-greeter` and `tairix-wm` — which render through
  this blur — pass unchanged, which is the bit-identity witness.
- The blur is asserted byte-identical to a **naive `O(area·radius)` reference**
  written in the test file (no running sum, no reciprocal, no iterator
  arithmetic) over a spread of shapes and radii, including 1×N, N×1, radius 0
  and radius wider than the region.
- The reciprocal's exactness condition is checked for **every** count in range
  plus that a count above the cutoff genuinely breaks it, and its answer is
  compared against a written-out divide oracle for **every reachable sum** at
  the radii a desktop frosts at, plus boundaries and a seeded spread at the
  large counts. Writing these caught a real overflow for a numerator outside a
  legitimate window (the product now saturates, so the answer is total).
- The frost cache: one scene composed twice — reusing frosts and blurring
  afresh — is byte-identical in the scan-out frame *and* the back buffer after
  ~30 mutations (content above/below/inside, cursor motion, a fade, restacking,
  geometry, resize, corners, radius, scale, theme, mode, overlapping frosts, a
  frost clipped by the screen edge, removal). Plus: a content repaint inside a
  frost blurs 0 px and keeps its damage at the marked rectangle; a change below
  re-frosts the whole window exactly once; a present above keeps it; recomputing
  one drops the overlapping frost above it; removal and un-blurring release the
  entry; the ceiling and mild-pressure trim hold and the frame is unchanged
  after a trim; teardown releases everything.
- Docs: `lib/raster/README.md`, the `Reciprocal` and `blur_line` rustdoc,
  `userland/gui/wm/README.md`, `frost.rs`'s module docs,
  `docs/src/desktop/wm.md` (a *Retained backdrops* section), and
  `plans/SMARTRAM.md` (the frost cache as a reclaim client).

**Measured** (release, `cargo xtask bench`, the same scenes as the Stage A
baseline, both figures taken here):

- A `64×24` repaint inside a backdrop-blurred window: **17.43 ms → 27.2 µs**,
  a factor of **640** (D.1/D.2). The pixels the frame touched fell from
  **564 000 to 1 536** — the exact rectangle marked — because the frost no
  longer widens damage it does not have to recompute.
- A full-screen re-frost, which is all D.3 can help: **17.98 → 16.52 ns/px**
  (18.41 → 16.91 ms/frame). The blur family itself is 7.55 ns/px at radius 4
  and 7.71 at radius 24 — still flat in the radius, as a running-sum blur must
  be.
- The opaque cases are unchanged (2.020 → 2.043 and 0.593 → 0.705 ns/px on a
  1 536-pixel case, within run-to-run noise at that size), so nothing was
  traded for it.

### D.5 Decision (not silently taken)
Blurring at half resolution and upsampling is ~4× less area but
**changes the output**. It is therefore a rendering decision for the
User, with a visual comparison, not an optimisation to slip in. Left out
of D unless approved.

### D.6 Follow-up — the vertical pass may be streaming cache, unquantified
The vertical pass walks columns with `stride = width`, so for a wide region
every sample sits on its own cache line and the whole buffer is re-streamed
once per column; a cache-blocked column pass (several columns' running sums
carried at once) would fix it. An exploratory run during this work suggested a
~1.5× penalty for a wide region against a narrow one of equal area, but that
experiment is not in the committed harness and was not reproduced here, so the
figure is **not** evidence. Stage F must add the equal-area wide/narrow case to
`cargo xtask bench` and measure it before acting — the framework there is the
right home for a blocked variant anyway, and blocking must reproduce the
identical bytes like every other candidate.

### D.7 A mutation that changes nothing marks nothing  **[done]**
The frost is only as good as the damage that spares it, and three compositor
mutators marked damage — and so dropped a retained frost — for a call that
changed nothing:

- `raise` on the window already at the front restacked a `Vec` tail back into
  its own slot, then marked the whole window and invalidated every frost from
  that index up. `SessionWindows::keep_popups_stacked` runs immediately before
  every composite while any popup is open and raises the parent and the popup
  each time, so **opening a menu re-blurred the parent's whole window on every
  wake**; `InputRouter::press_primary`/`press_secondary` raise unconditionally
  too, so every click into an already-focused frosted window re-blurred it.
  Measured on a 20×14 frosted window: `damaged_px`/`blur_px` `280`/`280` per
  redundant raise, now `0`/`0`.
- `set_active_frame` re-asserting the activation a frame already shows, and
  `set_window_title` re-setting the label the bar already reads, each re-marked
  their furniture bands *and* dropped that window's chrome-cache entry.
  `decorate_window` builds the frame active and then immediately activates it,
  so the first was on every window open.

Each is one guard where it belongs, mirroring `lower`'s existing early-out; the
activation rule now has a single definition (`window::activation_for`) shared by
the setter and the new `frame_activation_changes` query, so the guard and the
mutation cannot drift.

**The general rule now lives in one funnel.** `mutate_frame` — which all nine
frame mutations run through — hands the mutation a `damage::sink()`, marks
exactly the rectangles it reported over that window's layer, and releases the
window's retained chrome *only when something was reported*. A mutation that
changed no drawn pixel therefore marks nothing and keeps its furniture, so the
two remaining cases of this class are closed with it and no caller computes a
band or invalidates a cache entry of its own:

- A refused mutation (an undecorated or non-resizable `toggle_window_size`, a
  failed reallocation, a retitle to the label already there) reports nothing,
  so it no longer costs a furniture re-render.
- `frame_pointer`/`frame_key` mark what the furniture reported instead of all
  four bands. A pointer sample crossing the drag region costs `0`/`0` and keeps
  the chrome; a hover entering one command control recomposites that control's
  rectangle alone, and a keyboard focus move the two controls the ring moved
  between. The `InputRouter` consequently carries no damage region at all —
  repainting is the compositor's, at the point the frame is mutated — and the
  resize grabber it drives as a gesture engine (the WM's chrome draws no
  grabber) reports into a sink behind `ResizeGrab::gesture`.

### D.8 A frosted window that moves keeps the frost the move cannot reach  **[done]**
The third dominant interaction, and the one the cache was worst at: dragging
the frosted window itself. Its backdrop does not move, so the retained frost is
still exactly right — in *screen* coordinates — wherever neither difference
between the two positions applies. Only two differences exist, and both are
confined to a border:

- the blur **replicates** at its rectangle's edges, so a pixel less than
  `radius_px` inside either position's on-screen rectangle averaged a different
  set of samples;
- the shape **weights** the mix at a window-local coordinate, so a pixel within
  a corner's reach of either position's own rectangle was mixed at a different
  coverage.

So `FrostedBackdrop::reuse` answers `FrostPlan::{Whole, Core(rect), Blur}`,
where the core is the shared rectangle taken in by the larger of the two
reaches. A differing blur radius keeps nothing (every pixel is a different
average); a resize and a corner change are **not** special cases, because the
coverage argument holds for them word for word — which is why `reuse` compares
no shapes for equality. `matches` is gone: an entry is released only when
nothing can be kept from it.

`Surface::frost_region_around` is the raster half: frost a rectangle *except* a
kept inner block, writing exactly what the whole-rectangle frost would write
around it. `blur_line` generalised into `blur_span` (the outputs of a line, not
all of them), so `box_blur` and the partial path share one sliding window;
`frost_region` and `frost_region_around` share one private `frost`, so a border
and a whole cannot round, replicate, weight, or dither differently. Two things
that had to be got right, both found by the tests rather than argued:

- **All four border bands are blurred before any is mixed back.** A band's
  neighbourhood reaches into the bands beside it, and what it must read there is
  the backdrop, not the frost of it. Blurring and mixing band by band seams.
- **`blur_span` confines its source to the line before walking it.** The walk
  reads a clamped edge by letting a strided iterator run out, which held while
  the scratch ended with the line and broke the moment several bands shared one
  max-sized scratch — a band then read a neighbour's pixels as its replicated
  edge.

The frost also no longer copies the rectangle out before blurring it: nothing is
written until both passes are done, so the horizontal pass reads the surface's
own rows. That removes a whole pass over the area from every frost, and it is
the *small* part of this work — 0.7% of a full-screen re-frost, which the two
blur passes and the plane composite dominate.

**The layers a frost covers are no longer composed** (`compose_plane`,
`frost_spared`). A frost is copied over whatever is beneath it, so composing
that stack first is work the copy throws away — and for a dragged frosted
window it was a whole window's worth of blending per pointer sample. A frame
composes below a frost only outside what the frost will write: nothing at all
under one reused whole, and only the ring the border blur *reads* (the core
taken in by the radius) under one reused in part. The remainder is composed as
the disjoint rectangles `Region::subtract` gives, never as the box around them.
The pair cannot disagree about a vanished entry: `frost_spared` and
`frost_segment` both consult the cache with only a composite in between, so a
missing frost composes the plane and blurs in full.

A frost the frame recomputed any part of is captured whole, so the next frame
compares against where the window is *now*; otherwise the core would erode a
sample at a time until nothing was left of it.

**Measured** (`cargo xtask bench --filter composite`, 1280×800, a 560×360
backdrop-blurred translucent window over two overlapping windows, moved 6 px a
frame). Both figures are taken from the *same* binary, with only this
behaviour disabled for the baseline, so the pair differs in nothing else:

- Drag, backdrop blur: **7.05 ms → 3.03 ms** a sample (34.06 → 14.64 ns/px),
  a factor of **2.3**.
- A `64×24` repaint inside a frosted window: **40.6 → 15.0 µs** (26.44 → 9.79
  ns/px), from the spared plane alone — the frost was already reused whole here,
  so this is the layers beneath it no longer being composed.
- Drag, opaque stack unchanged (0.484 → 0.471 ns/px) and the full-screen cases
  unchanged (opaque 2.91 → 2.87, translucent 9.57 → 9.55), so nothing was
  traded for it.

The counters carry the claim in CI: a three-pixel move blurs under a
fifth of the window, and a reused frost resolves *no* pixel of the layer beneath
it. B.6 and D.9 took the blurred drag further still, to **9.30 ns/px**.

### D.9 Any window that reads its backdrop retains one  **[done]**
A frost is a cache of the composed plane, and a window with no blur had none, so
every pointer sample of a plainly translucent drag re-blended the whole stack
beneath it — measured at **19.89 ns/px**, the slowest window on the desktop to
drag, worse than the blurred one.

The expected shape of the fix was a *second* cache for the unblurred plane. It
was not needed and would have been the duplication §2.2 forbids: **a blur of
radius zero leaves the composed layers exactly as it found them**, so the
retained entry already *is* the composed backdrop and the whole retention path —
the epoch, the invalidation funnels, the `reuse` core/whole/blur decision, the
spared-plane composite, the wipe on release — applies unchanged. All that was
missing was admission. One predicate, `Window::reads_backdrop` (a blur, or a
whole-window opacity below full), replaced the two `blur_radius() == 0` gates in
`compose_plan` and `recompose_rect`.

Deliberately excluded, because their backdrop is not a field: an antialiased
corner (a few pixels of arc) and a client painting alpha into its own content
(unknowable without reading every pixel). Both still composite correctly through
the blend path.

Two defects were found and fixed with it:

- `blur_px` counted a radius-zero frost as blurred, reporting blur work that
  never happened. Gated on `radius > 0`.
- `FrostedBackdrop::capture` composited the snapshot onto a fresh transparent
  surface through `blit`, reading and blending every pixel of a screenful. A
  snapshot is a copy: `Surface::overwrite` is the same shared blit walk laying
  rows down whole.

Measured (`cargo xtask bench --filter composite`, 1280×800, a 560×360 window
moved 6 px a frame over two more translucent windows):

- Drag, translucent stack: **19.89 → 6.95 ns/px** (4.12 → 1.44 ms), a factor of
  **2.9**. It is now cheaper than the blurred drag rather than dearer.
- Drag, backdrop blur: **15.30 → 9.30 ns/px**, from B.6 and the cheaper capture.

Gated by two tests: a change-by-change sweep composing one scene twice — every
backdrop retained against every backdrop recomposed — asserting the scan-out and
the back buffer are byte-identical at each of ~30 steps; and a drag measured on
two compositors driven identically, asserting the retaining one resolves under
half the layer contributions and under the damaged area of the other while
producing the same screen. The second fails without the admission change.

---

## Stage E — One present per frame, and a frame deadline

Depends on B and D. Touches the display wire protocol, so it must be one
evolution with `plans/FIX-DISPLAY-ACCELERATION.md` Stage B, not a second
shape (§2.2, §2.13).

### E.1 Keep the damage region disjoint  **[done in C.0]**
The damage region is `tairix_geometry::Region`, whose rectangles are
disjoint and band-canonical, so a scattered frame stays scattered rather
than coalescing to unions. What remains for E is the *present* side:
`Compositor::present` still collapses to the bounding box past
`MAX_PRESENT_REGIONS`, which E.2 replaces with a rect list.

### E.2 One present per frame, carrying a list of rects
`RemoteDisplay::push` rotates the 2-frame ring **per `present_region`
call** and refreshes `union(stale, damage)` as a **bounding rect**: with
8 rects that is 8 IPC round trips, each copying a growing box — in the
worst case ~8 near-full-screen copies for a frame that changed a few
thousand pixels. A window drag needs no pathological scene to hit it: the
frame presents the rectangle vacated and the one arrived at, and each of
those two calls copies the *union* box, so a window moved a few pixels is
copied to the screen twice per sample. Change the `Present` request in place to carry a
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
| B | opaque runs (occlusion is the same mechanism), `encode_run` | A | no | no |
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
