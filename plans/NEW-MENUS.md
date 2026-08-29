# NEW-MENUS — every menu is the desktop's: one chain, one grab, one renderer

Binding under `AGENTS.md` (§3, §15.18).

## Ledger

| Stage | What | State |
|---|---|---|
| M0 | Row model (`AppMenu`) + the icon-bar declaration carrying it | **landed** |
| M1a | Variable-length request framing: per-op wire length, exact-length decode | **landed** |
| M1b | Model: per-plate rows, submenu depth, titles, shortcuts, `About`→`Info` | next |
| M1c | The per-gesture open: anchor in, `Chosen`/`Dismissed`/`Refused` out | planned |
| M1d | Attached-window row kind + its present/arrive/refuse events | planned |
| M2 | The service: bands, drag, arrival-open, attach/detach, grab, lifetime | planned |
| M3.1 | Migrate `userland/apps/terminal`, delete its shell | planned |
| M3.2 | Migrate the pinboard backdrop menu, delete its shell | planned |
| M3.3 | Migrate `userland/apps/files`, delete its shell | planned (decision 2) |
| M3.4 | Migrate the bar's `menu.rs`/`clock_menu.rs`/`system.rs` | planned |
| M3.5 | The bar's start menu, if its rows fit the model | undecided (decision 3) |
| M4 | Plate as a cached damage-reporting surface | planned |

Open decisions: 2 (binds M3.3), 3 (binds M3.5). Decision 1 is **settled** —
variable-length framing, landed as M1a.

### Defects found, to fix in the stage named

Found while doing the work above; each is owned here until it lands, and the
stage that closes it carries its regression test (§2.18, §7).

| # | Defect | Fix in |
|---|---|---|
| D1 | `WindowRequest::from_bytes` accepted a frame longer than the op needs, checking only that the tail was zero. Exact-length framing refuses any trailing byte instead. | **closed in M1a** |
| D2 | The per-op operand-block end offsets were spelled twice — once implicitly by the encoder, once as a literal in the decoder's `reserved_zero(bytes, 36)` call. One named length per op now serves both (§2.2). | **closed in M1a** |
| D3 | Exact-length decoding refuses a random-length input before it reaches any operand, so the request decoders' fuzz coverage came to rest on the seeded frames — and only `SetAppBar` had one. `fuzz_decode` now seeds and bit-flips one frame per operation, at its length and one byte either side. | **closed in M1a** |
| D4 | Menu-child placement exists twice with two rules: `lib/controls`' `Menu::anchored_rect` slides a plate onto the screen, while the bar's own `child_rect` flips a child to its parent's other side. A root at a pointer and a child beside its parent legitimately differ, but the two rules must end up as one owner's (§2.2) rather than one shared and one private to the bar. | M2 |
| D5 | `AppMenu::push_under` refuses a submenu inside a submenu, so the model cannot express a chain at all — the one-level bound is load-bearing in the builder, not only in the renderer. | M1b |

**This is an architecture change, not a performance one.** The ~300 ms
context-menu stall that first prompted it was a kernel defect and is closed
(`plans/FIX-DESKTOP-SPEEDUP.md` H). Menus in their own windows are no longer
expensive. What this plan buys is *correctness of the model* — one menu on
the screen at a time, one implementation of menu behaviour, one place a
submenu's rules live — not milliseconds. Do not justify it on speed.

---

## Read first (§15.18)

- `AGENTS.md` §2.2 (one definition), §2.13 (evolve in place, no v2), §5.4
  (fail closed), §9 (the ABI discipline a request must meet), §17.3 (the
  desktop is optional; the edge into it is one-way), §24.4 (a format bound
  is not a capacity), §27 (a foundational primitive is complete, not the
  slice its first caller needs).
- `plans/APPWIN.md` — the window channel this extends, and the
  transient/owner relationship a plate and an attached window both use.
- `plans/COMPOSITOR-WORK.md` — server-side furniture. `lib/controls::window`'s
  `TitleBar` and the compositor's `WindowChrome` strip cache are what a menu's
  title band and its drag gesture are *made of*, not something to write again.
- `plans/GUI-CONTROLS-DESIGN.md` — the `Menu`/`MenuItem` control that becomes
  the service's renderer rather than each app's.
- `plans/DISPLAY.md` — seat ownership. The open chain is seat-scoped state.
- `plans/FIX-DESKTOP.md` — nothing in the open path may block on an app.
- `plans/NEW-TASKBAR.md` (T7), `plans/PINBOARD.md`,
  `plans/NEW-FILEMANAGER.md`, `plans/GUI-TERMINAL.md` — the surfaces whose
  menus this absorbs.

