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

Each stage lands complete — its rendering for **both** dark and light themes,
reduced-motion and high-contrast behaviour, its pointer/keyboard/focus paths,
and its `#[cfg(test)]` tests — before the next begins.

### Stage A — WM depends on `lib/controls`; frame layout + reserved client rect

- Add `tairix-controls` to `userland/gui/wm/Cargo.toml` (the WM already depends
  on `tairix-theme`/`tairix-raster`/`tairix-geometry`; this adds the furniture
  family it will compose).
- Give each `Window` an owned `WindowFrame` (from `lib/controls::window`),
  seeded with the window's `WindowFurnitureState` and its title. Extend
  `window.rs` (`Window`/`Window::new`) to hold it.
- Compute, per window, the outer frame rect and the **reserved** inner client
  rect from `WindowFrame::layout(...)` at the active `Scale`/`Theme`, exactly as
  `viewport.rs` reserves the scrollbar gutter and shrinks the client. The app's
  content `Surface` is presented **inside** the client rect and never overlaps
  furniture.
- Tests: layout reserves the title-bar/border extents at reference and scaled
  DPI; the client rect never intersects the frame rim, title bar, controls, or
  grabber corner (mirror the `viewport.rs` gutter/corner tests).

### Stage B — Compose and render the furniture

- In the compositor draw path (`compositor.rs`), after (or around) the content
  blit, render `WindowFrame` (rim), its `TitleBar` (title text via `lib/font`
  through the shared path, plus the four `WindowControl` buttons), and the
  `ResizeGrabber` — all through `WindowFrame::render`/`TitleBar::render`, using
  the one `lib/raster` fill and the existing `corner.rs` rounded-corner path
  (no second recipe, §2.2).
- Render the title the WM already receives on the channel (`WindowTitle`) —
  render it, do not merely label the taskbar with it.
- Active/inactive rim treatment is driven by the focused-window state the
  `InputRouter` already tracks; keep it in sync via `set_active_frame`.
- Damage: a focus change, title change, or control state change marks only the
  furniture bands dirty (reuse `damage.rs`), never a full-window repaint.
- Tests: dark and light theme render; active vs inactive rim; reduced-motion
  and high-contrast variants; damage is confined to furniture on a focus flip.

### Stage C — Furniture hit map + pointer/keyboard routing

- Extend the WM furniture hit testing so a press on the title bar, a control,
  or the grabber is classified by `WindowFrame::hit` (→ `FurniturePart`) and is
  never `FurnitureHit::Client`. Fold this into the existing `input.rs`
  `press_primary` furniture branch alongside the root-viewport `hit_test`, so
  frame furniture and scrollbar furniture share one classification step.
- A title-bar drag continues the existing move-grab (`begin_move`/`Moved`/
  `MoveEnded`); a grabber drag drives resize (new `InputResponse` variant,
  e.g. `Resized`/`ResizeEnded`, mirroring the move-grab lifecycle) via
  `ResizeGrabber::on_pointer` → `ResizeEvent`.
- The `WindowControl` buttons emit `WindowControlAction`; keyboard activation
  routes through `TitleBar::on_key`/`WindowControl::on_key`.
- The resize corner must never overlap either scrollbar thumb — assert it, as
  the design requires (`plans/GUI-CONTROLS-DESIGN.md` §1218).
- Tests: each furniture region hit-tests correctly and is excluded from the
  client; title-bar drag moves, grabber drag resizes; resize corner ∩ scrollbar
  = ∅; keyboard reaches the controls.

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
