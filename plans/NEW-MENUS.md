# NEW-MENUS — every menu is the desktop's: one chain, one grab, one renderer

Binding under `AGENTS.md` (§3, §15.18).

## Ledger

| Stage | What | State |
|---|---|---|
| M0 | Row model (`AppMenu`) + the icon-bar declaration carrying it | **landed** |
| M1a | Variable-length request framing: per-op wire length, exact-length decode | **landed** |
| M1b | Model: per-plate rows, submenu depth, titles, shortcuts, `About`→`Info` | **landed** |
| M1c | The per-gesture open: anchor in, `Chosen`/`Dismissed`/`Refused` out | **landed** |
| M1d | ~~Attached-window row kind~~ — deleted in M3.4 (D19) | **superseded** |
| M2 | The service: bands, drag, arrival-open, attach/detach, grab, lifetime | **landed** |
| M3.1 | Migrate `userland/apps/terminal`, delete its shell | **landed** |
| M3.2 | Migrate the pinboard backdrop menu, delete its shell | **landed** |
| M3.3 | Migrate `userland/apps/files`, delete its shell | **landed** |
| M3.4 | Migrate the bar's four menu subjects, delete `BarMenu` | **landed** |
| M3.5 | ~~The bar's start menu~~ — **not in scope** (decision 3) | **settled** |
| M4 | Plate as a cached damage-reporting surface | planned |
| M5 | Plates are floating chrome: 80% opacity over a 50% backdrop blur | planned |

Open decisions: 4 (binds M5 — what the owner's "80% opacity and 50% blur"
means in the theme, whose shared values every other floating surface takes).
Decision 1 is **settled** — variable-length framing, landed
as M1a. Decision 2 is **settled** — Open With… is one row that concludes the
chain, and the chooser is the application's own list surface; landed as M3.3.
Decision 3 is **settled** — the program-library popup is not a menu and stays
bespoke, so M3.5 is closed with nothing to do; the context menu *on* one of its
rows is a genuine menu and migrated with M3.4.

### Defects found, to fix in the stage named

Found while doing the work above; each is owned here until it lands, and the
stage that closes it carries its regression test (§2.18, §7).

