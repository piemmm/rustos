# NEW-MENUS — menus owned by the desktop, not by the app

Status: **partly landed.** M0's open transport decision is **settled** —
*inline bounded rows* — and the first surface built on it, the icon bar's
app-declared menu, is **done** (`plans/NEW-TASKBAR.md` T7). M1 (the
session's general per-seat menu service), M2 (migrating the four in-app
menu shells to it), and M3 (the cached compositor plate) remain planned.

Binding under `AGENTS.md` (§3, §15.18).

**This is an architecture change, not a performance one.** The ~300 ms
context-menu stall that prompted it was a kernel defect and is closed
(`plans/FIX-DESKTOP-SPEEDUP.md` H): a popup window's shared-memory
create/grant/map/unmap each re-froze the caller's whole address-space
snapshot. Menus in their own windows are no longer expensive. What this plan
buys is *correctness of the model* — one menu on the screen at a time, one
implementation of menu behaviour, and menu pixels the compositor owns and can
cache — not milliseconds. Do not justify it on speed.

---

## Read first (§15.18)

- `AGENTS.md` §2.2 (one definition), §2.4 (interface freeze on release),
  §2.13 (evolve in place, no v2), §5.4 (fail closed), §9 (the ABI
  discipline any new request must meet), §17.3 (the desktop is optional
  and the edge into it is one-way), §27 (a foundational primitive is
  complete, not the slice its first caller needs).
- `plans/APPWIN.md` — the app↔window channel this extends, and the
  transient/owner relationship a menu already uses.
- `plans/GUI-CONTROLS-DESIGN.md` — the Reactive Alloy `Menu` control that
  becomes the *server's* renderer rather than each app's.
- `plans/COMPOSITOR-WORK.md` — server-side window furniture: the existing
  precedent for the compositor drawing something on an app's behalf, and
  the chrome cache a menu plate would join.
- `plans/DISPLAY.md` — seat ownership; a menu grab is a seat-scoped input
  state, not a global one.
- `plans/FIX-DESKTOP-SPEEDUP.md` — C.3 (apps presenting real rectangles)
  and D (the frost cache a menu plate must not defeat).
- `plans/NEW-TASKBAR.md`, `plans/PINBOARD.md`, `plans/NEW-FILEMANAGER.md`,
  `plans/GUI-TERMINAL.md` — the four surfaces whose menus migrate.

---

## The defect this closes

Every menu in the system is drawn and driven by the app that owns it, so:

1. **There is no singleton.** Two apps can each have a menu open at once,
   and an app's menu can outlive the click that should have dismissed it,
   because no component owns "the menu that is up". A menu is a modal,
   screen-scoped thing; nothing models it as one.
2. **Menu behaviour is implemented more than once.** `lib/controls`'
   `Menu` is shared, but the *shell* around it — placement against a
   screen edge, the grab, dismissal on an outside press or Escape,
   keyboard traversal into and out of a submenu, and what happens when
   the owner window moves or dies — is re-implemented per app
   (`terminal`'s `ContextMenu`/`Settings` overlays, `files`' in-window
   menu, the pinboard's backdrop menu, the switchboard's task popup).
   Four spellings of one rule is the duplication §2.2 forbids.
3. **The compositor cannot cache what it does not own.** A menu plate is
   ideal cache material — a small, opaque-or-translucent rectangle whose
   pixels change only when the highlighted row changes — but while an app
   owns it, the compositor sees only "some window presented".
4. **An app-drawn menu can be clipped by its own window.** A menu that
   would extend past the app's window has to become a popup window (as
   `terminal` does) or be truncated (as an in-window menu is).

---

## Goal / invariants (bind every stage)

1. **One menu at a time, screen-scoped, owned by the session (§27).**
   Opening a menu closes whatever menu was open, whichever app asked for
   it. The open menu is one piece of state on the seat, not a set.
2. **The app describes, the desktop decides (§5.4).** An app sends a menu
   *model* (rows, enablement, marks, submenus) and an anchor; the session
   places, draws, grabs, routes, dismisses, and answers with *one*
   outcome. An app never learns pointer positions inside the menu, never
   draws a menu pixel, and cannot pin a menu open.
3. **One renderer, one behaviour.** The session drives `lib/controls`'
   `Menu`; no second menu widget, and no per-app menu shell. Migrating an
   app **deletes** its menu code (§2.14).
4. **The request is real ABI (§9).** Versioned, hashed, bounded, decoded
   fail-closed, fuzzed (§19.6), and frozen on the first release. It is
   evolved in place until then (§2.13) — no `v2`, no compatibility shim.
5. **Bounded by construction (§24.4).** Row count, label bytes, submenu
   depth and total model bytes are *format* bounds a hostile client
   cannot exceed, not capacities that grow with the machine.
6. **The headless build is untouched (§17.3).** The menu service lives
   with the session; no `lib/*`, `kernel/*` or non-GUI `userland/*` crate
   gains an edge into it.
7. **A refused menu is an answer, not a death (§2.24).** An app whose menu
   request is refused (no seat, no session, a malformed model) reports the
   refusal and carries on; it never terminates and never falls back to
   drawing its own.
8. **No new capability without a holder (§5.2).** Asking for a menu is
   something any windowed client may do — it is scoped by the window the
   client already owns — so this plan introduces no `CAP_*`.

