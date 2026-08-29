# FIX-DESKTOP-SPEEDUP — Software-compositor and GUI redraw performance

Status: **A done** (less A.4's QEMU hover vertical), **B done**, **C done**,
**D done** (D.5 is a User decision, D.6 an unmeasured follow-up), **E done**,
**H, I, J done**. **F** is gated on the F.0 decision and may not land before
B–C. **G** is kernel work gated on a User decision (§15.7).

Binding under `AGENTS.md` (§3, §15.18). This plan closes the standing
performance defect that the desktop repaints **orders of magnitude more pixels
than a frame changes**, with a per-pixel scalar loop. It is the software half of
desktop performance; `plans/FIX-DISPLAY-ACCELERATION.md` is the hardware half.
Neither depends on the other, and the software path is the mandatory
always-available fallback on every target (§17.3) — it is what runs when
acceleration is absent, refused, or falls back, which a backdrop-blur frame
always does.

**The order is binding: (0) measure, and check you measured the right binary →
(1) stop doing the work → (2) do the remaining work faster.** Vectorising a loop
that should not be running is forbidden; Stage F may not land before Stages B–C.

---

## Read first (§15.18)

- `AGENTS.md` §2.16 (performance first-class, measure don't guess), §2.2 (one
  definition, one blend path), §2.9/§5.4 (no panic, fail closed), §2.23 (no
  busy-poll), §17.1 (tickless, one-shot timers), §24.1/§24.4 (grown capacities
  vs fixed security bounds), §26.2/§26.3 (contended, memory-pressured operating
  conditions), §27 (foundational primitives are complete).
- `plans/FIX-DISPLAY-ACCELERATION.md` — Stage B's `PresentLayers` request shape
  (Stage E here evolves the *same* wire protocol, never a second one) and Stage
  D's per-layer damage (Stage C here produces it).
- `plans/GUI-CONTROLS-DESIGN.md` — the Reactive Alloy control model Stage C adds
  damage reporting to; the drawing recipes must not change.
- `plans/COMPOSITOR-WORK.md` — server-side window furniture, the chrome cache
  Stage D's frost cache is modelled on.
- `plans/FIX-DESKTOP.md` — non-blocking launch; Stage E's pacing shares its "an
  interactive loop never stalls" rule.
- `plans/SMARTRAM.md` — `lib/reclaim`; every cache this plan adds is a reclaim
  client with a budget, never an unbounded retainer.
- `plans/FONT-SERVICE.md` — font/glyph ownership; Stage C's text-measure memo
  lives in the font layer, not in `lib/controls`.
- `plans/FIX-HARDWARE-FEATURES.md` — `lib/cpuops`, the `lib/pagezero` candidate
  template Stage F copies, and the P3b axis correction Stage F depends on.
- `plans/NEW-SWITCHBOARD.md` — where Stage A's frame counters surface.
- `plans/WIRING.md`, `plans/ARCHSUPPORT.md` — Stage G's per-port work.

---

## What is left

Stages A–E are done. What remains is **A.4**'s QEMU hover vertical and the two
gated stages: **F** behind the F.0 decision, **G** behind a User decision.

Independently: **every published number is taken from a `--release`/installer
image.** A dev-profile timing is never quoted as evidence.

---

## Goal / invariants (bind every stage)

1. **One blend definition, one raster path (§2.2).** `Pixel::over` / `div255` in
   `lib/raster` stay the single blend, reached through the one span composite
   (`blend_span`). Stages B and F add *specialised loops* over that definition
   (an opaque run is a copy; a vector candidate reproduces the identical
   rounding) — never a second blend, a "fast" approximation, or a forked
   rasteriser.
2. **Bit-identical output, proven.** Every fast path, cache, and CPU candidate
   produces byte-identical frames to the portable reference for the same scene,
   asserted by composing the scene both ways. A change that alters output is a
   *deliberate, documented rendering decision with a visual test*, never a
   silent tweak. Exactly one has been made: B.5's per-pixel dither on a
   *blended* pixel, whose bound is stated there; a copied or opaque pixel is
   still byte-identical.
3. **Tests assert work, not wall-clock (§7).** CI gates on deterministic
   counters — pixels blended, rects presented, controls repainted, IPC round
   trips, cache hits — which are load-independent. A wall-clock threshold in CI
   is forbidden; timings are evidence for a completion report, produced by the
   Stage A harness, never a pass/fail gate.
4. **No security or correctness trade (§2.17, §2.9).** `overflow-checks` stays
   `true` in both profiles; the fix for arithmetic cost is hoisting it out of
   the inner loop, never disabling the check. No `unwrap`/`expect`/`panic!` on a
   frame path. A client-supplied damage rect is validated and clipped by the
   receiver (§5.4) — a smaller present must never become a way to smuggle an
   out-of-bounds rect.
5. **Every cache is bounded, reclaimable, and keyed by an epoch (§24.1,
   §26.3).** The frost cache, the text-measure memo, and any run/layer cache are
   `lib/reclaim` clients with a budget derived from discovered memory,
   invalidated by an explicit epoch (scale, theme, backdrop generation), never a
   fixed `const` retainer and never proportional to screen or window count.
6. **No busy-wait, no periodic tick (§2.23, §17.1).** Frame pacing is a
   **one-shot** timer armed for the next deadline; an idle desktop arms nothing
   and parks. Nothing polls for the next frame.
7. **Platform-neutral (§2.20/§2.21).** Everything in Stages A–E, I and J is
   arch-neutral. Stage F's ISA-specific candidates follow the `lib/pagezero`
   shape — a `build.rs`-emitted cfg, never `cfg(target_arch)` in source, so
   `cargo xtask cfg-check` stays green.
8. **Foundational primitives are complete (§27).** The region type (C.0) and the
   control damage sink (C.1) are the whole abstraction, not the slice the first
   caller happens to use.
9. **No speculative surface (§2.3/§2.4).** No ABI field, capability, or public
   method lands before the change that consumes it. This plan introduces no new
   capability; the frame counters ride the existing session→Switchboard feed.

---

## Stage A — Measure, and measure the right binary  **[done, less A.4]**

- **A.1 Product-speed per-pixel crates in every profile.** `tairix-wm`,
  `-controls`, `-font`, `-window` and `-display` join the existing
  `[profile.dev.package.*]` `opt-level = 3` overrides, because the debug/QEMU
  images build userland `Run` binaries in the dev profile (`tools/xtask`
  `pie_build`). Overflow checks and debug assertions stay on.
- **A.2 `cargo xtask bench`.** The raster, text and whole-frame composite
  families run through `lib/cpuops`'s existing `BenchHarness` with a host time
  source injected through its `CycleCounter` seam — no new dependency (§2.12).
  Text draws through the production entry point with a warm glyph cache, or the
  figure would describe the mock service's reply encoding. Not a CI pass/fail
  gate (invariant 3).
  - **A small case needs a large budget.** The default budget leaves ±15%
    run-to-run spread on a 10 k-pixel case — the same order as a candidate's
    effect — so a single default-budget pair is *not* evidence. Use
    `--iters 400 --rounds 25` there; the megapixel composite cases do not need
    it, which is why the defaults stay low.
- **A.3 Frame work counters.** `Compositor::frame_stats` snapshots a per-frame
  `FrameStats` (damaged, blended, copied, frosted, encoded px, dirty rects,
  present calls, furniture-cache hits/misses), surfaced as the Desktop block of
  the Switchboard's System → Resources page over the port that already carries
  the seat report — no new syscall, sysinfo query, or capability, and a receiver
  that validates every count and fails closed. The load-bearing reading is
  **damaged px vs blended px vs screen px**.
  - **A monitor must not measure its own act of displaying.** The session
    suppresses a `FrameReport` when the only content served since the last
    decision came from the live Switchboard's own window(s); without that gate
    the panel rebuild is itself a frame whose counters differ, which sends
    another report, forever. Rate-limiting or quantising the counters is not a
    fix — the content gate is.
  - `FrameStats` deliberately carries **no** frost hit/miss pair: `blur_px == 0`
    already *is* the per-frame statement that a frost was reused, and a second
    tally would be duplication. Furniture has no equivalent pixel signal, which
    is why it has counters.

**Remaining — A.4 the QEMU hover vertical.** A desktop vertical that hovers
across a control-rich window and asserts **counter bounds**, not timings. This
is the regression gate every later stage tightens, and it does not exist yet.
Unit counter tests (exact counts for a scripted scene, zero for an empty-damage
frame) are in.

**It is blocked on decision 5 below, and the blocker is not effort.** A guest
kernel observes userland through the audit trail, and the trail cannot carry
this assertion:

- **`FrameStats` reaches nothing a guest can read.** A.3 deliberately routes
  the counters over the port that already carries the seat report, to the
  Switchboard's own page — no syscall, no sysinfo query, no log record. There
  is nothing for a guest sink to latch.
- **A present is not recognisable in the trail.** `CallReplied` carries
  `endpoint`, `ticket` and `len`, and on `DISPLAY_ENDPOINT` both `Present` and
  `Configure` — and every error and decode-failure path on either — answer with
  the same four-byte status word; only `Query`'s twenty-byte mode reply is
  distinguishable. Counting presents by reply length is precisely the guess
  that caused `plans/OPEN-DEFECTS.md` D10, and `appbar`'s own contract records
  the same refusal for the window channel. So "presents per hover" cannot be
  counted from the trail either.
- **The pointer script has no hover.** `PointerPen` emits one jump-to-target
  `Move` per step, and the enrolled-script unit test asserts every script ends
  on a `Click`; a run of motion samples needs a `PointerPen` hover helper and
  that test amended. That part *is* only effort.

So the vertical needs an observation channel that does not exist, and inventing
one is new surface: either a rate-limited frame-cost record on the system log,
or a `sysinfo` query for the counters. Both are decisions about what the system
publishes, not test plumbing, so neither is taken here.

---

## Stage B — Stop blending pixels nothing can see  **[done]**

Compositor-local: no ABI change, no app change.

- **B.1/B.2 Opaque runs *are* the occlusion cull, and there is no second
  mechanism.** `WindowRow::opaque_run` yields the longest run of source pixels
  that each replace what is beneath them exactly; `compose_row` copies such a
  run into the back buffer with `copy_from_slice` and encodes it with one
  `encode_run`. A copied run has skipped every layer below it — the windows
  beneath, the desktop layer, the root fill — for exactly those columns. It is a
  loop specialisation, not a second blend: *over* with a fully opaque source
  **is** the source.
  - **Sound without trusting a client:** "fully opaque" is read from the source
    pixels (alpha 255, full window opacity, no rounding coverage on the row), so
    a window whose *content* is translucent can never cull what shows through
    it. A window-level `opacity == 255` test would have been wrong.
  - Runs are sought only **within a blur segment**, so a blurred window stays a
    cull barrier and nothing a frost reads is ever skipped. A fade in flight and
    the rows the cursor draws on take the general path, because both change the
    bytes a copy would have written.
  - The condition set is stated once, in `compose_row`'s rustdoc and
    `WindowRow::opaque_run`.
- **B.3 Run-at-a-time encode.** `ChannelOrder::encode_run` sits beside `encode`
  in `lib/display/src/scanout.rs` — **not** `lib/abi`, which cannot name a pixel
  type without closing the cycle `abi → raster → theme/reclaim → abi`. It is
  defined over `encode`, returns the whole pixels written (so a short `out`
  truncates instead of panicking, and a partial trailing group is never
  written), and is not ABI surface. There is no bulk-`memcpy` case: `Pixel`
  carries no layout guarantee to copy through.
- **B.5 Blended pixels are dithered — the one sanctioned output change.** A
  blend into the 8-bit back buffer admits only `256 - a` of the 256 levels
  beneath it, so one fixed rounding stepped a smooth wallpaper into plateaus
  under a translucent window. Every blended pixel now rounds at its own bias
  from `tairix_raster::DitherRow`, resolved once per screen row and indexed by
  screen column.
  - **The bound, stated rather than assumed:** the dither's tile mean is exactly
    `ROUND_NEAREST`, so nothing lightens or darkens, and no pixel moves more
    than one level from the undithered answer.
  - Not a second blend: `div255` *is* `(value + 127) / 255`, so the biased
    divide is the same arithmetic with its rounding point named, and every
    unbiased operator delegates to it. Cost falls only on pixels that were
    already blending; the B.1 copy path pays nothing.
  - **The hardware path keeps the guarantee by not taking work it cannot do.**
    No layer stack can express a per-pixel dither, so
    `Compositor::has_translucent_window` sends a window-wide translucency
    through software exactly as a backdrop blur does, and a baked layer
    (`Window::sample_local`) reads the dither at the pixel's *screen* position
    (`plans/FIX-DISPLAY-ACCELERATION.md` A.3).
- **B.6 A segment is composed a layer at a time, not a pixel at a time.** The
  columns between two copyable runs are one **segment**, composed across its
  whole width as the base fill, the desktop row, each window row back to front,
  then the cursor — each a straight run at a screen column and a constant
  opacity, laid through `blend_span`, which `Surface::blit` also takes. A
  per-pixel `compose_pixel` no longer exists.
  - **What keeps it exact:** a window row is three straight runs (two furniture
    strips and the client's drawable pixels) *except* where the shape cuts it,
    and there `WindowRow::blend_into` keeps the column walk, as does the cursor.
    The dither is read at each pixel's own surface column, so a run split
    anywhere writes what the whole run wrote and a segment boundary that moves
    with a window cannot seam.

---

## Stage C — Repaint the control that changed, not the window

**[C.0–C.3, C.4b, C.4c, C.5 done; C.4a withdrawn]**

### C.0 One region type, in one place  **[done]**
`tairix_geometry::Region` (`lib/geometry/src/region.rs`) is the one region type;
the WM-private `DamageRegion` is deleted. It holds a pixel set as
pairwise-**disjoint**, band-ordered rectangles in a canonical form, so equal
sets compare equal, no pixel is composited or presented twice, and two far-apart
updates stay two small rectangles instead of collapsing into the box between
them.

- Surface: `new`, `with_budget`, `budget`, `is_empty`, `rects`, `bounds`,
  `clear`, `add`, `subtract`, `clip`, `translate`, `contains`, `intersects`,
  `From<Rect>`. `add`/`subtract`/`clip` are one linear band-stripe merge walk
  over a shared `combine`, whose two buffers are reused so a frame's edits
  allocate once.
- `translate` collapses to the clamped bounding box rather than wrap or drop a
  rectangle when a coordinate would leave `i32` range — over-cover is safe,
  silent loss is not. `with_budget` degrades to the bounding box past its
  rectangle count; `new` stays exact and grows.
- A `contains_rect` and a by-value `clipped` are deliberately **absent**: no
  consumer needs either (invariant 9).
- The compositor consumes it through a **compose plan** rather than
  damage-widening: damage touching a backdrop-blurred window whose frost must be
  recomputed promotes that window's whole screen-clipped rectangle into one plan
  rectangle (overlapping blurred windows merge, because each reads what the
  other wrote) and *subtracts* it from the disjoint residual. The frost sees a
  whole rectangle and cannot seam, while damage elsewhere stays as tight as it
  was marked.

### C.1 A damage sink in `lib/controls`  **[done]**
`lib/controls/src/damage.rs` is the seam: `sink()` hands out a
`Region::with_budget(8)`, and **two guarded writes** decide when a change is
worth reporting, so no family invents its own rule —
`damage::set(field, value, bounds, damage)` for one drawn field, and
`damage::move_mark` for an index-valued mark a container draws on one child at a
time. `RenderInvariant` fields report nothing, exactly as they fail to trip the
render gate. Both writes are **public**, because a host guards its own fields
with them rather than hand-rolling a comparison beside every setter.

- **The budget is 8** because a host pays twice per reported rectangle (re-render
  clipped to it, then present it) and the compositor refuses more than eight
  present round trips per frame, so a ninth could never buy a separate present.
  One routed pointer event produces at most four (child left, child entered, a
  child holding a press, the container's chrome), so an interactive frame stays
  exact while a whole-model refresh degrades to the one box it may as well have
  been.
- **No per-control `last` rect.** A container owns its children's geometry, so
  given a scale and theme it can always name both the rectangle a mark left and
  the one it arrived on (`TitleBar::move_focus` is the worked example). A
  control's *own* bounds moving is a host layout decision, and only the host
  that moved it knows both rectangles. A `last` field would be a second, staler
  copy of the host's layout with no render path reading it (§2.3).
- **Where a report is the host's.** A value the host commits back into a control
  (`Toggle::set_on`, `Slider::set_value`, …) is reported by the owner, which
  holds that control's rectangle at exactly that moment. A mark of the host's
  own that moves between two controls — keyboard focus — is reported by
  `damage::move_mark` over the host's focus field, after which the per-control
  flags are written unconditionally: if the field did not move, no ring moved.
  Focus landing on the host's own chrome maps to `None` and the chrome reports
  itself.
- **Container-mark setters are closed:** `Tabs::set_current`/`set_selected`,
  `Menu::set_current`, `TableHeader::set_sort`, each with an `adopt_*` sibling
  for a rebuild that shares the one admission rule so a rebuild cannot admit a
  mark the interactive path would refuse. `set_selected` sweeps *every* tab and
  reports each whose selection actually changed, because the owner sets each
  tab's initial selection and nothing may assume only one was ever lit.
  `move_mark` is generic over the mark, because a sort carries its direction.
  `ComboBox` adopts internally: every path that moves its menu's highlight while
  the popup is on screen already reports that whole popup.
- Deliberate shapes worth keeping: `ScrollBar` reports its **whole bar**,
  because its awake look is the whole bar, not the part under the pointer; the
  text fields never compare their buffer, so a secret field's characters are
  never copied into a comparison temporary; `cell_shows` is the one definition of
  which breadcrumb cell shows crumb *i*, read by both the render path and the
  report, so an elided ancestor's ring is reported on the ellipsis.

### C.2 Enter/leave hover routing in containers  **[done]**
`Toolbar`, `Panel`, `Rail`, `Decision` and the collection families track the
hovered and armed child and route through the shared `route_pointer` /
`grab_after` policy in `lib/controls/src/paint.rs` — one hit test per event, then
delivery to at most the child left, the child entered, and any child holding a
press. The grab is deliberately *wider* than the child's own latch, because a
container cannot see whether a disabled or denied child caught the press;
over-grabbing only routes further events to a child that ignores them. A
`#[cfg(test)] fan_pointer` oracle keeps the old delivery as the differential
reference.

### C.3 Apps present the rect they changed  **[done]**

No ABI change is required: `lib/window`'s `WindowClient::present` already carries
a per-present `DamageRect`. The decision is shared, not per-app —
`tairix_window::present_damage` over `Repaint::{Nothing, Reported, Whole}`, with
`damage_in` clipping a reported client-space rectangle onto the window (the app's
own fail-closed step, since the session refuses one outside the surface).

**The recipe, in this order per app:**

1. **Retain the surface.** Allocating *and zeroing* a window-sized `Surface` per
   present is a whole-window pass in its own right. The surface lives for the
   life of the window.
2. **Clip the draw to the damage**, which is sound *because* the surface is
   retained: every pixel outside the clip is the one already on screen.
   `Surface::with_clip` confines writes centrally (every primitive reaches
   pixels through `row_span_mut`), so no control needs changing.
3. **Convert and present only that rectangle.** The conversion itself is not the
   app's to write — it is `tairix_display::winframe::encode` (Stage J).

A round that changed the view but reported nothing presents the **whole window**,
not nothing: over-covering costs pixels, under-covering leaves a stale frame,
because the session copies only what a present declares. That safety net is
reachable (a focus step that finds nowhere to move reports nothing yet answers
"changed") and is **not** a licence to under-report.

**Every app landed with the same two-directional proof.** A host test renders
the app before and after every event of a scripted walk over its own controls
and asserts every changed pixel lies inside what that round reported; further
tests hold the *tight* direction (a hover reports exactly the widget entered, a
second sample inside it reports nothing) or the whole thing would pass by
presenting everything. Each app also carries its host-owned reports (a
committed value, a focus mark — C.1).

A model refresh that is not a control round (a clock tick, an animation, new
service data, a resize, a theme change, a first paint) keeps presenting whole,
which is correct and needs no report.

- **`widgets` is the control-tree recipe**, landed exactly as above.
- **`viewer` and `wallpaper` follow it.** Both hold one window-sized surface for
  the life of the window, reallocated with the frame region on a resize and
  adopted only once the session accepts the re-map. The viewer's engine draws
  through `Viewer::render_into` — the intermediate text sub-surface it used to
  allocate and blit is gone — and reports the text area and the bar together
  whenever the scroll offset moves, which is the one commit its host makes into
  a control. The chooser reports its gallery marks through
  `damage::move_mark` over the tile rectangles (`Chooser::candidate_rect`), adds
  the preview model and its caption when the selection moves, reports the status
  line when an apply outcome is committed, and — the win peculiar to this app —
  reports the *one square* a thumbnail arriving from the sandbox fills, so
  filling an N-wallpaper grid costs N tiles rather than N whole windows.
- **The one wire-to-geometry conversion is shared.** `tairix_window::pointer_point`
  widens a wire pointer position into the signed geometry the controls hit-test
  in; the seven private copies (two of them identically named `client_point`)
  are deleted.
- **`terminal` reports from a *cell diff*, not from control rounds**, because a
  character grid has no control tree: `render::Screen` retains the surface *and*
  the cells it was last painted from, and `Screen::paint` returns the block that
  differs (widened to whole glyphs, so clobbering a wide glyph's continuation
  cell repaints its lead cell). Two things a diff cannot see for itself are
  explicit `Screen::invalidate` calls: new colours or a new face, and a session
  redraw request. A resize needs no call site — `present_frame` reconciles the
  picture to the `DisplayMode` describing the frame region, so a surface and a
  region of different shapes cannot arise however a resize half-fails. A screen
  effect *is* inherently whole-frame, so an active pass copies the finished
  screen into a reused buffer, runs there and presents whole, leaving the
  retained screen clean; the buffer exists only while an effect is on.
  - Its settings sheet's strip reports on both paths; the sheet's radios,
    sliders and buttons still need the `move_mark` focus report.
- **Translucency and backdrop blur are not passes**, so a see-through frosted
  window types at cell-diff cost too. An opacity a hair below full is invisible
  on screen yet takes the unpremultiply divide path and the compositor's blend
  path for every pixel; the fix is to remove that cliff, never to snap the
  slider to hide it.
- **`files` reports from *marks*, not control rounds**, for the terminal's
  reason: its rows, tiles and rail rows are built afresh from the browser's own
  state each frame, so there is no control to report itself. `sidebar::RailMark`
  (hover, cursor, keyboard focus) and `listing::ViewMark` (focused entry, scroll
  offset) are read before a round and reported after it, resolving back to
  rectangles through the renderer's own geometry — `render::entry_rect`,
  `SidebarView::row_rect`, `render::item_area` — so the reported rectangle and
  the painted one are one fact. The painter is `render_into`, into a surface the
  window owns for its life; the allocating `render` is **deleted**, and the
  session's picker allocates its own. Every other round is `Repaint::Whole` and
  that is the *correct* answer, not a deferral: a listing change, an overlay, a
  toolbar command, a resize, a re-theme each move more than a report could
  describe. The two conclusions merge (`Whole` wins), so a round that reported a
  rectangle *and* replaced the listing still covers the window.
- **`switchboard` already retained its surface and already had the sink** — its
  sections have reported into `damage::sink()` since C.1 — but the sink was
  built inside `Switchboard::on_pointer` and dropped. C.3 hoists it to the
  `Panel`, which is what owns "what is on screen" (the `Presented` record), so
  the report reaches the present. The composition-wide transitions the controls
  cannot describe report their own rectangles (a scroll marks the content
  column, a section change the whole client via the focus sweep, opening or
  dismissing the section list the popup's rect, and a Tasks selection the two
  rows plus the rail it re-states); everything else calls `Panel::repaint_whole`.
  `Switchboard::view_mut` is **deleted**: input routes through the panel.

**Receiver side is already fail-closed and needs no change:** the session's
`window_presented` refuses a `DamageRect` outside the client's surface, or a
frame shorter than the damage needs, with `Errno::OutOfRange`, and the
compositor's `present_window_content` intersects the translated rectangle with
the window's own client rectangle, so an over-large or negative one is clipped
and can never reach a neighbouring window.

### C.4 Draw and measure text once  **[C.4b, C.4c done; C.4a withdrawn]**

- **C.4a is withdrawn, and measurement is why.** `BitmapFont::for_role` reads
  the theme's spec for the role, scales its size and fills in three fields — no
  lock, no client call, no cache lookup, no allocation — so `role_font()` per
  control paint is arithmetic and hoisting it into a `Faces` table cannot buy
  measurable time. It would also add surface beside the one resolver every
  caller shares (§2.3). Nothing is left of this item.
- **C.4b Text measurement is memoised in `lib/font`**, beside the glyph-bitmap
  `ReclaimCache` it already owns, so text caching has one home. The memo is the
  string's per-character **cumulative advance array**, the single representation
  all three queries read: `text_width` is its last entry, and
  `truncate_to_width`/`elide_to_width` are a `partition_point` over it (sound
  because saturating sums are non-decreasing).
  - **Key:** the face identity `GlyphKey` already uses (family, pixel height,
    weight) plus the text's length and CRC-32C. The text itself lives in the
    *value* and is compared on every hit, because the cache takes its key by
    value (an owned-string key would allocate per lookup) and wipes values but
    merely drops keys (a `Box<str>` key would leave titles and filenames in
    reused heap). A fingerprint clash costs a re-walk, never a wrong width.
  - **Epoch:** the advance-source generation, bumped when the font transport is
    installed. Face and scale are in the *key*, not the epoch, because an epoch
    change empties the whole cache and one frame measures several roles at
    several sizes. **Budget:** the glyph cache's own RAM-derived policy, reused
    verbatim.
  - The monospace path is untouched and pays **no** memo lookup: its advance is
    arithmetic with nothing to save.
- **C.4c A drawn run pays one glyph lookup per character, not two.** A glyph's
  coverage reply carries its own advance, so `draw_text` reads the pen step from
  the bitmap it is about to composite instead of asking the cache again; and
  whether a face is fixed-pitch is a property of the *face*, resolved once per
  run. `draw_text` is one `with_client` borrow over a `draw_on` seam (the shape
  `width_on`/`elision_on` use), which is also what lets a test count lookups on
  its own client. Correctness is proven by counts against a reference walk that
  draws the old way and must produce identical pixels and an identical final pen
  position.
  - **The fixed-pitch and proportional runs are deliberately two written-out
    loops.** A fixed-pitch run must not pay for an advance it discards, and
    sharing one glyph-blitting call gives both runs a closure that returns one,
    which measures worse on *both* faces; a single loop with a per-character
    branch is worse still and regresses the terminal's own path.

### C.5 One shell present per drained batch  **[done]**
`DesktopShell::handle` is split into `apply` (route the event, mutate state) and
`settle` (taskbar `present()`, then `sync_active_frame`, then `refresh_cursor`).
`handle` remains both, so a single event is unchanged; `pump` runs `apply` per
drained event **in order** and `settle` **once**, and not at all when nothing was
drained. The keyboard drain and the pinboard backdrop menu fold the same way.

Folding is exact rather than merely cheaper because each settled item is
level-triggered: the taskbar `present()` drains a set-like idempotent
per-surface repaint latch; `sync_active_frame` reconciles the *current* focus and
early-returns when it already matches; `refresh_cursor` re-runs the shape policy
against the current pointer. No frame is published between samples, so
intermediate values were never observable. A source that faults mid-drain still
settles what it delivered. `mirror_focus`'s conditional second present is
**deleted**, not moved.

### C.6 Tests + docs
Landed with C.0–C.2, C.4 and C.5 (region disjointness/subtract/budget/property
tests against a naive grid model; hover enter/leave reporting exactly two rects
and motion within one control reporting none; the routing differential against
`fan_pointer`; the shell batch producing one taskbar present and one cursor
refresh with the same final state; the docs in
`plans/GUI-CONTROLS-DESIGN.md`, `lib/controls/README.md`, `lib/geometry`'s
rustdoc and `docs/src/desktop/`).

Per app, the two-directional differential proof above landed with its C.3, plus
a whole present asserted for a resize, a theme change, or a round that reported
nothing (`files`: `sidebar_tests.rs` + `listing_tests.rs`; `switchboard`:
`view/mod_tests.rs` + `panel_tests.rs`).

**Acceptance:** the A.4 hover vertical's damaged-pixel counter drops from
window-area to control-area; every existing control and WM test still passes
unchanged.

---

## Stage D — Make blur cost what it changes  **[done; D.5 is a User decision, D.6 an unmeasured follow-up]**

### D.1 Three damage funnels, because the kind of change decides which frosts survive
There is no bare `damage.add` in the compositor. A mutation uses the **narrowest
funnel whose reasoning is exact** — losing a frost costs a re-blur and never a
wrong pixel, so marking too widely is the safe direction, but *needlessly*
widely is the defect this closes:

- `mark(rect)` — a change not confined to a single layer: the root fill, the
  desktop layer, the density or theme every window is drawn with, and
  restacking. Drops the frost of every window whose bounds it reaches.
- `mark_layer(id, rect)` — a change confined to one window's own layer (content,
  position, size, shape, furniture). Drops the frosts of windows stacked
  *above* that one only: a frosted window is blended over a blur of the layers
  **below** it, so nothing at or above its own layer is part of its frost.
- `mark_overlay(rect)` — a change no frost can read: the cursor, composed after
  every window, and the screen reveal, applied as a pixel is encoded for
  scan-out. Drops nothing.

`compose_plan` promotes only a blurred window whose frost must be **recomputed**;
recomputing one drops any overlapping frost above it, because a blur spreads the
change far past the rectangle that caused it.

### D.2 The frosted backdrop is retained
`userland/gui/wm/src/frost.rs`: `FrostedBackdrop` (the rectangle's frosted
pixels plus the rectangle, physical radius and window shape they are a function
of) in a `ReclaimCache` keyed by `WindowId`, built by `frost_cache` from
`lib/reclaim`'s shared `screenful_ui_cache` policy — generalised from
`window_chrome_cache`, since "no more of this can be visible at once than fills
the screen" is the furniture argument word for word.

- The rectangle recorded is the window's **whole** one, not the on-screen part:
  a window pushed off an edge is frosted from the row and column the screen
  begins at while its shape is read from its own top-left, so two positions that
  clip alike are still two different frosts.
- **Epoch: `(scale, screen extent)`, deliberately not the theme.** A palette
  change repaints the layers below and marks them, which drops the frosts that
  read them. Both epoch components are already caught per entry, so the epoch is
  not what keeps a stale frost off the screen — it is what stops a superseded one
  staying *charged*. `set_backdrop_blur(_, 0)` releases the entry outright.
- **One counted lookup per frosted window per frame.** The plan and the
  composite both need to know whether a frost may be reused, so the answer is
  taken once (`frost_reusable`) and remembered: two lookups could disagree,
  leaving a window blurred over a rectangle whose lower layers the frame never
  composed. The lookup goes through `get_or_build`, so a reuse records a **hit**
  and refreshes recency, and an entry whose geometry no longer matches is
  released before the lookup so the miss is counted once. The session registers
  this ledger with the process cache report, so the hit ratio is what `sysmon`'s
  reclaim page renders.
- **The cache is read-only for a whole composite pass** and written at the end
  (`retain_pending_frost`, through `ReclaimCache::retain`, which counts no
  lookup): admitting one mid-pass could evict an entry the pass had already
  decided to reuse.

### D.3 Cheaper, bit-identical blur arithmetic
The divisor is constant for a whole pass (replicated edges keep it at
`2·radius + 1`), so it is resolved once into a fixed-point `Reciprocal` instead
of four integer divides per pixel per pass. It is *exactly* the divide, not an
approximation: the rustdoc carries the proof, and the cutoff (`count <= 65536`)
is where the proof stops holding rather than a comfortable guess — above it the
divide stays. The output slot and the two samples the sliding window trades are
each monotone along the line, so all three are strided iterators bounds-checked
**once per line**. No indexing, no `unwrap`, no panic path.

### D.4 Tests + docs
The blur is asserted byte-identical to a **naive `O(area·radius)` reference** in
the test file over a spread of shapes and radii (1×N, N×1, radius 0, radius
wider than the region); the reciprocal's exactness condition is checked for every
count in range, that the cutoff is where it breaks, and against a written-out
divide oracle over every reachable sum at desktop radii. The frost cache is
proven by composing one scene twice — reusing frosts and blurring afresh —
byte-identical in the scan-out frame *and* the back buffer across ~30
mutations, plus the counter assertions for each funnel, the ceiling and
mild-pressure trim, and teardown. Docs: `lib/raster/README.md`, the `Reciprocal`
and `blur_line` rustdoc, `userland/gui/wm/README.md`, `frost.rs`'s module docs,
`docs/src/desktop/wm.md` (*Retained backdrops*), and `plans/SMARTRAM.md`.

### D.5 Decision (not silently taken)
Blurring at half resolution and upsampling is ~4× less area but **changes the
output**. It is therefore a rendering decision for the User, with a visual
comparison, not an optimisation to slip in. Left out of D unless approved.

### D.6 Follow-up — the vertical pass may be streaming cache, unquantified
The vertical pass walks columns with `stride = width`, so for a wide region every
sample sits on its own cache line and the whole buffer is re-streamed once per
column; a cache-blocked column pass (several columns' running sums carried at
once) would fix it. No committed measurement exists, so there is **no evidence**
yet. Stage F must add the equal-area wide/narrow case to `cargo xtask bench` and
measure it before acting — that framework is the right home for a blocked
variant anyway, and blocking must reproduce the identical bytes like every other
candidate.

### D.7 A mutation that changes nothing marks nothing
`mutate_frame` — which all nine frame mutations run through — hands the mutation
a `damage::sink()`, marks exactly the rectangles it reported over that window's
layer, and releases the window's retained chrome **only when something was
reported**. So a refused mutation (an undecorated or non-resizable
`toggle_window_size`, a failed reallocation, a retitle to the label already
there) costs no furniture re-render, and `frame_pointer`/`frame_key` mark what
the furniture reported rather than all four bands. No caller computes a band or
invalidates a cache entry of its own.

- `raise`/`lower` on a family already at the end it is being moved to, and
  `set_active_frame`/`set_window_title` re-asserting what is already shown, each
  early-out. The activation rule has a single definition
  (`window::activation_for`) shared by the setter and the
  `frame_activation_changes` query, so the guard and the mutation cannot drift.
- The `InputRouter` consequently carries no damage region at all — repainting is
  the compositor's, at the point the frame is mutated — and the resize grabber it
  drives as a gesture engine reports into a sink behind `ResizeGrab::gesture`.

### D.8 A frosted window that moves keeps the frost the move cannot reach
A moved window's backdrop does not move, so the retained frost is still exactly
right — in *screen* coordinates — wherever neither difference between the two
positions applies. Only two exist, and both are confined to a border: the blur
**replicates** at its rectangle's edges (a pixel less than `radius_px` inside
either position averaged a different sample set), and the shape **weights** the
mix at a window-local coordinate (a pixel within a corner's reach was mixed at a
different coverage).

`FrostedBackdrop::reuse` therefore answers `FrostPlan::{Whole, Core(rect),
Blur}`, where the core is the shared rectangle taken in by the larger of the two
reaches. A differing radius keeps nothing; a resize and a corner change are
**not** special cases, because the coverage argument holds for them word for
word — which is why `reuse` compares no shapes for equality. An entry is
released only when nothing can be kept from it. A frost the frame recomputed any
part of is captured whole, so the next frame compares against where the window is
*now* — otherwise the core would erode a sample at a time.

`Surface::frost_region_around` is the raster half: frost a rectangle *except* a
kept inner block, writing exactly what the whole-rectangle frost would write
around it. `blur_line` generalised into `blur_span` (the outputs of a line, not
all of them) so `box_blur` and the partial path share one sliding window, and
`frost_region`/`frost_region_around` share one private `frost`, so a border and a
whole cannot round, replicate, weight, or dither differently. Two invariants a
future change must keep:

- **All four border bands are blurred before any is mixed back.** A band's
  neighbourhood reaches into the bands beside it, and what it must read there is
  the backdrop, not the frost of it.
- **`blur_span` confines its source to the line before walking it.** The walk
  reads a clamped edge by letting a strided iterator run out, which breaks the
  moment several bands share one max-sized scratch — a band then reads a
  neighbour's pixels as its replicated edge.

**The layers a frost covers are no longer composed** (`compose_plane`,
`frost_spared`): a frost is copied over whatever is beneath it, so composing that
stack first is work the copy throws away. A frame composes below a frost only
outside what the frost will write — nothing under one reused whole, and only the
ring the border blur *reads* under one reused in part — as the disjoint
rectangles `Region::subtract` gives, never the box around them. `frost_spared`
and `frost_segment` both consult the cache with only a composite in between, so a
missing frost composes the plane and blurs in full.

### D.9 Any window that reads its backdrop retains one
A frost is a cache of the composed plane, and a *plainly translucent* window had
none, so every pointer sample of its drag re-blended the whole stack beneath it.
A second cache for the unblurred plane was **not** needed and would have been
duplication (§2.2): **a blur of radius zero leaves the composed layers exactly as
it found them**, so the retained entry already *is* the composed backdrop and the
whole retention path applies unchanged. All that was missing was admission — one
predicate, `Window::reads_backdrop` (a blur, or a whole-window opacity below
full), replacing the `blur_radius() == 0` gates in `compose_plan` and
`recompose_rect`. `blur_px` is gated on `radius > 0`, so a radius-zero frost is
not reported as blur work.

Deliberately excluded, because their backdrop is not a field: an antialiased
corner (a few pixels of arc) and a client painting alpha into its own content
(unknowable without reading every pixel). Both still composite correctly through
the blend path.

### D.10 A window and the menu it owns are one thing to restack
`Window` carries `parent: Option<WindowId>` — the window it is a *transient* of.
`Compositor::add_transient_window(parent, origin, surface)` records it and
inserts the popup directly above its owner and any transient already there,
refusing an unknown owner (fail closed). `raise` and `lower` move the **family**
— owner immediately below its transients — whichever member is named, through
one private `restack_family`, so nothing can be raised between the two: the
invariant a per-frame re-assert used to protect is now held by construction. A
family already at the end it is being moved to is left completely alone (the
settled check is a count and two slice reads, and allocates nothing).
`SessionWindows::keep_popups_stacked` is **deleted**;
`DesktopShell::open_popup_window` takes the owner and returns `Option`; and
`Compositor::remove` clears the transient link of anything the removed window
owned, so no stale link outlives a window.

A deliberate behaviour change comes with it, and is an improvement: the old
two-`raise` idiom pinned an owner topmost for as long as its menu lived. The
family restack keeps the pair glued without pinning it.

**The app half.** `ContextMenu::outcome` reads the region `lib/controls`' `Menu`
fills, answering `Ignored` when nothing was reported, so a sample inside the
highlighted row costs no render, no frame copy and no present, while crossing
into another row still repaints. `Settings::on_pointer` has the same boundary
fixed the same way. A round that reported *something* still repaints the whole
plate, deliberately: a change the sheet composes above its controls (a switched
tab's body) is wider than the rectangle the control that caused it reports.
Per-rectangle *presenting* of a plate needs a retained overlay surface and
belongs with E.2's rect-list present.

### D.11 The desktop layer repaints the icons that changed, not the screen
The desktop layer is the **bottom** of the stack, so marking all of it
recomposites every window above it and drops every frosted backdrop over it.
`DesktopOutcome::redraw` is **deleted**: `set_focused`, `pointer_moved`,
`pointer_left`, `press`, `context_press` and `key` each take a
`tairix_geometry::Region` sink and add the *cell rectangle* of every icon whose
appearance changed — hover left and hover taken, old selection and new, the
selected icon whose focus ring appeared or disappeared. One private
`Desktop::mark_cell` spells that rule once, and `IconTile::render` draws strictly
inside its cell, so the cell is the whole of repainting the icon. A gesture that
changes nothing visible adds nothing and composes **no frame**.

`Compositor::repaint_desktop(area, paint)` hands the painter the rectangles of
`area` clipped to the layer and marks exactly those; a freshly allocated layer is
still painted whole, holding no pixels a partial paint could preserve.
`DesktopShell::present_desktop_area` paints each rectangle under a narrowed
surface clip, and `present_desktop` is now the whole-screen case of the same call
— kept for the changes that genuinely alter the whole layer: bring-up, a new
wallpaper, a theme switch, adopted settings, and a re-list that moved the icons
(which is why a re-list reports `relisted` rather than cells).

**A latent rendering defect closed with it:** the painter used to skip the
backdrop fill when the wallpaper surface was screen-sized, but `lib/sandbox`
leaves a letterboxed or centred placement's margins *fully transparent* on
purpose, so those margins showed the root fill on the first paint and stale
pixels afterwards. The backdrop colour is now laid down first and the wallpaper
composited over it, which is also what makes a partial repaint total.

### D.12 A restack marks where it crossed, not what it moved
**Reordering two windows that do not overlap changes no pixel** — nothing is
drawn differently and no frost sees a different backdrop, so there is nothing to
mark. `restack_family` asks `crossed_bounds` for the windows the family actually
swaps sides with (those above it when moving to the front, below it when moving
to the back, visible only) and marks each moved member's bounds **intersected**
with each of them. Windows on the far side keep their relative order with the
family and so see exactly the stack they always did.

This matters because the taskbar sits above every application window, so an app
is essentially never frontmost: the raise that brings a family forward always
crosses the bar, and the bar's own `keep_topmost` re-assert crosses back. Both
crossings used to mark a large translucent window in full and drop its frost.

---

## Stage E — One present per frame, and a frame deadline  **[done]**

Touches the display wire protocol, so it must be one evolution with
`plans/FIX-DISPLAY-ACCELERATION.md` Stage B, not a second shape (§2.2, §2.13).

### E.1 Keep the damage region disjoint  **[done in C.0]**
The damage region is `tairix_geometry::Region`, whose rectangles are disjoint and
band-canonical, so a scattered frame stays scattered rather than coalescing to
unions. E.2 carries that to the driver.

### E.2 One present per frame, carrying a list of rects  **[done]**
A frame is presented **once**, naming every disjoint rectangle it changed.
`Display::present_rects(&[DamageRect])` is the one damage-aware present — there
is no per-rectangle entry point beside it — and the `DISPLAY_ENDPOINT` `Present`
request carries a fixed-width, self-validating `DamageList` of up to
`MAX_DAMAGE_RECTS` rectangles, the one wire shape
`plans/FIX-DISPLAY-ACCELERATION.md` Stage B extends. The invariants later work
must keep:

- **The ring rotates once per frame.** `RemoteDisplay` holds each frame's
  outstanding damage as a disjoint `Region`, so a buffer catching up copies the
  rectangles it missed rather than one box spanning them. The region's budget is
  *derived* — ring depth × `MAX_DAMAGE_RECTS` — so a double-buffered desktop's
  scattered catch-up never degrades to that box. A buffer is still wholly
  current after its present, which is what lets a driver scan it out in full.
- **Covering the screen and spanning it are different questions.**
  `tairix_display::damage_list` is the single place that chooses between the
  rectangle list, its bounding box (past the bound) and the whole-frame present.
  Two far-apart corners span the screen while changing a few dozen pixels.
- **The whole list is validated before any pixel is blitted**
  (`DamageRect::validate_list`), so a bad rectangle refuses the present rather
  than leaving the ones before it on screen.
- **`MAX_DAMAGE_RECTS` is a format bound (§24.4), not a capacity**: it is what
  one fixed-width request carries, and a producer holding more rectangles
  presents their bounding box. There is no *per-call* rectangle limit to
  reintroduce — a frame publishes once, so no cost model trades rectangles
  against round trips.

### E.3 One-shot frame pacing in the session  **[done]**
The session composites at most once per frame period however many wakes fed it.
`FramePacer` (`userland/gui/session/src/pace.rs`) is the whole policy: the run
loop asks `admit(now_ns, Compositor::has_damage())` at each of its two present
sites, damage accumulates in the compositor between deadlines, and a held frame
shortens the park through the same `park_within` fold the clock, the reveal, the
lock and the frame report use — so a desktop with nothing held arms nothing
(§17.1, §2.23). The invariants later work must keep:

- **Latency is paid only where a frame would have been wasted.** A frame whose
  period has elapsed is admitted on the wake that produced it, so a click, a
  keystroke, and every interaction slower than the display cost nothing. Only a
  producer outrunning the screen is held.
- **The period is the one the desktop already animates at.**
  `tairix_theme::Timeline::FRAME_NS` is the shortest gap between two frames
  worth drawing, which is the same fact for an animation step and a drag, so
  there is no second frame-period constant (§2.2) and an animated surface is
  never woken for a frame the pacer would refuse. A refresh taken from the mode
  would be an ABI field with no producer; real vsync off the flip signal is
  `plans/FIX-DISPLAY-ACCELERATION.md` Stage E.
- **`admit` holds only what is not yet due**, so the deadline it arms is never
  zero-length and the loop cannot spin between a refusal and its frame.
- **An undamaged frame is never held and never starts the period.** Presenting
  one moves nothing and is what re-reads the counters as idle for A.3's report;
  holding it would suppress that reading and starting the period would put the
  next real frame behind a frame that changed no pixels.
- **The compositor owns the damage, the pacer only the clock**
  (`Compositor::has_damage` is the one answer to whether a composite would
  recompose a pixel), and a clock that jumped backwards admits rather than
  freezing the screen for the length of the jump.
- **The departure fade is deliberately unpaced**: it runs on its own timed park
  with the seat still held, because it is the last thing the session draws and
  must complete before the screen is handed on.

### E.4 Tests + docs  **[done]**
The present-side tests and docs landed with E.2 (one transport call per frame
however scattered; a rectangle-sized catch-up copy; the existing double-buffer
tests unchanged). E.3's are `userland/gui/session/src/pace_tests.rs`: a flood
inside one period costing one composite and a sustained flood no more than one
per period; an idle session and every undamaged frame arming nothing; a held
frame arming exactly the time left and never a zero-length deadline (asserted on
the park value, not on timing); an animation's cadence frames never deferred;
and the clock-jump and long-background paths admitting rather than stalling.
Beside the frame-cost tests, sixteen pointer samples pumped through the real
shell and compositor inside one period — each moving the cursor, so each really
does damage the screen — composite nothing until that deadline.

**Acceptance:** CPU at idle unchanged from parked — the pacer folds
`NO_DEADLINE_NS` through untouched whenever nothing is held.

---

## Stage F — CPU-dispatched raster kernels (`lib/cpuops`)  **[not started]**

This is the honest answer to "can CPU feature detection help?": yes, and it is
the *last* 20%. It may not land before B–C.

### F.0 Prerequisite — correct the `ByBenchmark` axis (decision, §15.7)
`plans/FIX-HARDWARE-FEATURES.md` P3b lists `lib/raster` blit/blend/fill under
`ByBenchmark` and marks it **blocked**, because the bounded microbenchmark
measures over the kernel-only `CpuCycles` counter and raster is userland. That
classification is wrong for the same reason the plan already corrected page-zero
in P3a: a packed-SIMD premultiplied `over` is *unconditionally* faster than four
scalar `div255`s and is bit-identical when the vector form implements the same
rounding — so it is a **capability** decision (`ByPriority`), never a performance
measurement.

The capability axis is already wired to userland: `lib/rt`'s startup delivers the
kernel-folded common `CpuFeatureSet` (`cpu_features()`), and `lib/cpuops` is a
plain `lib/*` crate with no kernel edge. So `ByPriority` selection in
`lib/raster` works **today** — no new kernel mechanism, no ABI change, with the
existing self-verify + fail-closed baseline + pin + audit machinery. **Amend P3b
in place (§2.13)**, confirming with the User before editing that plan.

### F.1 Make the loops vectorisable before reaching for intrinsics
NEON is baseline on `aarch64-unknown-none`, so a large part of the win needs no
dispatch: operate on `chunks_exact` of packed pixels instead of a per-pixel
`Option`-returning sample, hoist the row-constant factors, and let LLVM
vectorise. Measure this step on its own before adding candidates — it may be most
of the win.

### F.2 Candidates, following `lib/pagezero` exactly
Same shape, because it has passed review once: a `build.rs`-emitted per-ISA cfg
(never `cfg(target_arch)` in source, so `cargo xtask cfg-check` stays green), a
portable baseline registered **last** that is always feature-legal, the mandatory
self-verify against that baseline over a fixed size/alignment/alpha vector,
`ByPriority` selection, host fuzzing, and the pin for determinism.

### F.3 Families, in order
1. `blend_span` — one source over a span (the one span composite B.6 routed
   every blended pixel through).
2. `Surface::blit` — src-over-dst row zip.
3. the WM's opaque/blended run loop (B.1).
4. `blur_line`/`blur_span` add/sub/mean — after the D.3 reciprocal, and the home
   for D.6's cache-blocked column variant.
5. `encode_run` — a byte-order shuffle (B.3).
6. `resample` `filter_row`/`write_row` — icon and wallpaper scaling.

All are secret-free and bit-identical, so all are legal on the capability axis
(`plans/FIX-HARDWARE-FEATURES.md` invariant 8). None may be benchmark-selected.

### F.4 What is actually available per target

| Target | User-space vector state | Verdict |
|---|---|---|
| `aarch64` | full `q0`–`q31` + `FPCR`/`FPSR` saved on user trap entry/exit; `d8`–`d15` in the kernel switch | **Green today.** NEON candidates are a pure userland change. |
| `x86_64` | none — no `fxsave`/`xsave` in `kernel/`; the target is a soft-float, SSE-disabled kernel target reused for user PIE bundles | **Blocked on Stage G.** |
| `riscv64` | none found — no `fsd`/`fld` in `trap.s`/`context.s`, no `mstatus.FS` handling | **Blocked on Stage G**, and see G.0 — a suspected defect. |
| `wasm32` | `simd128` not in the baseline | Baseline only. |

### F.5 Tests + docs
- Self-verify vectors per family (sizes, alignments, alpha extremes,
  overlapping/short spans).
- Differential fuzz: candidate vs baseline over random buffers
  (`cargo xtask fuzz`), added to the regression corpus (§19.6).
- The pin makes CI deterministic; the audit records the selection.
- Docs: `lib/raster/README.md` (families and their gates),
  `plans/FIX-HARDWARE-FEATURES.md` P3b corrected, README support matrix.

**Acceptance:** bit-identical output on every candidate, baseline chosen when
features are masked off, measured improvement quoted from the A.2 harness.

---

## Stage G — User-space vector/float enablement (kernel work; User decision)

Not started, and not startable without a decision (§15.7).

### G.0 Two findings, one confirmed and one to confirm
- **x86_64 user space has no FPU/SSE.** `x86_64-unknown-none` is the *kernel's*
  soft-float, SSE-disabled target and is also used to build user-space PIE
  bundles. Userland is not the kernel: it should have SSE2 and hardware float.
  Enabling it needs `fxsave`/`xrstor` (or `xsave`) in the x86_64 trap/switch path
  plus `CR0`/`CR4` setup, then a user-space target feature set. Real kernel work,
  and a decision — not something to slip into a GUI change.
- **riscv64 appears to save no float state at all** — no `fsd`/`fld` in
  `kernel/arch/riscv64/src/trap.s` or `context.s`, and no `mstatus.FS` handling
  found — while `riscv64gc` mandates the D extension and `lib/raster`'s gradient
  path uses `f64`. Either FP traps, or two tasks corrupt each other's float
  registers. **This is a defect noticed by reading (§2.18) and must be confirmed
  and then fixed or explicitly ruled out, regardless of any GUI work**; it is
  tracked as `plans/OPEN-DEFECTS.md` D37 and carries a regression test when the
  fix lands (§7).

### G.1 If approved
Per-port lazy-or-eager FP/vector context save/restore behind the Arch HAL
context-switch slice (§17.2), the user-space target feature floor raised in
`tools/xtask`'s per-image floor (`plans/FIX-HARDWARE-FEATURES.md` P0), Arch-HAL
conformance coverage proving two tasks cannot observe each other's FP state, and
only then the Stage F SSE2/AVX2 candidates. Cross-referenced from
`plans/WIRING.md` and `plans/ARCHSUPPORT.md`.

---

## Stage H — The kernel cost of a popup window  **[done]**

Not a compositor stage, and recorded here because this is where a reader chasing
"opening a menu is slow" arrives: the cost was **below** every stage above it,
which is why a menu drawn inside a window was instant while the same menu in its
own window was not.

`terminal.app` is the only app whose menus are separate **popup windows**
(`files.app`, the pinboard and the switchboard draw theirs into their own
surface), so it alone pays `shm_create` + `shm_grant` in the app and `shm_map` in
the session per open, and an unmap on each side per close. **Every one of those
syscalls re-froze the caller's entire address-space snapshot** — a page-table
walk plus a fresh heap node per resident page (`plans/FIX-KHEAP.md`) — over the
largest address space on the machine, inside non-preemptible syscalls. It is
invisible at QEMU screen sizes because the session's resident set is a fraction
of a 1080p one.

Every path that knows *which* pages it changed now publishes exactly those,
through one pair in `kernel/core/src/syscalls.rs`: `publish_region_mapping`
(reads each page's resolved mapping from the live space) and
`publish_region_teardown` (removes them unconditionally — a re-freeze is a no-op
on a CPU with no published live space, which would leave freed pages
translating). Both fall back to the wholesale re-freeze only when a snapshot
cannot absorb a delta, so the delta is a cost reduction and never a correctness
dependency. `sharedreg::unmap`, `DmaPool::free_at`, `LiveUserSpace::free_dma` and
`DmaAllocFacility::free` report the byte extent they released. The same defect on
the **file-backed fault** path (a whole re-freeze per faulted page, making an
N-page mapping O(N²) to read) and on the stack-growth walk is fixed with it. Only
two callers re-freeze now, both compressed-tier batches that move several pages
at once and report no list: the ramzip warm/cluster restore and the
direct-reclaim compress-out sweep. Detail: `docs/src/architecture/memory.md`.

**What this does not close:** an app-owned popup window still costs the app's
own repaint and the session's decode of it. C.3 makes both cost the rectangle a
round reported rather than the window, but a *newly opened* popup has no prior
frame to differ from and so always costs its whole surface. Moving menus out of
apps altogether is `plans/NEW-MENUS.md`, an architectural change rather than a
performance one now that this is fixed.

---

## Stage I — Compose on every core the machine has  **[done]**

The rows of a dirty rectangle are independent by construction — each writes one
back-buffer row and the scan-out bytes of that row, and reads only immutable
window content — so they are composed in bands across a worker pool, and a
frost's column pieces with them. `lib/parallel` is the engine: the `JobRunner`
contract a pass expresses its independent work through, the one
index-to-element erasure, the one split policy, and the fork-join pool over
`lib/rt` threads whose workers park on a futex and never spin. Detail:
`docs/src/lib/parallel.md`.

- **Where it is installed.** `Compositor::set_job_runner`; the default composes
  on the calling thread, which is what a single-CPU machine, a headless build,
  and a process the kernel would grant no thread all keep. The session sizes the
  pool from the online CPU count it reads through the System Information API —
  never a constant — and states on `stderr` when it was granted fewer threads
  than the machine has cores.
- **Bit-identity, not near-identity.** Each scene is composed twice — once
  whole, once split into bands that run backwards — comparing the scan-out
  frame, the back buffer, and every counted pixel of `FrameStats`; the frost
  does the same over rectangles, radii, coverages and random kept blocks. Each
  band tallies its own work and folds it in once, so a split frame reports
  exactly what a whole one does.
- **What splitting costs.** A rectangle below one band's pixel budget is composed
  on the calling thread with no atomics, so a pointer-motion repaint pays what it
  always did. A frost asks for exactly one piece per participant rather than
  several, because each piece re-primes its sliding window at its own first
  column.
- **Verticals.** The `parallel` role of `threads_qemu_{aarch64,riscv64,x86_64}`
  runs a divided pass through a real multi-worker pool many times over, compares
  every round against the same pass on one thread, dispatches before the workers
  can have reached their loop, allocates on workers inside a nested dispatch, and
  drops the pool to join them.
- **Two related invariants this established** (§2.18): the `lib/rt` global
  allocator takes the runtime's futex `Mutex`, never a spin lock — two threads
  allocating at once would otherwise burn a slice spinning through a critical
  section that maps pages, and on one core without preemption could not progress
  at all. And `Pool::with_workers` waits for every worker to read its starting
  epoch before returning, without which a worker that had not run yet would read
  an already-bumped epoch and park without acknowledging the dispatch.

`lib/controls`' selection frost stays on the calling thread, which is right for a
small rounded plate. Stage F's per-pixel kernels are orthogonal and compose with
the pool.

---

## Stage J — The one whole-window pass above the compositor  **[done]**

Converting an application's presented straight-alpha frame into the compositor's
own window surface, and reporting the pixels that genuinely changed, is not the
app's to shrink: the **app** declares the damage, so a client that repaints
everything makes the desktop convert everything. C.3 is the answer for the two
passes inside the app; it cannot be the answer for this one.

- **One definition, not seven.** The conversion is `tairix_display::winframe`,
  beside the `ChannelOrder` the scan-out path owns: `encode` writes a surface out
  as the straight-alpha bytes a window frame holds, `decode` reads a frame in and
  answers the changed sub-rectangle. The scan-out encoder is deliberately *not*
  reused — the screen is opaque, a window frame is not — so the pair sits beside
  it as `encode_straight` / `decode_straight`.
- **Spread, because it cannot be bounded.** Both directions are row-independent
  and expressed over `lib/parallel`'s `JobRunner`. The session hands the decode
  the compositor's own runner, read back through `Compositor::job_runner`, so the
  conversion and the composite cannot disagree about how wide the machine is. An
  app passes the calling-thread runner: it decides how much it presents, and a
  pool per app would be threads and stacks spent on a pass C.3 removes.
- **Bit-identity under splitting** is proven against `tairix_parallel::Reversed`,
  the one shared order-shuffling runner, so the `unsafe impl` lives once beside
  the trait whose obligations it discharges.
- **Fail closed.** Every index either direction uses is validated before the
  first write — more strictly than the hand-rolled loops were, since a row span
  wider than the stride is refused rather than relied on — so a hostile geometry
  refuses the whole conversion rather than leaving a window half-converted.

---

## What this plan refuses

Stated so a later change cannot quietly take a shortcut:

- **No SIMD before the algorithm.** Stage F may not land before B and C.
- **No performance claim without a number** from Stage A, and never a number
  taken from a dev-profile image — nor from a small case at the harness's default
  budget (A.2).
- **No second blend, raster, or region implementation** "for speed" (§2.2). One
  path, specialised loops.
- **No raising a constant instead of fixing the algorithm** (§2.17) — a bigger
  cache, more frame buffers, or a larger present limit is not a fix.
- **No disabling `overflow-checks`** (§2.9, §2.17). Hoist the arithmetic.
- **No output change without a decision** (invariant 2): approximate blends,
  half-resolution blur, or altered rounding are User decisions with visual
  evidence.
- **No wall-clock threshold as a CI gate** (§7): counters only.
- **No unbounded cache** (§24.1, §26.3): every cache is a reclaim client.

---

## Stage dependencies

| Stage | Content | Depends on | Touches ABI? | Touches kernel? |
|---|---|---|---|---|
| A | build profile, bench harness, frame counters | — | no | no |
| B | opaque runs (occlusion is the same mechanism), dither, segment composite, `encode_run` | A | no | no |
| C | region hoist, control damage, hover routing, per-app damage, text memo, batch shell work | A | no | no |
| D | damage funnels, frost cache/reuse, blur reciprocal, family restack, desktop cells | A, B | no | no |
| E | disjoint region, one present per frame, one-shot pacing | B, C, D | `Present` rect list (with FIX-DISPLAY-ACCELERATION Stage B) | no |
| F | `lib/cpuops` `ByPriority` raster candidates (aarch64 first) | B, C, (D, E), F.0 decision | no | no |
| G | user-space FP/SSE enablement | User decision | target floor | yes |
| H | publish a region's own pages instead of re-freezing the space | — | no | yes |
| I | compose a dirty rectangle's rows in bands across a worker pool | A, B, D | no | no |
| J | one window-frame codec, and the desktop's decode spread across it | A, I | no | no |

A–E are expected to dominate F entirely.

---

## Decisions required (§15.7)

1. **Amend `plans/FIX-HARDWARE-FEATURES.md` P3b** to move the raster families
   from the blocked `ByBenchmark` axis to the unblocked `ByPriority` capability
   axis (F.0). Blocks Stage F.
2. **Half-resolution blur** (D.5): approve or refuse the output change. Blocks
   nothing; D landed without it.
3. **Stage G**: whether to do the x86_64 user-space FPU/SSE kernel work at all,
   and when. Blocks Stage F on x86_64 and riscv64 only.
4. **The riscv64 float-state finding** (G.0, `plans/OPEN-DEFECTS.md` D37) must be
   confirmed and fixed independently of this plan's schedule; it is a correctness
   question, not a GUI decision.
5. **How the frame counters become observable to a guest**, so A.4 can assert
   them: a rate-limited frame-cost record on the system log, a `sysinfo` query,
   or leaving A.3's Switchboard-only routing as it is and dropping A.4's
   counter-bound gate for something the audit trail can already carry. Blocks
   A.4 and nothing else.

---

## Definition of done (whole plan)

- Every stage's code, tests, and docs land together (§7, §13); no stage ships a
  stub, a no-op, or a "later" (§2.19).
- Output is byte-identical to the pre-change reference for every scene
  (invariant 2), proven by the golden-frame tests, except where a User decision
  above explicitly approved a rendering change.
- The QEMU desktop verticals assert work counters, not timings, and the counters
  for hover, drag, blur, and idle are at or below the bounds each stage set.
- Every cache added is a bounded `lib/reclaim` client with an epoch and a
  pressure path (§24.1, §26.3).
- §23 self-review applied: security (client damage rects validated and clipped,
  no ambient authority, no new capability), correctness and multi-arch (no
  `cfg(target_arch)` outside the allow-list, no arch-only copy of shared logic),
  no-compat/no-dead-code (the WM's private `DamageRegion`, `compose_pixel`,
  `DesktopOutcome::redraw` and `keep_popups_stacked` are **deleted**, not left
  beside their replacements), tests/docs.
- Whole-project gate green: `cargo fmt --all`, `cargo xtask ci` (once),
  `cargo xtask fuzz --secs 5` (the run/encode/region/candidate decoders get
  harnesses, §19.6), and `tools/ci/soak.sh both --secs 20`.