---

## 1. What a menu is

A menu is a **chain of session-owned plates**, not a window an app draws.

- **A plate** is one column of rows plus a **title band**: a centred text
  title and nothing else — no close, minimize, put-to-back or size-toggle
  control, no resize edge. The band is the plate's drag handle.
- **A chain** is a root plate and the descendants open beneath it. Each
  child is placed edge-adjacent to its parent at its parent row's top,
  flipped to the parent's other side when the screen edge leaves no room,
  and slid vertically to stay on screen.
- **A child is one of two things**: a **submenu** (more rows, from the same
  model) or an **attached window** (a surface: the session's own info panel,
  or one the owning app presents). Both hang where a submenu hangs and both
  obey the chain's lifetime. They differ in exactly one rule, below.
- **The chain is the seat's singleton.** One chain per seat, whoever asked
  for it. Opening a menu closes the chain that was open and answers its
  requester `Dismissed`.

### 1.1 The title band

Every plate carries one, and it is `lib/controls::window`'s `TitleBar` with
an **empty command set**, never a second title-bar control (§2.2). Two
properties follow from that emptiness rather than from new knobs: with no
command clusters the drag span is the whole band, and with no leading cluster
to justify against the title centres. The gesture is the one already
compared and tested — press, drag threshold, `TitleBarEvent::DragBegin` /
`DragMoved` / `DragEnd` — and the title text goes through the same
untrusted-label bounding a window title does.

Titles are not new wire fields where they can be derived:

- A **submenu's** title is its parent row's label.
- The **icon-bar menu's** root title is the application's name from its
  **signed** manifest — the same attested identity its info panel states, so
  a menu cannot be titled as an application it is not.
- A **per-window menu's** root title is the app's, bounded and sanitised
  exactly as its row labels are: a name, not a credential.

### 1.2 Dragging

A press on any plate's band moves **that plate and its descendants**;
ancestors stay put. Dragging pins the plate — its placement stops being
derived from the anchor — and its children re-place relative to their parent
row as usual.

Dragging is **not** detaching. A dragged chain is still the seat's one
chain, still holds the grab, and still closes on an outside press. Nothing
an app can send pins a menu open; the only thing that moves a plate is the
user's own drag.

### 1.3 Submenus open on arrival, and there is no timer

A submenu opens when the pointer **arrives on** its parent row — no click,
no hover delay, no timer. Two placement and closing rules make that
deterministic without one:

- A child plate is **edge-adjacent** to its parent, so there is no dead gap
  for the pointer to cross and no diagonal-travel dead zone.
- An open child closes when the pointer **settles on a different row of the
  same parent plate**, or when the chain dismisses — *never* merely because
  the pointer left the parent row's rectangle. Travelling from a parent row
  into its own child therefore cannot close what it is travelling to.

A disabled submenu row opens nothing (fail closed).

### 1.4 Attached windows, and the one rule that makes them different

An **attached window** is a surface hanging where a submenu would hang. It
is the general form of the info panel, and it is the one place an app's own
pixels enter a chain.

- **Attached** it lives and dies with the chain: it closes when the pointer
  settles on another row of its parent, when the chain dismisses, or when
  the chain's owner dies. It wears the plate title band, so it reads as part
  of the chain.
- **Clicking its row detaches it.** The window becomes an ordinary top-level
  window — the compositor's full `WindowFrame` furniture replaces the menu
  band — and the chain dismisses. A detached window is no longer chain state
  and a later menu does not close it.
- **A real submenu never detaches.** Clicking a submenu row opens or keeps
  its plate; there is no gesture that turns a submenu into a window.

The info panel is the canonical instance and stays **session-drawn from the
signed manifest**. The app declares only that the row exists and supplies
none of the panel's text, so it cannot state an identity that is not its own
inside desktop chrome. That property is already built and is not traded away
for generality: an app-provided attached window is the app's *own* content
in its own client rect, never the identity panel.

An app-provided attached window is bounded, placed, and clipped by the
session (a format bound, §24.4) — it cannot become a full-screen surface
parented to a menu row, and it cannot cover the plates of its own chain.

**Presenting one may not stall the chain (`plans/FIX-DESKTOP.md`).** Arrival
on the row sends the owning app a request to present; the chain stays live
and fully usable while the app answers. A window that arrives after the
pointer has moved on is refused rather than shown, so a slow or hostile app
can neither freeze the menu nor plant a panel under a row the user has left.

### 1.5 The grab

While a chain is up the seat's pointer and keyboard route to it:

- A press **inside** the chain — any plate, any attached window — acts there.
  An attached window's own input is the app's, as any window's is.
