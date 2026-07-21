# COMPOSITOR-WORK.md — Window decorations in the compositor (server-side furniture)

This is the staged build plan for giving every windowed app real window
decorations — a title bar with Close / Minimize / PutToBack / SizeToggle
controls, an active/inactive frame rim, and a resize grabber — by rendering
them in the **window manager**, not in any app. It is **binding under
`AGENTS.md`** — read `AGENTS.md`, `PLAN.md` Stage 7, `plans/GUI-CONTROLS-DESIGN.md`
(the control/furniture design this consumes), `plans/APPWIN.md` (the window
channel + WM-presented windows this builds on), and `plans/DISPLAY.md` (the
seat/display model beneath it) first; every rule in all of them applies here
without exception.

## 0. Why this work exists (findings, binding for this plan)

- **Decorations are the window manager's job, not the app's.** The design is
  explicit and server-side: the WM owns outer window-frame and furniture
  rendering, hit testing, pointer capture, move/resize behaviour, stacking
  actions, minimization, and size-state transitions. Applications provide
  typed metadata and receive typed events through the existing window path;
  they never paint over or intercept window-manager chrome, and the WM keeps a
  **separate furniture hit map** so an app can never impersonate a real frame
  control (`plans/GUI-CONTROLS-DESIGN.md` §1, §11.17/§11.18, §424, §1189).
  The Files app is therefore **correct as-is** — it draws only its browse
  content. It has no decorations because nothing draws them yet.
- **The furniture family already exists, unconsumed.** `lib/controls::window`
  provides the complete, tested family — `WindowFrame`, `TitleBar`,
  `WindowControl` (kinds Close/Minimize/PutToBack/SizeToggle), `ResizeGrabber`,
  `ScrollCorner` — with `layout`/`render`/`hit`/`on_pointer`/`on_key` and typed
  events (`WindowControlAction`, `TitleBarEvent`, `ResizeEvent`,
  `FurniturePart`). It is a *reference composition*: no WM consumes it.
- **The WM composites content only.** `userland/gui/wm` blits per-window
  content `Surface`s with rounded corners (`corner.rs`), damage tracking
  (`damage.rs`), a cursor overlay (`cursor.rs`), click-to-activate + move-grab
  (`input.rs`), and the **root-viewport scrollbar** furniture (`viewport.rs`:
  `RootViewport`, `FurnitureLayout`, `FurnitureHit`, `hit_test`). There is **no**
  `WindowFrame`/`TitleBar` composition. `userland/gui/wm/Cargo.toml` does not
  depend on `tairix-controls`.
- **The title already flows over the channel.** `lib/window` carries
  `WindowTitle` on `Create`, and `lib/abi::window_ipc::WindowEvent` already has
  `CloseRequested { window_id }`. Today the title is consumed only as the
  taskbar label; nothing renders it as a title bar.

The `viewport.rs` root-viewport scrollbar is the exact precedent to follow:
reserve a stable furniture gutter, shrink the client rect, keep a furniture hit
map so a furniture press is never `FurnitureHit::Client`, and route furniture
input to typed actions. This plan extends that pattern from the inner scrollbar
to the outer frame.

## 1. Guiding rules (do not violate)

- **Nothing here is deferred, stubbed, no-opped, or "for now."** Every stage
  lands *complete* (`AGENTS.md` §2.19, §27): a "title bar today, resize later"
  split is exactly the deferral the charter forbids. If a stage genuinely
  depends on prerequisite work, that prerequisite is part of the same change or
  the conflict is raised with the User (§15.7).
- **Wire it once, in the WM; touch no app crate.** Because decorations are
  server-side, every app (Files, Terminal, Viewer, and any future Switchboard)
  gets decorated by the WM composing the furniture around each window. Adding a
  per-app decoration path, or letting an app draw its own title bar, is a
  design violation (`plans/GUI-CONTROLS-DESIGN.md` §1, §424).
- **No second visual recipe or constant (§2.2).** Frame/title metrics, palette
  roles, and motion come from `lib/theme`; drawing goes through the one
  `lib/raster` path already used for rounded corners; the rounded-corner math
  is the existing `corner.rs`/`round_rect_coverage` path — no new recipe. The
  furniture geometry is `lib/controls::window`'s `layout`/`hit`, not a
  reimplementation in the WM.
- **No new syscall, no ambient authority (§4, §5.4).** Cooperative
  close/minimize/put-to-back/size-toggle ride the **existing** window path as
  typed `WindowEvent`s; the WM validates the event targets a live window owned
  by the addressed client. Force-quit stays the separate capability-checked
  recovery path — it is not a title-bar button.