| # | Defect | Fix in |
|---|---|---|
| D1 | `WindowRequest::from_bytes` accepted a frame longer than the op needs, checking only that the tail was zero. Exact-length framing refuses any trailing byte instead. | **closed in M1a** |
| D2 | The per-op operand-block end offsets were spelled twice — once implicitly by the encoder, once as a literal in the decoder's `reserved_zero(bytes, 36)` call. One named length per op now serves both (§2.2). | **closed in M1a** |
| D3 | Exact-length decoding refuses a random-length input before it reaches any operand, so the request decoders' fuzz coverage came to rest on the seeded frames — and only `SetAppBar` had one. `fuzz_decode` now seeds and bit-flips one frame per operation, at its length and one byte either side. | **closed in M1a** |
| D4 | Menu-child placement existed twice with two rules: `lib/controls`' `Menu::anchored_rect` slides a plate onto the screen, while the bar's own `child_rect` flips a child to its parent's other side. A root at a pointer and a child beside its parent legitimately differ, but the two rules must end up as one owner's (§2.2) rather than one shared and one private to the bar. M1c's wire anchor is a *region* precisely so one rule can serve both: a slot-anchored bar menu and an app's context menu differ only in whether that region has an extent. **One rule now**: `plate_rect(size, anchor, side, gap, viewport)` bounds the plate to the viewport, opens edge-adjacent on the asked-for side, flips when that side has no room (roomier side when neither does), then slides the cross axis and clamps. `Menu::anchored_rect` is its point case and the bar's `child_rect` is deleted. Placement is now *flip*-then-slide where a context menu used to slide only, which also keeps the press point at a corner of the plate rather than inside it. | **closed in M2** |
| D5 | `AppMenu::push_under` refused a submenu inside a submenu, so the model could not express a chain at all — the one-level bound was load-bearing in the builder, not only in the renderer. Nesting is now bounded by `APP_MENU_MAX_DEPTH` instead, and a submenu on the deepest plate is refused rather than drawn opening nothing. | **closed in M1b** |
| D6 | A row's text was a widest-case buffer per row, so the three fields `MenuItem` draws (label, accelerator caption, disabled-row reason) would have multiplied by the row bound — and `WindowRequest` carries a menu inline, so the hot `Present` path's own frame would have grown with them. A menu now holds its rows' text in one bounded block (`APP_MENU_TEXT_BYTES`) and the wire carries lengths, not offsets, in row order. | **closed in M1b** |
| D7 | `lib/window`'s client encoded every request into a fresh `MAX_WIRE_LEN` stack array and the session's serve loop received into one, so both cleared the widest operation's width on every call — including the hot `Present`. M1a made the *frame* per-operation but left these two buffers at the ceiling. Each is now held once for the life of the connection. | **closed in M1b** |
| D8 | The bar built a child plate's rows without folding a declared separator into the next row's group break, so a separator inside a declared submenu drew as a blank disabled row where the same separator on the root plate drew a divider. One `plate_rows` builder now serves both. | **closed in M1b** |
| D9 | The model describes a chain `APP_MENU_MAX_DEPTH` plates deep; the bar renders one level, so a submenu declared *inside* a submenu draws its chevron and opens nothing. Nothing in the tree declares one (`appbar::declaration` refuses a submenu outright), and the chain renderer is what M2 is. **Closed**: the chain opens a plate per level to `APP_MENU_MAX_DEPTH`, each placed against its own parent. | **closed in M2** |
| D11 | The frame-layout block every surface-opening request shares ended at the literal `41`, spelled three times over (`RESIZE_WIRE_LEN`, `POPUP_PARENT_OFFSET`, `CREATE_TITLE_LEN_OFFSET`) with its field offsets spelled again inside the codec — D2's shape a second time, and a fourth operation was about to join it. One `FRAME_LAYOUT_AT`/`FRAME_LAYOUT_END` now says where the block is and how wide, and every operation that puts operands after it derives its own offsets. | **closed in M1d** |
| D12 | Three requests open a surface and each wrote the same prologue longhand — the granted region, the event route, the frame layout. One `write_surface_operands` now writes it for all of them (§2.2). | **closed in M1d** |
| D13 | `WindowClient` remembers each window's extent and last presented frame to answer a redraw, and pruned that record only in its own `close`. The **session** ends an attached window with its chain, so a client using panels would have kept one record per gesture — unbounded growth on a list every `present` linearly scans. The client now settles its records on a chain's outcome, by the one shared `MenuOutcome::detaches` rule the desktop settles by, and `close` forgets a window even when the session no longer knows it. | **closed in M1d** |
| D14 | The shared `Menu` maps *any* chevroned row's activation to `OpenSubmenu`, so it could not tell a panel row's click — which detached its window — from a submenu row's, which opens a plate. Resolved by layer rather than by widening the control: `Menu` owns rows and chevrons, and the chain owned panel semantics. Moot since D19 deleted the panel row: every chevroned row now opens its child. | **closed in M2** |
| D15 | `MenuChain::render_plate` drew its rows through `Menu::render`, which lays a **complete** plate of its own — rim, rounded corners and ground. The chain had already laid one for the band and the rows together, so every plate carried the Signal Rim twice and the rows' own rounded top corners notched the outer ground just under the band. Resolved by layer, not by a flag: `Menu` gained `render_rows`, which paints rows into a plate someone else laid, and `render` is now that plus the plate. Row geometry is unchanged — both land exactly where `row_rect` reports — so hit-testing never disagreed with drawing and only the extra rim was ever visible. | **closed in M3.1** |
| D16 | The terminal assigned a freshly-opened settings sheet straight over `TerminalWindow::overlay`, so an assignment that found one already there would drop it without calling `close` — leaving a session-side popup window on screen with nothing owning it. `set_overlay` made the assignment unable to leak. **The open question is answered: it was a *live* leak, not a latent one.** A key does reach a parent whose popup is up — not by focusing its decoration, which gives the keyboard to the *furniture* and swallows the key, but through the bar's hover window picker: `raise_window` → `TaskBridge::raise` → `router.focus` moved focus to the parent with no client press, so nothing dismissed the sheet and the next `Ctrl ,` ran `Command::Settings` behind it. That focus move is itself the defect (D18) and is fixed there, which is where the regression test lives; `set_overlay` stays as the guard, now unreachable by construction rather than argued safe. | **closed in M3.2** (cause is D18) |
| D17 | **No QEMU vertical opened a menu**, so the chain's drain, grab and answer paths in `userland/gui/session/src/run.rs` — and the terminal's own open/answer path — were exercised by nothing, and no one had ever seen a plate on screen. **Closed**: the session now announces `MENU_SHOWN` ("menu chain on screen", `EventId(20_006)`) beside `WINDOW_SHOWN`, once per open and only after a frame carrying the chain reached the display — the only honest gate, since the reply an application gets says the open was *accepted*, never that a plate was drawn. `tests/integration/menu_qemu_aarch64` then launches the terminal from the program library, right-clicks its client, photographs the plate at the rectangle the production chain reports, and clicks its *Settings…* row; the guest latches `APP_LOADED` for the bundle, the 12-byte minted-id reply only `OpenMenu` produces, and a create observed *after* it — the sheet the chosen row opens, which the terminal opens on nothing but the one `MenuClosed` naming that row. | **closed in M3.2** |
| D18 | Raising a window gave the keyboard to *that* window even when its own live transient sat above it. A raise brings a window's transients up with it (`Compositor::raise` restacks the family), so `show_raise_focus` — the one path `TaskBridge::open` and `TaskBridge::raise` share — left the focused window *underneath* its own modal sheet: the bar's hover window picker could focus a terminal behind its open settings sheet, with no client press to dismiss it, and the next accelerator ran in the client instead of the sheet. That is the route that made D16 live. **Fixed** by focusing the front of the family that rose (`Compositor::family_front`, one rule shared with `family_top`); a window with no transient open is its own family front, so the ordinary case is unchanged. | **closed in M3.2** |
| D19 | `AppMenuRow::Panel`, `WindowRequest::CreateMenuPanel` and `WindowEvent::MenuPanelRequested` had **no production client** — only a fuzz seed declared a panel row. M1d landed the attached window for the shape decision 2 turned out not to want. **Closed by deletion.** None of the bar's four subjects is its client, and the reason is structural rather than a judgement: an attached window is a surface the *owning application* draws, and a bar chain's owner is the desktop itself, so there is no client to ask. A presentation child the desktop would draw itself — a calendar under the clock — is the `Info` row's kind, not this one's. So the row kind, its `AppMenuRowView` and `RowKind` cases, `APP_MENU_KIND_PANEL`, `APP_MENU_PANEL_MAX_PX`, `WindowRequest::CreateMenuPanel`, `WindowEvent::MenuPanelRequested`, `MenuOutcome::detaches`, `WindowHost::menu_panel_opened`, `MenuPanelSpec`, the engine's `MenuAttachment` lifetime and `close_menu_panel`, `WindowClient::create_menu_panel`/`settle_menu_chain`, the chain's `ChainChild::Panel`/`Attached`/`place_panel`/`PanelRefused`/`SurfaceKind::Attached`/`ChainAction::RequestPanel`, and the fuzz seed are all gone. `deliver_event` lost the `host` parameter M1d gave it for the teardown, and §1.4 collapses to the one child a chain hangs: the desktop-drawn information panel. | **closed in M3.4** |
| D20 | The chain holds the seat's grab, so a press outside it is consumed and never delivered — which makes the second press of `files.app`'s FM12 **right-double-click** ("activate, then close this window") structurally unreachable the moment its first press opens a desktop menu. Not an implementation cost: an application deliberately cannot see a press inside chrome it does not own. **Resolved by re-spelling the capability rather than dropping it**: `ContextCommand::OpenAndClose` carries the same `AfterHandoff::CloseWindow` activation as a menu row — discoverable, and reachable from the keyboard, which the gesture never was — and `gesture.rs`'s `secondary_press`/`SecondaryPress`/`MenuOnSingle` are deleted with the gesture. A right-press still breaks a half-finished left pair (the app resets the tracker). | **closed in M3.3** |
| D21 | Threading the chain through `files.app` grew three of its routers (`apply_event`, `apply_nav_event`, `apply_pointer`) past the argument threshold, so each carried a justified `#[allow(clippy::too_many_arguments)]`. **Closed**: one bundle per side — `WindowState` for what a round acts *on* (browser, overlays, places) and `Acts` for what it acts *through* (the menu link, the launcher). All three routers are back under the threshold and all three allows are gone. | **closed in M3.4** |
| D22 | The one-based `index ↔ AppMenuItemId` numbering every command-list menu uses was spelled twice — `lib/browse::chrome`'s `row_id`/`context_command_from_item` and `session::pinboard`'s `PinboardCommand::row_id`/`from_item` — and M3.4 needed it a third time for the bar's three table-driven menus. **Closed**: `AppMenuItemId::for_index` / `AppMenuItemId::index` are the one definition, beside the id type whose non-zero invariant is the reason the numbering is one-based at all. | **closed in M3.4** |
| D23 | The bar's own menus were reachable only through the bar: no host test could open one *and* read its answer back, because the plate and the grab were the bar's own shell and the answer never left it. Migrating them made the round trip host-testable in two halves — the model and the id→response inverse in the taskbar suite, the plate, grab and one answer in the session suite (`choose_bar_menu_row` drives the whole path the serve loop drives). The end-to-end wiring stays witnessed by the two QEMU verticals that already click a bar menu row: `appbar_qemu_aarch64` (a slot's declared *New window*) and `datetime_elevate_qemu_aarch64` (the clock's *Set Date & Time…*), both of which now pass only if the chain path is right. | **closed in M3.4** |
| D10 | The bar's own menus state two things the model cannot carry: a row denied for want of a capability (`AuthorityState::NeedsCapability`, which draws an Authority Mark rather than merely greying) and a row whose setting is already in effect (`ActivityState::Complete`). **Answered.** The Authority Mark says *the system* refused a command, and only the system may say it, so it is in-process-only — and structurally, not by a check: the wire model has no field for an authority state, so a decoded row is always `Allowed` and an application cannot paint the mark on its own row. §1.6's "bounded subset" means an absent field, never a validated one. `ActivityState::Complete` ("work finished successfully") is a *misuse* on an appearance row and is deleted rather than carried: the alternatives are a radio group, which the wire already spells `AppMenuMark::Radio` plus disabled plus a reason. The bar's own rows keep `NeedsCapability`, because they are the desktop's. **Closed in M3.4**: `system.rs`'s in-force appearance row is now `MenuMark::Radio` plus disabled plus its reason, as the pinboard's sort and arrangement rows became in M3.2, and no menu row anywhere states an activity. (`render.rs`'s remaining `ActivityState::Complete` is a *notification card* for a `Success` severity — "work finished successfully" about work that did — and is correct.) | **closed in M3.4** |

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
  transient/owner relationship a plate uses.
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
  model) or the **information panel** — the session's own `FactList` of the
  owning bundle's signed manifest. Both hang where a submenu hangs and both
  obey the chain's lifetime.
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