- A press **outside** the chain dismisses it and is **consumed**, not
  delivered. A dismissal must not double as a click on whatever was behind
  the menu.
- **Escape** closes the deepest open child; with only the root open it
  dismisses the chain. Repeated Escape therefore always gets the user out,
  and an attached panel's Escape closes that panel first, which is what a
  panel with a field in it should do.
- **Keyboard traversal** is the service's: Up/Down within a plate, Home/End
  to its ends, Right into the highlighted row's child, Left back out of it,
  Enter/Space to activate.
- The owner window keeps its active look throughout, as an app-owned
  transient does today.
- A **mode change under the gesture** — the seat's output resized, the UI
  scale or theme switched — dismisses the chain rather than re-placing it;
  re-placing a plate the user has dragged is not defined.

### 1.6 One service, one model, in the session process

`userland/gui/session` is the desktop server: it links the compositor
(`tairix-wm`), the bar (`tairix-taskbar`), and the window server
(`tairix-window`), and owns the seat pump. The service lives there, and the
chain's plates are composited as transients of the owner through the
compositor's existing `add_transient_window` restack — inheriting the
stacking guarantees already proven there — and retained in its chrome-strip
cache.

**The desktop's own menus are clients of the service like any app's.** The
backdrop menu, the bar's application/clock/system menus, the file manager's
context menu, and the terminal's are all *models handed to the one service*;
none of them keeps a shell. The only difference is where a model comes from:
built in-process, or decoded from the wire.

That means one service-facing model type, with the wire model decoding
**into** it. The wire model is a bounded subset of what the service renders —
never a second model with a second set of behaviours (§2.2).

---

## 2. Invariants (bind every stage)

1. **One chain per seat, owned by the session (§27).** The open chain is one
   piece of seat state, not a set.
2. **The app describes; the desktop decides (§5.4).** An app sends a model
   and an anchor. The session titles, places, draws, grabs, routes,
   dismisses, and answers with *one* outcome. An app never learns pointer
   positions inside a menu, never draws a plate pixel, and cannot pin a
   chain open.
3. **One renderer, one behaviour.** The service drives `lib/controls`' `Menu`
   for rows and `TitleBar` for bands. No second menu widget, no second
   title-bar control, no per-app menu shell. Migrating a surface **deletes**
   its menu code (§2.14).
4. **The request is real ABI (§9).** Versioned, hashed, bounded, decoded
   fail-closed, fuzzed (§19.6). Evolved in place until first release
   (§2.13) — no `v2`, no shim.
5. **Bounded by construction (§24.4).** Rows per plate, label bytes, submenu
   depth, attached-window extent and total model bytes are *format* bounds a
   hostile client cannot widen, not capacities that grow with the machine.
6. **No blocking on an app (`plans/FIX-DESKTOP.md`).** No step of opening,
   traversing, or dismissing a chain waits on a client.
7. **Identity stays attested.** The info panel's text is the session's, from
   the signed manifest, always.
8. **The headless build is untouched (§17.3).** The service lives with the
   session; no `lib/*`, `kernel/*`, or non-GUI `userland/*` crate gains an
   edge into it.
9. **A refused menu is an answer, not a death (§2.24).** An app whose
   request is refused reports the refusal and carries on; it never
   terminates and never falls back to drawing its own.
10. **No new capability (§5.2).** Asking for a menu is scoped by the window
    the client already owns.

---

## 3. The defect this closes

1. **There is no singleton.** No component owns "the chain that is up", so
   two surfaces can each have a menu open and a menu can outlive the click
   that should have dismissed it.
2. **Menu behaviour is implemented four times over.** `lib/controls`' `Menu`
   is shared, but the *shell* around it — placement, the grab, dismissal,
   keyboard traversal, what happens when the owner moves or dies — is
   re-implemented per surface: the terminal's `menu.rs`, the file manager's
   `ContextMenu` and `OpenWithMenu`, the pinboard's backdrop menu, and the
   bar's `menu.rs`/`clock_menu.rs`/`system.rs`. Four spellings of one rule
   is the duplication §2.2 forbids —
   and none of them agrees with the others about anything in section 1.
3. **Menus have no title and cannot be moved.** Nothing draws a plate band,
   so no menu can be identified or dragged aside to read what is under it.
4. **A submenu chain does not exist.** The model allows one level, and the
   one shell that renders a child renders exactly one, so a submenu cannot
   open a submenu.
5. **An app-drawn menu is clipped by its own window.** A menu that would
   extend past its app has to become a popup window or be truncated.