- **Headless stays first-class (§17.3).** `userland/gui/wm` is a
  `userland/gui/*` crate; the one-way dependency edge holds and a headless
  build simply excludes it. No non-GUI crate gains a `tairix-controls`/GUI edge.
- **Client can never reach the furniture (§424, §1189).** The client content
  surface never overlaps, clips, or receives input from the frame, title bar,
  controls, or grabber; a furniture press is furniture in the hit map, never
  `FurnitureHit::Client`.

## 2. Stages

**Status:** Stages A–C are **done**; Stages D–E are **planned** (not started).
Per the User's direction, one full stage lands per change.

Each stage lands complete — its rendering for **both** dark and light themes,
reduced-motion and high-contrast behaviour, its pointer/keyboard/focus paths,
and its `#[cfg(test)]` tests — before the next begins.

### Stage A — WM depends on `lib/controls`; frame layout + reserved client rect — DONE

The window-manager geometry foundation for decorated windows is complete and
tree-green. What it now guarantees:

- `userland/gui/wm/Cargo.toml` depends on `tairix-controls`; the crate root
  re-exports the furniture family it will compose.
- `WindowFrame` exposes the single outer↔client derivation both directions:
  `FrameInsets`, `WindowFrame::insets()`, and `WindowFrame::outer_for_client()`
  (the inverse of `layout`). `layout` and `insets` share one `edges()` metric
  helper, so the frame band has exactly one definition (no §2.2 duplication).
- `Window` holds an opt-in `Option<WindowFrame>` (mirroring the existing
  `Option<RootViewport>` precedent). `bounds()` returns the outer rectangle for
  a decorated window; `client_rect()`/`frame()` expose the inset client and the
  frame; content sampling maps outer-local→content coordinates so the reserved
  band shows the background and the client never overlaps furniture. The
  undecorated path is byte-identical.
- `Compositor` owns the active `Theme` (`theme()`/`set_theme()`), offers
  `set_window_frame`/`clear_window_frame`/`window_frame`/`window_client_rect`,
  and re-resolves every window's band on scale or theme change.
- Tests cover insets/`outer_for_client` round-trips at reference and scaled DPI
  under both themes, plus WM outer-band reservation, client-vs-background on
  composite, clear-reverts, rescale-grows-band, theme-switch, and
  undecorated-unchanged.

No furniture is rendered or hit-tested yet, no ABI was touched, and no window
opts into a frame in the running desktop — so there is no behavioural or visual
change yet. That is Stage B onward.

### Stage B — Compose and render the furniture — DONE

The furniture chrome is rendered around every decorated window. What it now
guarantees:

- Each decorated `Window` keeps a pre-rendered, outer-sized decoration
  `Surface` (`window.rs::render_decoration`), painted through
  `WindowFrame::render`/`TitleBar::render` (rim, body, the sanitised title via
  `lib/font`, the four `WindowControl` buttons) plus a corner `ResizeGrabber`,
  using the one `lib/raster` fill and the shared rounded-corner path (no second
  recipe). The rim's rounded corners stay transparent so the desktop shows
  through.
- `Window::sample_local` samples that decoration in the reserved band and the
  inset client content inside it, so both the software composite and the
  hardware-accelerated `encode_layers` path draw the furniture identically; the
  client never overlaps the band.
- The title the WM receives on the channel (`WindowTitle`) is rendered in the
  title bar via `Compositor::set_window_title`, not merely used as the taskbar
  label.
- Active/inactive rim treatment follows the focused window through
  `Compositor::set_active_frame`; attention requests are preserved rather than
  clobbered by a focus change.
- A focus change or title edit repaints only the furniture bands
  (`Window::furniture_bands`/`title_band`), never the client — damage stays
  confined to the furniture.
- Tests cover dark and light theme render, active vs inactive rim, the title
  being drawn, reduced-motion pixel-identical render, high-contrast glyph
  thickening, and furniture-confined damage on a focus flip and a title edit.

No furniture is hit-tested or wired to a lifecycle action yet, and no ABI was
touched — that is Stage C onward.

### Stage C — Furniture hit map + pointer/keyboard routing — DONE

The window manager classifies and routes every frame-furniture interaction to
typed outcomes, entirely inside `userland/gui/wm`. What it now guarantees:

- `Compositor::frame_hit` classifies a screen point against a decorated
  window's `WindowFrame::hit` (→ `FurniturePart`); `input.rs` `press_primary`
  consults it first (then the root-viewport `hit_test`), so frame furniture and
  scrollbar furniture share one press-classification step and a frame press is
  never `Activated`/delivered to the client.