### 1.4 The information panel

The one child of a chain that is not a plate of rows is the **information
panel**: a `FactList` of the owning bundle's `AppInfo`, hanging where a
submenu's plate would and dying with the chain — it closes when the pointer
settles on another row of its parent, when the chain dismisses, or when the
chain's owner dies. It wears the plate title band, so it reads as part of the
chain, and a press on it is claimed and acts on nothing: it states facts and
offers no command, so its row names no id and choosing it answers nothing.

It stays **session-drawn from the signed manifest**. The app declares only
that the row exists and supplies none of the panel's text, so it cannot state
an identity that is not its own inside desktop chrome.

**There is deliberately no app-drawn attached window.** M1d landed one — a
surface the *application* presented, hanging off a menu row, detaching when
its row was chosen — and it never found a client: decision 2 established that
a selection list cannot be one (a panel cannot conclude a gesture), and M3.4
established that a *desktop*-owned chain has no application to ask in the
first place. The whole mechanism is deleted (D19). A presentation child the
desktop draws itself is this panel's kind, not a second one.

### 1.5 The grab

While a chain is up the seat's pointer and keyboard route to it:

- A press **inside** the chain — any plate, the information panel — acts
  there.
- A press **outside** the chain dismisses it and is **consumed**, not
  delivered. A dismissal must not double as a click on whatever was behind
  the menu.
- **Escape** closes the deepest open child; with only the root open it
  dismisses the chain. Repeated Escape therefore always gets the user out.
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
   depth and total model bytes are *format* bounds a
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
an ordered bounded list of `Item(AppMenuItem)`, `Separator`,
`Submenu { label, enabled }` and the manifest-attested `Info` row, with
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

- ~~Submenu depth, per-plate rows, a root title, accelerator text, and
  `About` → `Info`~~ — landed as M1b, along with the disabled-row reason and
  the role the same control draws.
- ~~The per-gesture open: an anchor, and the three-way outcome
  `Chosen(id)` / `Dismissed` / `Refused(reason)` delivered **once** to the
  requesting window~~ — landed as M1c, with the outcome keyed to a
  session-minted open id so one gesture's answer cannot read as another's.
- ~~A row kind for an app-provided attached window, plus the present request
  and the arrival/refusal events~~ — landed as M1d and **deleted in M3.4**: no
  client ever wanted it (D19), so the chain's one non-plate child is the
  session's own information panel.