6. **The compositor cannot cache what it does not own.** A plate is ideal
   cache material — small, and changing only when its highlight does — but
   while an app owns it the compositor sees only "some window presented".

---

## 4. What stands, and what of it has to change

**Built and kept:** the `AppMenu` row model in `lib/abi/src/window_ipc.rs` —
an ordered bounded list of `Item { id, label, enabled, mark }`, `Separator`,
`Submenu { label, enabled }` and the manifest-attested info row, with
non-zero item ids unique within a menu, a decoder held to the **same**
shape rule as the builder, and both fuzzed in `lib/abi/tests/fuzz_decode.rs`.
It is carried by the caller-scoped `WindowRequest::SetAppBar` and answered by
the application-scoped `WindowEvent::AppBarMenu { item }`. The service reuses
this model verbatim; a second menu model would be the duplication §2.2
forbids.

**Built and kept:** the info panel as a `FactList` of the bundle's signed
`AppInfo`, placed trailing-or-flipped at its parent row — the two-deep
special case the general chain generalises.

**Has to change (M1):**

- **Submenu depth**: one level → a fixed `APP_MENU_MAX_DEPTH`. Nesting is
  already expressed as a parent index, so depth is a shape check, not a new
  encoding. Propose **4**.
- **Rows**: `APP_MENU_MAX_ROWS` (12) is a whole-menu total sized for the
  icon bar. A general menu needs it per plate and needs more of them.
- **Titles**: a root title for a per-window menu (a submenu's is derived,
  §1.1).
- **Accelerator text**: `MenuItem` already renders a shortcut column and
  every migrating surface has shortcuts to state; the wire model carries
  none.
- **A row kind for an app-provided attached window**, plus the present
  request and the arrival/refusal events §1.4 needs.
- **`AppMenuRow::About` → `AppMenuRow::Info`**, matching the label the
  desktop already draws and the vocabulary this plan uses.
- **The per-gesture open**: an anchor, and the three-way outcome
  `Chosen(id)` / `Dismissed` / `Refused(reason)` delivered **once** to the
  requesting window.

**And that re-opens M0's transport decision.** Inline bounded rows was
settled because the icon-bar model was small; `WindowRequest` is one fixed
frame sized to its widest op, so every one of those additions widens the
frame that the hot `Present` path also pays for. See decision 1.

---

## 5. Stages

### M1 — the model, the wire, and the transport

The contract, in four complete steps. No renderer work here. Each step moves
the builder's shape rule and the decoder together and extends
`lib/abi/tests/fuzz_decode.rs`, so a menu that crossed the wire stays exactly
a menu that could have been built.

**M1a — variable-length request framing (landed).** A `WindowRequest` now
encodes to the length its own operation needs, not to one frame sized to the
widest. Each operation owns one named `*_WIRE_LEN` that the encoder writes to
and the decoder requires *exactly*, so a request carrying a trailing byte is
refused rather than tolerated-if-zero (D1), and the end of an operand block
has one spelling rather than two (D2). `WindowRequest::wire_len` is the
per-value length — value-dependent for `SetAppBar`, whose declaration now
carries only its declared rows. `WINDOW_MAX_REQUEST` remains the endpoint's
receive bound.

Its point is that the menu model can grow through M1b–M1d without the hot
path paying for it: `Present` was one 522-byte frame and is now 36 bytes,
which is the whole of what it needs.

**M1b — the model.** `APP_MENU_MAX_ROWS` becomes a per-plate bound; submenu
nesting gains `APP_MENU_MAX_DEPTH` (propose 4) as a shape check over the
existing parent index; a root title joins the model (a submenu's is derived,
§1.1); rows gain the accelerator text `MenuItem` already renders; and
`AppMenuRow::About` becomes `AppMenuRow::Info`.

**M1c — the open.** The per-gesture request carrying an anchor, and the
three-way outcome `Chosen(id)` / `Dismissed` / `Refused(reason)` delivered
**once** to the requesting window.

**M1d — attached windows.** The row kind, the present request §1.4 needs,
and the arrival/refusal events, including the refusal of a window that
arrives after the pointer has left its row.

### M2 — the service

- One `Option<OpenChain>` per seat: the owner window, the model, the plates
  with their placements and pinned-by-drag flags, and any attached window.
- `lib/controls::window`'s `TitleBar` gains the empty command set §1.1 needs
  — today it always seats four controls and justifies its title against the
  leading cluster. It is extended, not copied: the drag gesture and the
  untrusted-label bounding have one implementation.
- Everything in section 1: bands and their drag, arrival-driven submenus,
  attached-window attach/detach, the grab, traversal, dismissal, lifetime.