- A title-bar press begins the existing move-grab (`begin_move` → `Moved`/
  `MoveEnded`); a resize-edge press begins a resize-grab that drives the shared
  `ResizeGrabber` (`ResizeEvent`), recomputes the clamped outer rectangle per
  edge (minimum-client floor `MIN_CLIENT_W`/`H`), and applies it through
  `Compositor::resize_window` (content surface reallocated, pixels preserved,
  origin + decoration following), reporting `Resized`/`ResizeEnded`; Escape
  cancels and restores the pre-drag geometry exactly.
- A command-control press captures the frame (`control_grab`), feeds the click
  to `TitleBar::on_pointer`, and emits `InputResponse::WindowControl { window,
  control }` on the completed release. Keyboard control activation routes
  through `Compositor::frame_key` → `TitleBar::on_key` (arrows move focus,
  Space/Enter activate) when the frame furniture holds the keyboard; a control
  press claims that focus and a client press returns it, so a decorated
  window's content keeps its keys until the user reaches for the furniture.
- The furniture press/keyboard repaint marks only the furniture bands dirty
  (never the client), and the resize corner is reserved clear of the scrollbar
  tracks/thumbs (`plans/GUI-CONTROLS-DESIGN.md` §1218) — asserted.
- Tests cover each furniture region hit-test and its exclusion from the client,
  title-bar drag→move, corner resize grow, resize clamp-to-minimum + Escape
  restore, pointer and keyboard control activation, client-press keyboard
  return, and resize corner ∩ scrollbar track = ∅.

No served window opts into a frame in the running desktop yet, so — as with
Stages A–B — there is no behavioural change in the live session; wiring the
typed outcomes to the window lifecycle over the channel is Stage D, and turning
decorations on is Stage E.

### Stage D — Typed control actions → window lifecycle

- Map `WindowControlAction` to lifecycle:
  - **Close** → deliver `WindowEvent::CloseRequested { window_id }` (already in
    the ABI) to the owning client over the existing window path; the client
    tears down cooperatively (§1039 — no new syscall). The WM validates the
    window is live and owned by the addressed client (§955).
  - **Minimize** → the WM minimizes (hide + taskbar `TaskVisibility`), delivered
    as a typed `WindowEvent` so the client can react; add the ABI event variant
    if one is not already present, under the ABI discipline (versioned/hashed —
    the ABI is unfrozen pre-release, §2.13/§9).
  - **PutToBack** → WM restacks (z-order), a WM-local action.
  - **SizeToggle** → WM size-state transition (normal ↔ maximized), delivered as
    a typed resize/size-state `WindowEvent` so the client re-lays-out.
- Force-quit is **not** a title-bar control — it remains the separate
  capability-checked recovery path.
- Session glue (`userland/gui/session`): `SessionWindows`/`WindowServer` present
  decorated windows exactly as today; confirm minimize/restack/size-toggle flow
  through the session's present path.
- Tests: Close delivers `CloseRequested` only to the owning client; a control
  action targeting a foreign/dead window is rejected; minimize/put-to-back/
  size-toggle change WM state and emit the right typed events.

### Stage E — Complete, documented, gated

- Rustdoc on every new public item; add/update the `docs/src/desktop/` page
  describing server-side decorations (who owns furniture, the hit map, the
  typed lifecycle events) (§13).
- QEMU app vertical: extend an existing desktop/app vertical (as in
  `plans/APPWIN.md`) to assert a presented window is decorated and that Close/
  Minimize/PutToBack/SizeToggle behave end to end.
- Update `PLAN.md` Stage 7 to record decorations as done, and this plan's
  status. Register this plan in the `AGENTS.md` §15.18 jump-sheet (done in the
  same change that creates it).
- The whole validation gate is green before "done" (`AGENTS.md` §2.15, §7):
  `cargo fmt --all` (+ `--check`), `cargo xtask ci` (once), `cargo xtask fuzz
  --secs 5`, and `tools/ci/soak.sh both --secs 20`.

## 3. Definition of done

- Files — and every other windowed app — is drawn with a title bar
  (title + Close/Minimize/PutToBack/SizeToggle), an active/inactive frame rim,
  and a resize grabber, **without any change to an app crate**.
- All furniture is rendered and hit-tested by the WM via `lib/controls::window`;
  the client can neither draw over nor receive input from it.
- Close/Minimize/PutToBack/SizeToggle work cooperatively over the existing
  window path with no new privileged syscall and no ambient authority.
- Dark + light themes, reduced-motion, and high-contrast are all covered;
  damage is confined to furniture on state changes.
- Headless build unaffected; §17.4 layering intact.
- Docs updated and the whole-project gate green (§2.15, §7).