**And that re-opened M0's transport decision**, settled as decision 1 and
landed as M1a. M1b then found the same cost in a second place the framing
decision did not reach — the model held *in memory*, which rides inside every
decoded `WindowRequest` — and answered it the same way (D6).

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

**M1b — the model (landed).** `APP_MENU_MAX_ROWS` (32) now bounds one
*plate*, with `APP_MENU_MAX_TOTAL_ROWS` (64) bounding the whole menu — its
own bound rather than the product of the others, because it is what holds the
one frame a menu crosses in. Nesting is bounded by `APP_MENU_MAX_DEPTH` (4)
as a shape check over the existing parent index, so a chain is expressible at
last (D5) and a submenu on the deepest plate is refused rather than drawn
opening nothing.

A row now states everything the shared `MenuItem` control draws: its
accelerator caption (`APP_MENU_SHORTCUT_MAX`, 24), the reason it is disabled
(`APP_MENU_REASON_MAX`, 64), and its role (`AppMenuRole::{Neutral,
Destructive}`) beside the label and mark it already had. `AppMenuRow::Item`
carries an `AppMenuItem` built through `new` + `with_*` rather than seven
spelled fields, and `AppMenuRow::About` is `AppMenuRow::Info`.

Those three text fields per row would have multiplied by the row bound had a
row kept a widest-case buffer each, and `WindowRequest` carries a menu inline,
so the hot `Present` path's own frame would have grown with them (D6). A menu
therefore holds every row's text in **one** bounded block
(`APP_MENU_TEXT_BYTES`, 1536) — the "total model bytes" bound invariant 5
names — and reports rows back as `AppMenuRowView`, borrowing that block. The
wire matches: one 8-byte record per row carrying *lengths*, then the text in
row order, consumed exactly. There are no offsets, so nothing can point
anywhere, no two rows can share bytes, and no text can ride along unread.
The model is 2344 bytes where fixed-width row text would have been ~8.6 KiB,
and a pinned test keeps it that way.