- The desktop's own surfaces become its first clients (§1.6).
- Tests: the singleton rule (a second request closes the first and answers
  it `Dismissed`); placement and flip against all four screen edges at more
  than one depth; arrival-open with no timer, including travel from a parent
  row into its own child and onto a sibling row; drag moving a plate and its
  descendants but not its ancestors; attach/detach and that a submenu cannot
  detach; outside press consumed; Escape closing deepest-first; owner death,
  seat loss, and scale/theme change each answering `Dismissed`; a late
  attached window refused; and that moving a highlight damages only the two
  rows that changed.

### M3 — migrate, and delete

Each landing complete with its tests and docs, and each **deleting** the
surface's menu shell (§2.14):

1. `userland/apps/terminal` — `menu.rs` (`ContextMenu`, and `Command`'s
   shortcut table where the model now carries it) and the popup-window
   plumbing its run loop drives it through.
2. `userland/gui/session`'s pinboard backdrop menu.
3. `userland/apps/files` — its `ContextMenu` and `OpenWithMenu`. The shared
   `lib/browse::ContextMenuModel` **stays**: it is a model, and it is what
   keeps the file manager and the trusted picker from diverging. Open With…
   becomes a submenu, or an attached window where the candidate list is
   longer than a plate holds (decision 2).
4. `userland/gui/taskbar` — `menu.rs`, `clock_menu.rs`, `system.rs`. The bar
   keeps its *subjects* and loses its shell.
5. `userland/gui/taskbar`'s start menu, if its rows fit without distorting
   the model — its entries carry icons and are launcher entries rather than
   commands. Decide with evidence; it may legitimately stay bespoke.

`userland/gui/switchboard` has **no** menu of its own — it only receives
`AppBarMenu` — and is not a migration target.
`userland/apps/widgets` draws a `Menu` as a *control-gallery sample*, not as
a menu; it stays.

The final step removes any menu-shell helper in `lib/controls` left without
two consumers (§15.5).

### M4 — what the compositor can then do

Only after M3, because it is meaningless while apps own menu pixels: a plate
becomes a cached, damage-reporting surface like the window furniture
(`plans/COMPOSITOR-WORK.md`), so moving a highlight repaints two rows rather
than the plate.

---

## 6. Decisions required (§15.7)

1. **The transport, re-opened by section 4.** `WindowRequest` is one fixed
   frame sized to its widest operation, so a richer menu model widens the
   frame `Present` pays for on every composited frame.
   **Recommendation: make the request frame variable-length** — a
   length-prefixed payload with a per-op maximum, written into a caller
   buffer rather than materialised as one fixed array. The menu model then
   grows without touching the hot path, and `Present` *shrinks* to the bytes
   it actually uses (§2.16). Alternatives: keep the fixed frame and accept
   the widening; or grant a shared region per declaration (rejected at M0
   for a per-open cost, but a *declaration* is rare enough that it would now
   be defensible).
2. **A dynamic list longer than a plate.** Open With… is as long as the set
   of installed applications that claim the type, which no format bound can
   promise to hold. Either raise the per-plate row bound to a generous fixed
   value and refuse (fail closed) beyond it, or spell that a genuinely
   unbounded list is an **attached window** with its own scrolled list
   rather than a menu. Both mechanisms exist; the second keeps the menu a
   menu, at the cost of Open With… no longer looking like a submenu.
3. **Whether the bar's start menu is in scope** (M3 step 5), or stays a
   bespoke surface because its rows are icon-bearing launcher entries.

---

## 7. Definition of done (whole plan)

- One chain can be open per seat, no app draws a menu pixel, and every menu
  in the desktop — the apps' and the desktop's own — comes from the one
  service.
- Every plate has a title band and drags by it; submenus open on arrival
  with no timer; an attached window attaches, detaches on a click of its
  row, and cannot be a submenu that does.
- The info panel's text is the session's, from the signed manifest.
- Every migrated surface's menu code is **deleted**, not left beside the new
  path (§2.14), and no second menu shell or title-bar control exists
  anywhere (§2.2).
- The request is bounded, validated, fail-closed and fuzzed (§5.4, §19.6),
  and adds no capability (§5.2).
- The headless build still builds and runs with every `userland/gui/*` crate
  excluded (§17.3), and `cargo xtask deps-check` stays green.
- Docs land with the code: `docs/src/desktop/menus.md` states the model of
  section 1, and every migrated surface's page loses its menu description
  (§13).
- Whole-project gate green (§7): `cargo fmt --all`, one `cargo xtask ci`,
  `cargo xtask fuzz --secs 5`, `tools/ci/soak.sh both --secs 20`.