---

## Stage M0 — the model and the wire — **transport settled; the icon bar's menu done**

**The transport decision is `inline bounded rows`.** The icon-bar model is
small (≤ 12 rows, ≤ 1 submenu level, ≤ 36 label bytes), so a fixed frame is
the simplest thing to validate and to fuzz and adds no mapping to a path
that must be cheap. A granted shared region was the alternative and is
rejected: it would put an `shm_grant` and a map on every menu open for a
payload that fits in the frame the window channel already carries. The
choice binds every menu request, not only the icon bar's.

What is **built** on that decision (`plans/NEW-TASKBAR.md` T7): the
`AppMenu` model in `lib/abi/src/window_ipc.rs` — an ordered, bounded list of
rows, each `Item { id, label, enabled, mark: none/check/radio }`,
`Separator`, `Submenu { label, enabled }` (one level, by parent index), or
`About` (session-rendered from the bundle's signed manifest; the app
supplies none of its text). Ids are the app's, non-zero and unique within a
menu; the session never interprets one. The wire decoder is held to the
**same** shape rule as the builder, so a menu that crossed the wire is
exactly a menu that could have been built, and both are fuzzed in
`lib/abi/tests/fuzz_decode.rs`. It is carried by
`WindowRequest::SetAppBar`, a caller-scoped declaration rather than a
per-open request, because an icon-bar menu belongs to the *application*
rather than to one gesture; the outcome is the application-scoped
`WindowEvent::AppBarMenu { item }`, delivered through the route the
declaration recorded.

What **remains** for a general per-window menu (M1's request): a
per-gesture open carrying an anchor, and the three-way outcome
`Chosen(id)` / `Dismissed` / `Refused(reason)` delivered once to the
requesting window. The row model itself is done and is reused verbatim — a
second menu model would be the duplication §2.2 forbids. Accelerator text
is not in the built model and is added with M1, which is the first surface
that has any.

## Stage M1 — the session's menu service

- One `Option<OpenMenu>` per seat in the session: the owner window, the
  model, the placement, and the `lib/controls` `Menu` driving it.
- **Placement** is the session's: anchor in the owner's coordinates,
  flipped and slid to stay on screen, scaled through
  `tairix_geometry::Scale`. A menu is never clipped by its owner.
- **The grab** routes every pointer and key event to the menu while it is
  up. An outside press dismisses (and is *consumed*, not delivered);
  Escape dismisses; a choice answers. The owner window keeps its active
  look, exactly as an app-owned transient does today.
- **Lifetime**: the menu closes if its owner window closes, loses its
  seat, or the session ends — always answering `Dismissed`, so an app is
  never left waiting.
- The plate is a session-owned surface composited as a transient of the
  owner (the existing `add_transient_window` family restack), so it
  inherits the stacking guarantees already proven there.
- Tests: the singleton rule (a second request closes the first and answers
  it `Dismissed`), placement against all four screen edges, dismissal by
  outside press / Escape / owner death, keyboard traversal including into
  and out of a submenu, and that a hover inside a highlighted row damages
  only the two rows that changed.

## Stage M2 — migrate, and delete

In this order, each app landing complete with its tests and docs:

1. `userland/apps/terminal` — its `ContextMenu` and the menu half of its
   settings sheet, and the popup-window plumbing they use.
2. `userland/gui/session`'s pinboard backdrop menu.
3. `userland/apps/files` — its in-window menus.
4. `userland/gui/switchboard`'s task popup.
5. `userland/gui/taskbar`'s start menu, if its model fits without
   distorting the ABI (it may legitimately stay bespoke — decide with
   evidence, do not force it).

Each migration **deletes** the app's menu shell (§2.14). The final step
removes any menu-shell helper in `lib/controls` that no longer has two
consumers (§15.5).

## Stage M3 — what the compositor can then do

Only after M2, because it is meaningless while apps own menu pixels: the
menu plate becomes a cached, damage-reporting surface like the window
furniture (`plans/COMPOSITOR-WORK.md`), so moving the highlight repaints
two rows rather than the plate.

---

## Decisions required (§15.7)

1. ~~**M0's transport**: inline bounded rows, or a granted shared region.~~
   **Settled: inline bounded rows** (see M0), and built on.
2. **Whether the taskbar's start menu is in scope** (M2 step 5), or stays
   a bespoke surface because its rows are launcher entries with icons
   rather than a menu model.

---

## Definition of done (whole plan)

- One menu can be open at a time, system-wide, and no app can draw one.
- Every migrated app's own menu code is **deleted**, not left beside the
  new path (§2.14), and no second menu shell exists anywhere (§2.2).
- The request is bounded, validated, fail-closed and fuzzed (§5.4, §19.6),
  and adds no capability (§5.2).
- The headless build still builds and runs with every `userland/gui/*`
  crate excluded (§17.3), and `cargo xtask deps-check` stays green.
- Docs land with the code: `docs/src/desktop/` gains the menu-service
  page, and every migrated app's page loses its menu description (§13).
- Whole-project gate green (§7): `cargo fmt --all`, one `cargo xtask ci`,
  `cargo xtask fuzz --secs 5`, `tools/ci/soak.sh both --secs 20`.