A root title joins the model (`AppMenu::titled`; a submenu's plate takes its
parent row's label, §1.1). The **declaration has no title field at all**: the
icon-bar menu is titled from the bundle's signed manifest, so a titled menu
is refused at encode rather than being encoded and quietly retitled — an
application cannot title system chrome as something it is not. M1c's open
request is where a title crosses the wire.

**M1c — the open (landed).** `WindowRequest::OpenMenu { window_id, anchor,
menu }` (op 14) is the per-gesture ask: window-scoped rather than
process-scoped, and not idempotent-replace. It adds no capability — the window
the caller already owns is the scope, and ownership is the kernel-attested
identity of the in-flight caller.

The **anchor is window-local** (`MenuAnchor`, physical pixels from the
requesting window's own client origin), because that is the only space an
application can speak truthfully: it is never told where its window is, and
never learns a pointer position inside a menu, so a seat-global anchor would
have to be fabricated. It is exactly what `WindowEvent::Pointer` already
reports, so an app anchors a context menu at the press it was handed. It is a
**region, not a point** — that is what §1's placement rule reads, and a
zero extent is the point case — so the bar's slot-anchored menus and an app's
context menu resolve through *one* placement rule rather than the two D4
already flags. Any origin is legitimate (the session clamps, as
`CreatePopup`'s offsets already do); only the far edge must be representable,
which is what leaves the placement arithmetic no unrepresentable input.

The **title crosses here**, carried by the menu itself (`AppMenu::titled`),
length-prefixed after the rows' text rather than in a widest-case field. A
declaration still structurally cannot carry one and is refused at encode, so
§1.1's rule stands: system chrome is titled from the signed manifest. An open
carrying no rows is refused at both ends, where a declaration legitimately
offers none. Both operations share **one** menu block codec
(`write_menu_block`/`read_menu_block`), so a row cannot be laid out one way by
a declaration and another by an open.

**The outcome is one event, and the open id is what makes it unmistakable.**
The reply is only the acceptance and carries the session-minted, never-reused
open id — the shared `WINDOW_MINTED_ID_REPLY_LEN` frame a `Create` reply now
also begins with, rather than a second near-identical shape. The answer is
exactly one `WindowEvent::MenuClosed { window_id, open_id, outcome }` with
`MenuOutcome::{Chosen, Dismissed, Refused}`. **One** kind, not three, so an
app's handling is a total `match` and the engine's "this event answers an
open" rule is one variant that cannot drift. It fits the existing fixed
40-byte event frame (14 of its 24-byte block), so an outcome costs no other
event a byte — a deliberate choice, since widening that block would have taxed
every event on the channel.

The id is load-bearing, not decoration: without it an app that asked again
while a previous answer was still in its mailbox would read one gesture's
`Dismissed` as the next one's outcome — reachable with no app bug, since a
keyboard-driven open races a delivered answer. `MenuRefusal` is closed
(`NoDisplay`, `SeatBusy`, `NoResources`) and an unknown discriminant fails
closed; a refusal is an answer the app reports and carries on from.

**Exactly-once is enforced, not hoped for.** The engine keys the open to the
attested owner, holds one unanswered open per window, and requires an outcome
to name *that window's own* open before delivering it, clearing it when the
sink accepts. A second outcome for one open is refused; so is an outcome for
another window's open, or for one already answered. Per *window* rather than
per seat is what lets M2's singleton close one chain and answer **it** while
another window's open stands — no engine change, no second slot, and no sink
threaded through the request path. A second open on a window whose open is
unanswered is refused (`AlreadyExists`), which a well-behaved app cannot reach:
while its chain is up the seat's grab consumes the press that would open
another. `WindowHost::menu_open_requested` defaults to refusing, so a desktop
composing no menu service fails closed, and a refused open records nothing and
spends no id.

**M1d — attached windows (landed, then deleted).** The row kind, its present
request, its arrival event and the engine's attach/detach lifetime were built
here and removed in M3.4 for want of any client (D19). What the stage
contributed that *stayed* is the shared frame-layout block one definition now
serves (D11), the shared surface prologue every surface-opening request writes
through (D12), and the client's per-window record pruning (D13).

### M2 — the service (landed)

`userland/gui/session/src/menu.rs` is the chain: one `Option<OpenChain>` per
seat holding the owner, the model, the plates with their placements and
pinned-by-drag flags, and whatever hangs off the deepest one. It owns model,
state and geometry and touches **no** compositor — the session presents the
surfaces it lists and takes down what it no longer has — which is what lets
every rule of section 1 be tested without a screen.

What it guarantees, beyond section 1 itself:

- **One answer, one delivery point.** Every close — a chosen row, a dismissal,
  a chain displaced by the next open, an owner's death, a mode change — queues
  its answer through `MenuChain::take_answers`, drained in one place. No path
  can answer a chain twice and none can leave one unanswered. A chain very
  often ends inside the window engine's own serve pass, which already holds
  the borrow `deliver_event` needs, so queueing *all* of them rather than only
  those is what keeps the point single.
- **The mode change is the chain's own rule**, not a dismissal every
  mode-changing call site must remember: the chain records the screen and the
  output epoch it was placed against and ends itself when either moves.
- **The service-facing model is a superset of the wire one, structurally.** A
  `ChainRow` carries the whole of `ControlState`, because the desktop's own
  rows may state that the *system* lacks authority for a command; the wire has
  no field for that, so a decoded application row cannot claim it (D10).
- **The host resolves the anchor**, because the engine retains neither the
  model nor the pointer: `ShellWindowHost::menu_open_requested` resolves the
  window-local anchor against the owner's live client origin and never reads a
  file.
- **`lib/controls::window`'s `TitleBar` seats an empty command set**
  (`TitleBarCommands::Empty`, `TitleBar::plate()`), extended rather than
  copied: one drag gesture, one untrusted-label bounding. The whole-band drag
  span and the centred title follow from the emptiness, not from new knobs.

**`MenuRefusal` has its producers, one per seat condition.** `NoDisplay` when
there is no output to place on and `SeatBusy` when a surface a menu may not
displace holds the seat (the lock screen, the trusted picker) are decided as
the open is served — the open is *accepted*, so the application is owed its one
answer and gets the reason rather than a chain. `NoResources` is answered where
it is honest: when the session actually cannot give a plate its surface, which
is at draw time, not at ask time. A refusal never takes down a chain the user
is already using.

Two things the stage's own test list named that the tree answers differently,
rather than leaving untested:

- **Seat loss** has no per-chain answer. Losing the lease ends the *session*
  (`drain_fault`), and the window channel goes with it, so there is no
  surviving path on which a `MenuClosed` could be delivered. The chain's
  lifetime rules cover owner death and the mode change; seat loss is the
  session's own end.
- **Damage confinement** is M4's, not this stage's: a plate is not yet a
  cached damage-reporting surface. What M2 does guarantee, and tests, is that
  a pointer travelling *within* a row costs no repaint at all, and that moving
  a highlight on one plate opens, closes and moves nothing else.

The desktop's own surfaces are **not** clients yet: the pinboard's backdrop
menu is M3.2 and the bar's four subjects are M3.4, which is where those
migrations already lived. The service's production client today is the wire
path — any application's `OpenMenu`, implemented end to end.

### M3 — migrate, and delete

Each landing complete with its tests and docs, and each **deleting** the
surface's menu shell (§2.14):

1. `userland/apps/terminal` — `menu.rs` (`ContextMenu`, and `Command`'s
   shortcut table where the model now carries it) and the popup-window
   plumbing its run loop drives it through.
2. ~~`userland/gui/session`'s pinboard backdrop menu~~ — landed as M3.2.
3. ~~`userland/apps/files` — its `ContextMenu` and `OpenWithMenu`~~ — landed
   as M3.3.
4. ~~`userland/gui/taskbar` — `menu.rs`, `clock_menu.rs`, `system.rs`~~ —
   landed as M3.4. The bar kept its *subjects* and lost its shell.
5. ~~`userland/gui/taskbar`'s start menu~~ — decision 3 settled it: the
   program-library popup is a searchable scrolled list, not a menu, and stays
   bespoke. Nothing to migrate.

**M3.1 (landed).** `userland/apps/terminal` keeps no menu shell: `ContextMenu`,
its `MenuOutcome`, and the popup arm that drew them are gone, and `menu.rs` is
now the row model alone — `Command::ALL` built into an `AppMenu` by
`menu::model`, read back by `Command::from_item`. A secondary press sends
`OpenMenu` with the window-local point the press was reported at; the answer is
one `MenuClosed` matched against the open id that window is waiting on, so an
answer to a settled gesture cannot run a stale command. The accelerator
captions are the model's rows, and a test parses each row's own caption back
through `Command::accelerator`, so a row cannot advertise a keystroke that does
nothing. `Command::accelerator` itself stays: matching a keystroke is not the
same thing as captioning a row. The settings sheet is **not** a menu and keeps
its popup surface; `Content`/`OverlayRequest` collapsed to it alone.

**M3.2 (landed).** The pinboard keeps no menu shell: `PinboardMenu`, its
`PinboardMenuOutcome`, the seat drain and settle path that drove it, and
`DesktopShell`'s `present_pinboard_menu` / `pinboard_menu_bounds` / its own
compositor window are all gone. `pinboard.rs` is now the row model alone —
`PinboardCommand::ALL` built into a `ChainModel` by `pinboard::model`, read back
by `PinboardCommand::from_item` — and `DesktopAction::OpenMenu` is deleted with
it: `Desktop::context_press` answers whether an icon was under the press and
names no action, because the menu is the seat's chain and the embedder that owns
the chain opens it.

It opens with `ChainOwner::Backdrop`, so no `OpenMenu` crosses a wire, no open id
is minted, and its one answer arrives at `run.rs`'s single delivery point
(`answer_menu_chain`) beside every application's. **The two seat drains became
one**: the chain's drain now carries the desktop state a chosen backdrop row
acts on, so the pinboard's duplicate is gone.

Two rules landed with it rather than being carried over. D10's judgement is
applied: the sort order and the arrangement in force are a *radio group* —
`MenuMark::Radio` plus disabled plus a reason — not `ActivityState::Complete`,
which said "work finished successfully" about an appearance row. And a row's id
is its command's own position in `PinboardCommand::ALL` rather than its position
on the plate, so the gesture that leaves `Open` out shifts no other row's
meaning. M3.4 converted the bar's `system.rs` in-force appearance row the same
way.

The desktop's own menu also now resolves through the **same** seat rule an
application's open does (`seat_menu_refusal`, hoisted out of
`ShellWindowHost::seat_refusal`), so a backdrop press cannot take the grab from
the lock screen or the trusted picker by arriving from the other direction — a
guarantee M2 stated as the service's and the old shell did not have.

**M3.3 (landed).** `userland/apps/files` keeps no menu shell: `ContextMenu`,
`OpenWithMenu`, the two routers that owned input while each was up
(`apply_menu_event` / the old `apply_open_with_event`), and `lib/browse`'s seven
drawn-menu renderers (`build_context_menu`, `context_menu_rect`,
`draw_context_menu`, `context_menu_command_at`, `context_menu_command_rect`,
`build_open_with_menu`, `open_with_index_at`, and the private
`menu_enabled_row_at` they shared) are all gone. It is a **wire** client, so
M3.1 is its template: `lib/browse::chrome` is now the row model alone —
`CONTEXT_COMMANDS` built into an `AppMenu` by `context_menu`, read back by
`context_command_from_item`, with the one-based `row_id`/`from_item` inverses so
ids need no second table — and `run.rs`'s `open_context_menu` sends `OpenMenu`
with the window-local point the press was reported at, storing the returned open
id on the window (`OpenWindow::menu`). The answer is one `MenuClosed` matched
against that id, so an answer to a settled gesture cannot run a stale command; a
refusal is stated on `stderr` and the window carries on.

Two rules landed with it rather than being carried over. A row's id is its
command's own position in `CONTEXT_COMMANDS` (M3.2's rule), and because every
command is declared *disabled with its reason* rather than left out, position on
the plate and position in the list are the same thing however few are
actionable. And `ContextMenuModel::is_enabled` is now **derived from** a new
`reason`, so a row cannot be greyed with nothing to say — the reason field M1b
landed has its first client, and the three ways Open With… can be inapplicable
(no selection, not a file, a link that leads nowhere) each say which.

`ContextMenuModel` itself **stays**, as the plan said: it is the model that keeps
the file manager and the trusted picker from diverging. What moved is only where
the rows are *drawn*.

The builder lives in `lib/browse` rather than in the app because `run.rs` is a
freestanding binary no host test can reach: the model, the id round-trip, the
reasons, the emphasis, and the chooser's whole geometry are host-proven there,
and what is left in the app is the glue the menu vertical already exercises for
the terminal.

Decision 2 is settled above, so **Open With… is one row that concludes the
chain** and the chooser is the application's own surface: `OpenWithChooser` in
`lib/browse::open_with` (candidates, selection, scroll offset, its own
`ScrollBar`) drawn by `render::draw_open_with_chooser` as a scrolled list of
`ListRow`s in a `Panel`, hit-tested by `open_with_row_at` through the one
placement all three share. It scrolls by wheel, by the drawn bar's drag, and by
Up/Down/Home/End with the selection revealed; `Enter` or a press on a row hands
the file over through the same `launch_viewer` the default open uses, and
`Escape` or a press off the rows dismisses it. Its bar and the listing's now
route a press through **one** rule (`route_scroll_bar`), so the two cannot come
to behave differently.

D20 is the migration's own finding: the chain's grab took the right-double-click
gesture's second press away, so that capability is a menu row now
(`ContextCommand::OpenAndClose`) and `gesture.rs` loses the three types that
spelled the gesture. Nothing else in the tree drew or clicked the file manager's
context menu — no QEMU vertical reconstructed it — so the migration broke no
end-to-end test; `PLAN.md`'s claim that the FM9-c right-click→Delete
click-through was wired into the `autoload_input` vertical was already stale and
is corrected there.

**M3.4 (landed).** The icon bar keeps no menu shell: `BarMenu`, `MenuLayout`,
`MenuChoice`, `MenuOutcome`, `OpenChild`, `TaskbarRepaint::MENU`,
`TaskbarRenderer::render_menu`, `TaskbarPresenter::present_menu`,
`Taskbar::{menu, menu_routing_mut, menu_layout, open_*_menu, close_menu}` and
the router's `route_to_menu`/`apply_choice`/`apply_system_action` are all gone.
The bar holds **no menu state at all** — while a chain is up the seat's grab
means no event reaches the bar, so there is nothing for it to be modal about.

It is an **in-process** client, so M3.2 is its template rather than M3.1: a
secondary press answers `TaskbarResponse::OpenMenu(MenuRequest)` — which menu,
its rows, and where the plate hangs — and `run.rs`'s `open_bar_menu` opens the
seat's one chain for it. All four subjects migrated: the application slot's
declared menu, the program-library row's context menu, the system quick
actions, and the clock.

**The model type moved to `lib/controls`.** An in-process client builds a
`ChainModel` (never an `AppMenu` — D10's authority state has no wire field), and
the bar cannot depend on the session, which depends on *it*. So `ChainModel`,
`ChainRow`, `ChainChild`, `from_app_menu` and `INFO_ROW_LABEL` now live beside
the `Menu`/`MenuItem`/`FactList` they are made of, and `lib/controls` gains the
`lib/abi` edge the row id and the wire model need (every crate that already
depends on `lib/controls` depends on `lib/abi` too, so nothing widens). The
chain itself — state, geometry, grab, lifetime, answers — stays in the session.
`PlatePlacement { anchor, side, gap }` joined it: three values every caller
passes together, which turns `plate_rect` from six arguments into four and lets
the desktop's shared open path stay inside the argument threshold.

**One chain, two answer shapes, and the seam is the owner.** `ChainOwner` is
now `Window { window_id, open_id } | Backdrop | Bar(MenuSubject)` — the address
an answer goes to. A chosen row of a **wire** chain leaves as the `MenuClosed`
the engine holds it to; a chosen row of a **bar** chain is read back by the bar
itself (`Taskbar::menu_chosen`, over the same table the plate was built from)
into the very `TaskbarResponse` a click on the bar produces, and routed where
those are. So the application-scoped `AppBarMenu` relay is untouched: the App
subject answers `AppMenuChosen { app, item }` and `route_outcome` relays the id
to the declaring process exactly as before. The subject travels **inside** the
owner rather than beside it, so a chain the next open displaces cannot have its
answer read against the next chain's subject.

The desktop's two in-process menus now open through **one** call
(`menu::open_desktop_menu`), which applies the seat rule (§1.5) and the model
check together, so neither the backdrop nor the bar can take the grab from the
lock screen or the trusted picker. And **the two seat drains became one**: the
chain drain lives inside the `SEAT_TOKEN` branch and its answers flow through
the branch's existing routing loop, so a *Log Out* row and a *Log Out* click
are honoured in one place — the alternative was a third copy of that routing.

Three rules landed with it rather than being carried over. A row's id is its
command's own position in its table (M3.2's rule), so the system menu without
*Switch User…* shifts no other row's meaning — and that numbering is now one
definition rather than three (D22). D10's remainder is applied: the in-force
appearance row is a radio group, not `ActivityState::Complete`. And D19 is
closed by deletion: a desktop-owned chain has no application to draw an
attached window, so the mechanism had no client and is gone.

`userland/gui/switchboard` has **no** menu of its own — it only receives
`AppBarMenu` — and is not a migration target.
`userland/apps/widgets` draws a `Menu` as a *control-gallery sample*, not as
a menu; it stays.

Every menu-shell helper in `lib/controls` has two or more consumers (§15.5):
`plate_rect` and `PlatePlacement` place every plate and the information panel,
`Menu::render_rows` paints the rows of every plate, `TitleBar::plate` bands
them, and `ChainModel` is built by the bar, by the pinboard and by the wire
decode.

### M4 — what the compositor can then do

Only after M3, because it is meaningless while apps own menu pixels: a plate
becomes a cached, damage-reporting surface like the window furniture
(`plans/COMPOSITOR-WORK.md`), so moving a highlight repaints two rows rather
than the plate.

### M5 — a plate is floating chrome, not a solid card

**Owner requirement.** Every menu is to read like the program-library popup and
the Switchboard capsule's own menu already do: **80% opacity over a 50%
backdrop blur**. A plate today is *opaque* and asks for no blur — M2 decided it
"covers what it opens over" — so this **supersedes that decision**, and the two
tests in the session crate that state it — the one asserting a plate asks for no
blur, and the one asserting it lays the opaque raised ground — invert with the
stage.

Opacity and blur are **one decision, not two.** Blur behind an opaque surface
is per-frame work nothing shows through; translucency without blur is sharp
detail competing with the rows on top. The theme already pairs them
(`Palette::chrome_alpha` with `Metrics::chrome_backdrop_blur`) and every other
floating surface takes both, so a plate takes both or neither.

**The numbers do not match the reference surfaces, and that is the decision to
settle first** (decision 4 below). Today's shared floating chrome is
`CHROME_ALPHA = 179` (70%) over `chrome_backdrop_blur = 7` logical pixels — so
the popup the requirement cites as correct is at 70%, not 80%, and 7 px is not
readily "50%" of anything but the `WINDOW_BACKDROP_BLUR_MAX_PX` (64) wire
ceiling, of which it would be 32.

What the stage does, once decision 4 fixes the values:

- The chain grounds its plates in the **floating** theme
  (`Theme::floating()`, which flips `SurfaceGround` and touches no metric, so
  no rectangle moves) and asks the compositor for the theme's
  `chrome_backdrop_blur` where it places each surface — the plate, the
  information panel, and every child.
- The floating form is derived **once** for the desktop rather than per surface
  (the bar clones its own today), so the bar, its popups and every plate ground
  themselves identically and a theme switch cannot leave one behind.
- One theme for the whole chain: the plate's pixels and the rectangles its rows
  are hit-tested against must not come from two.
- The plate's own **rows** already take `chrome_alpha` when the ground is
  floating (`Palette::chrome_alpha`'s rule — anything reading as part of the
  surface takes it), so a resting row stays exactly its ground with no second
  rule.

Untouched by it: the plate is still one ground for the band and the rows
together (D15), and the corner radius and rim are still the shared popup
recipe's.

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
2. **A dynamic list longer than a plate — settled.** Open With… stays **one
   `Item` row on the one plate**. Choosing it concludes the chain, and the
   application then opens **its own chooser**, which is not a menu: a scrolled
   list of `ListRow`s in a `Panel` (`lib/browse::open_with`'s
   `OpenWithChooser`, drawn by `lib/browse::render`). Both named alternatives
   were rejected, each for a reason that is fatal on its own:
   - **Raising the per-plate row bound: no.** The candidate set grows with the
     applications a user installs, so no fixed value can promise to hold it —
     raising the bound moves the refusal rather than removing it. And it buys
     an expressible plate no screen can draw in full: `plate_rect` bounds a
     plate to the viewport and nothing scrolls it, so thirty-two rows is
     already about seven hundred logical pixels and a raised bound would put
     rows where no pointer can reach them. Widening a format bound "to be
     flexible" is what §24.4 forbids, and the total-row bound is what the
     endpoint's receive ceiling is sized to, so every menu on the channel
     would pay for this one list.
   - **A submenu of candidates: no, because a submenu cannot be lazy.** The
     model crosses the wire *complete* — every row of every plate rides in the
     one `OpenMenu` — so the candidates must be enumerated **before** the menu
     opens. Enumerating them is filesystem I/O over three program stores,
     reading and decoding every `<Name>.app/AppInfo` (`RtBundleSource`), one of
     which is `/Apps` and may live on a slow or failing volume. Every
     right-click would pay it, for a list the user rarely opens, with a latency
     that scales with the number of installed applications (§2.16, §26.1). The
     only escape is a cache of every bundle's MIME table held for the life of a
     long-running desktop component, which then goes stale the moment an
     application is installed — and the list would *still* be bounded by what
     one plate holds.
   - **An attached window: no, because it cannot conclude the gesture.** Only
     a *row* of the chain ends a chain (`MenuOutcome::Chosen`), and an
     application deliberately holds no request that dismisses one (invariant 2
     — it cannot pin a chain open, and symmetrically cannot close one). A
     candidate list inside a panel would therefore leave the chain standing
     after the user had chosen an application, and every gesture that would
     then end it means something else: clicking the panel's own row *detaches*
     the panel, Escape closes the panel first, an outside press dismisses. The
     panel is a **presentation** surface — the session-drawn info panel it
     generalises is a `FactList` — not a selection one. That leaves it without
     a client (D19).
   What this costs is that Open With… does not look like a submenu, which is
   the honest shape: a *chooser over a data set whose size is a property of the
   machine* is a list, and lists scroll. What it buys is that the menu stays a
   menu — nine command rows, no I/O to open, one plate — and the unbounded list
   sits in the surface kind the file manager already draws for Properties and
   the delete confirmation. M3.1 set the precedent: the terminal's settings
   sheet is not a menu and kept its own surface.
3. **The bar's start menu — settled. It is not a menu and stays bespoke**, so
   M3 step 5 is closed with nothing to migrate. The program-library popup
   (`userland/gui/taskbar/src/library.rs`) is a **searchable, scrolled list over
   a data set the machine's size decides**: it holds a text filter, a scroll
   offset with the shared `ScrollBar`, and expandable folders over a catalog as
   large as the programs a user installs. That is decision 2's shape exactly,
   and two of its reasons bind here whatever else does not:
   - **A plate does not scroll.** `plate_rect` bounds a plate to the viewport
     and nothing scrolls it, so the entry set — which grows with what is
     installed — has no bound a plate could promise to hold. Raising the
     per-plate row bound moves the refusal rather than removing it, and widens a
     format bound "to be flexible" (§24.4).
   - **A plate has no text input.** The filter is the popup's primary
     affordance: typing narrows the list. A menu's rows are fixed at open, and
     the model carries no field a keystroke could edit.
   Two reasons that would have bound an application's list deliberately do
   **not** apply, and saying so is what makes this a decision rather than a
   restatement: the popup is session-owned, so nothing about it crosses the
   wire complete, and its rows carry icons, which `MenuItem::with_icon` can
   already draw. Neither rescues it from the two above. Expandable folders are
   *disclosure within one surface*, not a chain of child plates, so they are not
   a submenu either.
   What this costs is that the launcher does not read as a menu, which is the
   honest shape: a list of *everything installed* is a browser, and browsers
   scroll and filter. What it buys is that the popup keeps the search and the
   scroll it needs, while the genuine menu inside it — the context menu on one
   of its rows (`MenuSubject::Entry`) — is the desktop's chain like every other,
   migrated in M3.4.

4. **What "80% opacity and 50% blur" means in the theme** (binds M5). The
   requirement names two figures and two reference surfaces, and they disagree:
   the program-library popup and the Switchboard capsule's menu are drawn at
   `CHROME_ALPHA` = 179 (**70%**) over `chrome_backdrop_blur` = **7 logical
   pixels**. So either the figures are a description of that existing look, or
   they are new values — and the difference is not a detail, because
   `chrome_alpha` and `chrome_backdrop_blur` are **shared by every floating
   surface**: changing them re-skins the bar, the library popup, the
   notification popover, the Switchboard readout and every control plate
   standing on them, including the two surfaces the requirement cites as
   already right.
   - **Reading A — match the reference surfaces.** Menus adopt the existing
     shared values; no theme constant moves and nothing else on the desktop
     changes appearance. The figures are read as the owner's description of the
     look, not as literals.
   - **Reading B — the figures are literal.** `CHROME_ALPHA` becomes 204
     (80%) and the blur becomes half the wire ceiling, 32 logical pixels — a
     four-and-a-half-fold increase in blur radius. Both are shared, so this is
     a deliberate re-skin of all floating chrome, and it wants a look at the
     cost: the blur is a separable box blur whose cost is proportional to the
     blurred *area* rather than the radius, but a wider radius still widens the
     window-sum build at each backdrop's edges, and the physical radius scales
     with the desktop's UI scale.
   - **Reading C — menus alone take new figures.** Rejected unless the owner
     asks for it: a second alpha and a second blur for menus would be two more
     theme values saying what `chrome_alpha` and `chrome_backdrop_blur` already
     say, and the requirement's own wording ("as per the program library list")
     asks for menus to match those surfaces rather than to differ from them.
   **Stop and ask before implementing** (§15.7): every reading changes what is
   touched, and B changes how the whole desktop looks.

---

## 7. Definition of done (whole plan)

- One chain can be open per seat, no app draws a menu pixel, and every menu
  in the desktop — the apps' and the desktop's own — comes from the one
  service.
- Every plate has a title band and drags by it; submenus open on arrival
  with no timer; the information panel hangs where a submenu would and dies
  with the chain.
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
