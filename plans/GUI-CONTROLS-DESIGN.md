# GUI Controls Design Specification: Reactive Alloy

Status: Design specification  
Audience: TAIRiX desktop, window manager, taskbar, application, and shared GUI crate contributors  
Primary product context: TAIRiX graphical session  
Scope: General GUI controls across TAIRiX, including but not limited to Switchboard  
Design language name: Reactive Alloy  
Tagline: Stable surfaces. Live edges. Clear intent. Confident actions.

---

## Assumptions

- This document specifies TAIRiX graphical controls, not kernel behavior and not a new system-call surface.
- The implementation belongs in the TAIRiX graphical userland and shared Rust crates already described by the charter: `userland/gui/wm`, `userland/gui/taskbar`, `userland/gui/session`, `lib/window`, `lib/theme`, `lib/geometry`, `lib/raster`, `lib/icon`, `lib/input`, and application crates that render their own GUI controls.
- Theme values, metrics, motion timings, and semantic colors are shared data. They are not duplicated per application.
- Controls render state and suggest actions, but authority remains enforced by the existing capability-checked syscall and IPC paths.
- Exact public Rust item names are established during implementation review. The Rust identifiers used here are specification vocabulary and must not be treated as committed API names until they are added to the tree with tests and documentation.
- The window manager owns outer window-frame and furniture rendering, hit testing, pointer capture, move and resize behavior, stacking actions, minimization, and size-state transitions. Applications provide typed metadata and receive typed events through the existing window path; they do not paint over or intercept window-manager chrome.
- A root client viewport may expose window-level scrollbars composed by the window manager. Nested scrollbars inside application content remain application controls. Both forms use the same theme tokens, range invariants, and orientation-independent behavior rather than separate vertical, horizontal, window-manager, and application recipes.

---

## 1. Purpose

Reactive Alloy is the TAIRiX GUI control design language for systems where the state around a control changes continuously: tasks appear and exit, background jobs progress, resource pressure rises, devices arrive, permissions differ, panels resize, and recovery actions become available.

The goal is to make controls feel alive without making them feel unstable.

A Reactive Alloy control communicates three things at a glance:

1. What action is available.
2. What surrounding state makes that action relevant.
3. Whether the action is safe, recommended, delayed, privileged, or destructive.

Switchboard is the flagship example because it exposes live task, job, recovery, and system state, but this specification is deliberately broader. The same language applies to buttons, toggles, sliders, fields, menus, tables, toolbars, taskbar items, notifications, dialogs, window frames, title bars, window furniture, scrollbars, and application controls.

### Every control and window-furniture item is first-class

Every control and every piece of window furniture named in this specification —
buttons (Button, IconButton, SplitButton), boolean selectors (Toggle, Checkbox,
Radio), value controls (Slider, Progress), text entry (TextField, SearchField),
choice entry (ComboBox), navigation and command surfaces (Menu, MenuItem,
Toolbar, Tabs), collection controls (ListRow, TableRow, TableCell, Card, Panel),
decision surfaces (Dialog, Tooltip, HelpTip), shell surfaces (Notification,
TaskbarItem, TraySignal), and the window-manager furniture (WindowFrame,
TitleBar, the WindowControl set — Close, Minimize, PutToBack, SizeToggle — the
ResizeGrabber, the ScrollBar in both orientations, and the ScrollCorner) — is a
**first-class control**. Each MUST be **fully implemented**: not stubbed, not a
"minimal for now" core, not a partial subset of the states §11 gives it, and
not something an application is expected to hand-roll.

"Fully implemented" for a control means all of the following are present, correct,
and tested before the control is considered done (§20, `AGENTS.md` §27):

- Every state §11 specifies for that control, composed from the typed §5 state
  model (never an ad-hoc per-control flag bag).
- Dark and light theme coverage, high-contrast shape fallbacks (§15), and
  reduced-motion behaviour (§9), all resolved from `Theme` and `Scale` (§6, §14).
- Its complete pointer, keyboard, and focus behaviour (§11, §15).
- Authority-denied, pending, failed-closed, and destructive rendering wherever the
  control can express them, distinct from a plain disabled state (§13).
- Its §20 tests, including the furniture and scrollbar checklists.

None of these controls is optional, deferrable, or reducible to a placeholder.
A control that is missing a specified state, a theme variant, an accessibility
fallback, or a keyboard path is incomplete and is a defect, regardless of
whether it compiles or its current call site exercises the missing part
(`AGENTS.md` §27, §23). The staged build order in `.junie/gui-controls-work.md`
sequences *when* each family lands; it never licenses shipping any of them in a
thinned-down form.

### Nothing is deferred, no-opped, or left "for now"

Every behaviour this specification describes is implemented properly in the
change that introduces it — never deferred to a "later stage", stubbed with a
`TODO`, or handled by a no-op match arm that silently drops input
(`AGENTS.md` §2.1, §2.17, §2.18, §2.19, §2.23, §27). This binds input paths as
much as rendering: a scrollable surface handles **every** input the spec gives
it — keyboard line/page/bound navigation, thumb drag, *and* the mouse wheel
(§11.28) — in the same change, not "wheel later". A control or input path
delivered as "keyboard today, wheel to follow" is exactly the deferral this
rule forbids. If the proper implementation depends on prerequisite work (an ABI
event that does not exist, a routing seam that is missing), that prerequisite is
completed as part of the same change; if it genuinely cannot be, the conflict is
raised with the User (`AGENTS.md` §15.7), never papered over with a temporary
gap. A wheel event that genuinely has nothing to scroll (a live terminal screen
that keeps no scrollback) is a *correct, complete* answer, not a deferral — but
"there is nowhere to route this yet" is not.

### Do not remove a control's genuinely useful public API

These crates are developer-facing: `lib/controls`, `userland/gui/wm`,
`lib/window`, and the app crates expose public control and window-furniture APIs
that third-party developers and a proper, complete UI legitimately depend on. A
public item that is part of a *complete, correct* control surface — a viewport's
`clear_root_viewport`, a scroll model's step and query methods, a furniture
hit-test — is kept even when the in-tree call sites are few or absent, because
removing it takes a genuinely useful primitive away from a consumer and makes
the control *less* than fully implemented (`AGENTS.md` §27, §15.5). This does
**not** license speculative surface (`AGENTS.md` §2.3, §2.4): the bar is "part
of a proper, complete control that a developer would reasonably use", never
"might be handy one day". When it is unclear whether an item is load-bearing API
or genuine dead code, keep the complete primitive and ask (`AGENTS.md` §15.7)
rather than delete it.

---

## 2. Design Position

Reactive Alloy is an instrument-panel language, not a decorative material language.

The surface should feel engineered: matte graphite, ceramic enamel, machined rims, lit seams, pressure rails, and compact signal lamps. It should not feel like liquid, rubber, jelly, or novelty gloss. Movement and lighting show actual state, not ornament.

### Core principle

Motion belongs to edges, traces, seams, and state indicators. Layout belongs to the user.

The body of a control remains reliable. The live perimeter tells the story.

### What makes it modern

A control is not just `Idle`, `Hovered`, `Pressed`, and `Disabled`. It can also know that a related job is running, that memory pressure is relevant, that a destructive operation needs deliberate confirmation, that a sibling row changed, or that the active theme switched.

Those signals must remain small, typed, and intentional. A control becomes modern by exposing useful system context, not by moving unpredictably.

---

## 3. TAIRiX Charter Alignment

Reactive Alloy must preserve the existing TAIRiX architecture.

### Rust-only implementation

All implementation is Rust. UI control logic is expressed as typed Rust state, Rust enums, Rust structs, Rust traits where justified, and Rust drawing code using TAIRiX crates. No design requirement in this document requires non-Rust source.

### Optional desktop

The graphical desktop remains optional. Controls live in userland GUI code and shared GUI-adjacent `lib/*` crates. Headless builds must not depend on GUI crates.

### One drawing path

Controls are drawn through the existing compositor and raster path. Rounded corners, alpha blending, vector glyph rasterisation, icon drawing, and cached assets must use the shared TAIRiX drawing stack rather than per-control copies.

### Theme data, not code forks

Dark, light, high-contrast, reduced-motion, and density variants are theme data. Adding a theme must not require adding a sibling control implementation or duplicating constants.

### DPI and scale

All lengths are authored in logical pixels and converted through `tairix_geometry::Scale`. A control must never carry a private scale conversion or assume a fixed physical pixel density.

### No ambient authority

A button can render `ActionDenied`, `ActionUnavailable`, or `NeedsCapability`, but it must not bypass permission checks. The service that performs the action remains responsible for identity, capability checks, validation, logging, and fail-closed behavior.

### No pseudo-files for live system state

Controls that display tasks, resources, device state, or limits consume typed TAIRiX state from the appropriate model or System Information API client. They must not scrape a fabricated process or device tree.

---

## 4. Ownership and Crate Boundaries

Reactive Alloy should be implemented as shared control behavior and theme data, not duplicated visual recipes.

| Concern | TAIRiX owner |
|---|---|
| Active theme, palette, metrics, motion timings, cursor selection | `lib/theme` and `userland/gui/session` |
| Logical geometry, rectangles, points, scaling | `lib/geometry` |
| Premultiplied-alpha surfaces, fills, polygons, blits | `lib/raster` |
| Shared icons and vector glyphs | `lib/icon` and curated asset pipeline |
| Pointer and keyboard input vocabulary | `lib/input` |
| Compositing, clipping, window surfaces, rounded windows, frame furniture, activation, stacking, move, and resize | `userland/gui/wm` |
| Typed window metadata, close requests, constraints, and root viewport exchange | `lib/window`, the owning application, and `userland/gui/wm` |
| Taskbar items, notification area, session controls | `userland/gui/taskbar` |
| Application-specific control composition | owning application crate |
| Shared system information client state | existing ABI and client helper crates |

The control system may be a shared GUI crate only when at least two independent consumers need the same control behavior. If only one application needs a custom control, the control stays in that application until there is a second real consumer.

---

## 5. Rust Terminology and State Model

Reactive Alloy controls are modeled as typed widgets with typed state. Avoid unstructured key/value bags for core state.

The following vocabulary is normative for the specification, not a frozen public API.

```rust
pub enum ControlKind {
    Button,
    IconButton,
    SplitButton,
    Toggle,
    Checkbox,
    Radio,
    Slider,
    Progress,
    TextField,
    SearchField,
    ComboBox,
    MenuItem,
    Tab,
    ListRow,
    TableCell,
    Card,
    Panel,
    DialogAction,
    WindowFrame,
    TitleBar,
    WindowControl,
    ResizeGrabber,
    ScrollBar,
    TaskbarItem,
    TraySignal,
    Notification,
}

pub enum ControlRole {
    Neutral,
    Primary,
    Recommended,
    Destructive,
    Recovery,
    Navigation,
    System,
}

pub struct ControlState {
    pub enabled: bool,
    pub focus: FocusState,
    pub pointer: PointerState,
    pub selection: SelectionState,
    pub validation: ValidationState,
    pub authority: AuthorityState,
    pub activity: ActivityState,
    pub pressure: PressureState,
    pub recovery: RecoveryState,
}

pub enum WindowControlKind {
    Close,
    Minimize,
    PutToBack,
    SizeToggle,
}

pub enum WindowActivationState {
    Active,
    Inactive,
    AttentionRequested,
}

pub enum WindowSizeState {
    Restored,
    Maximized,
}

pub enum ScrollOrientation {
    Vertical,
    Horizontal,
}

pub struct WindowFurnitureState {
    pub activation: WindowActivationState,
    pub size: WindowSizeState,
    pub movable: bool,
    pub resizable: bool,
}

pub struct ScrollRange {
    pub content_extent: u64,
    pub viewport_extent: u64,
    pub offset: u64,
}

pub struct ScrollModel {
    pub range: ScrollRange,
    pub line_step: u64,
    pub page_step: u64,
}
```

State composition is preferred over one enormous enum. A disabled destructive recovery button and a focused non-destructive primary button are different combinations of small typed fields, not unrelated custom code paths.

### Required state fields

| State field | Meaning |
|---|---|
| `FocusState` | Keyboard focus, active focus ring, focus field membership. |
| `PointerState` | None, hover, pressed, drag source, drag target. |
| `SelectionState` | Unselected, selected, mixed, current item. |
| `ValidationState` | Valid, warning, invalid, pending verification. |
| `AuthorityState` | Allowed, denied, needs confirmation, needs capability. |
| `ActivityState` | Idle, working, progress known, progress indeterminate, complete. |
| `PressureState` | CPU, memory, disk, network, power, thermal, or none. |
| `RecoveryState` | None, recoverable, hung, restart recommended, force action. |

### Window-furniture-specific state

| State | Meaning |
|---|---|
| `WindowActivationState` | Whether a frame is active, inactive, or requesting attention without stealing focus. |
| `WindowSizeState` | Restored or maximized. Fullscreen is a separate application/session mode and is not represented by the size-toggle control. |
| `WindowControlKind` | The exact window-manager command represented by a furniture button. |
| `ScrollOrientation` | Vertical or horizontal layout over one shared behavioral implementation. |
| `ScrollRange` | Content extent, viewport extent, and clamped offset used to derive thumb size and position. |
| `ScrollModel` | A validated range plus line-step and page-step distances in the same logical scroll unit. |

A `SizeToggle` renders the action that will occur next: `Maximize` while restored and `Restore` while maximized. A `ScrollRange` is normalized before painting or hit testing: when the viewport covers the content, the offset is zero; otherwise the offset cannot exceed `content_extent - viewport_extent`. Content extent, viewport extent, offset, line step, and page step use the same logical scroll unit declared by the owning viewport; they are not mixed implicitly between pixels, rows, or application records. Invalid, overflowing, or stale range data fails closed to a non-draggable, zero-offset scrollbar rather than producing out-of-bounds geometry.

---

## 6. Theme Model

Every visible property must resolve from the active `Theme` plus control state. A control must not hard-code colors, radii, font sizes, or animation timings outside test fixtures.

```rust
pub struct Theme {
    pub palette: Palette,
    pub metrics: Metrics,
    pub typography: Typography,
    pub motion: MotionTheme,
    pub controls: ControlThemeSet,
    pub cursors: CursorTheme,
}
```

### Themeable values

| Theme value | Examples |
|---|---|
| Palette roles | `surface`, `surface_elevated`, `surface_pressed`, `text`, `text_muted`, `rim`, `rim_active`, `accent`, `danger`, active and inactive window-frame roles, scroll track, and scroll thumb. |
| Semantic signal roles | `cpu_pressure`, `memory_pressure`, `disk_pressure`, `network_activity`, `recovery`, `success`, `warning`, `denied`. |
| Metrics | Control height, inset, gap, corner radius, border width, seam thickness, rail thickness, bead size, title-bar height, frame inset, window-control extent, resize-grabber extent, scrollbar breadth, minimum thumb length, and invisible hit slop. |
| Typography | Font family token, label size, caption size, numeric size, weight roles, active title weight, and inactive title weight. |
| Motion | Open duration, hover duration, press duration, progress tick cadence, window activation, minimize and size-toggle transitions, scrollbar wake timing, and reduced-motion policy. |
| Window furniture | Leading or trailing control placement, control order, title alignment, active/inactive treatment, frame profile, scrollbar placement, and grip geometry. |
| Density | Compact, normal, comfortable. |
| Contrast | Normal, high contrast, monochrome-safe signal shape fallback. |

A theme may place the window-control group on the leading or trailing side and may change its visual order, but it cannot change command meaning. The visible glyph, tooltip, accessibility name, and keyboard command for each control must continue to identify `Close`, `Minimize`, `PutToBack`, or the next `SizeToggle` action unambiguously.

### Theme variants

TAIRiX must ship dark and light variants. Additional variants are data over the same typed model. A variant may alter color, radius, density, and motion, but must not change the meaning of state.

For example, `PressureKind::Memory` remains the same state in every theme. One theme may render it purple, another may render it with a patterned rail. The semantic value stays typed.

### Semantic color discipline

Accent colors are not raw decoration. They map to state:

| Semantic role | Default meaning |
|---|---|
| Accent | Primary action, active selection, current route. |
| CPU pressure | Compute saturation or compute-heavy work. |
| Memory pressure | Memory pressure or memory-caused slowdown. |
| Disk pressure | Storage throughput, copy, indexing, verification. |
| Network activity | Transfer, sync, remote I/O. |
| Recovery | Hung, not responding, repair, restart, force action. |
| Success | Completed, verified, recovered. |
| Warning | Elevated impact, caution, delayed risk. |
| Denied | Missing authority or blocked action. |

A theme may map multiple semantic roles to the same hue only if it also provides a distinct shape, rail position, bead mark, or text label.

---

## 7. Reactive Alloy Visual Vocabulary

| Term | Meaning |
|---|---|
| Alloy Plate | The base matte control surface. |
| Signal Rim | The one-pixel or scaled reactive perimeter. |
| Heat Seam | A progress or activity line on an edge. |
| Pressure Rail | A side indicator showing resource pressure. |
| Signal Bead | A compact badge for counts, alerts, and state. |
| Recovery Latch | A deliberate high-impact action treatment. |
| Focus Field | A grouped focus highlight around a related control set. |
| Trace Line | A short-lived cause-and-effect connector. |
| Action Warmth | A stronger edge treatment for the recommended action. |
| Authority Mark | A locked, denied, or capability-required marker. |
| Frame Rim | The window-manager-owned active or inactive perimeter around a client surface. |
| Grip Teeth | A repeated notch shape that marks a resize grabber without relying on color. |
| Scroll Channel | The quiet track, page regions, and thumb that expose viewport position and extent. |

These are not separate widgets. They are rendering layers that any control can use when the control state requires them.

---

## 8. Drawing Stack

A Reactive Alloy control paints in ordered layers. Each layer is optional, but the order is fixed for consistency and testability.

1. Clip to control bounds and rounded shape.
2. Paint shadow or occlusion only when the theme enables elevation.
3. Paint the Alloy Plate.
4. Paint inner tint or subtle material grain if provided by the theme.
5. Paint Signal Rim.
6. Paint Pressure Rail.
7. Paint Heat Seam.
8. Paint content: icon, label, value, shortcut, disclosure mark.
9. Paint Signal Bead.
10. Paint focus ring or Focus Field.
11. Paint transient Trace Line overlays through the owning container.

All alpha values are premultiplied. All geometry passes through shared logical-to-physical scaling. Every clipped rounded edge uses the shared compositor/raster path.

### Window composition stack

A top-level window uses a second fixed composition order owned by `userland/gui/wm`:

1. Paint the window shadow or occlusion region.
2. Paint the frame plate and Frame Rim.
3. Blit the application surface into the client clip only.
4. Paint the title bar, title text, and application identity glyph.
5. Paint window-control buttons and their independent hover, press, focus, and disabled states.
6. Paint vertical and horizontal scrollbars when the root viewport exposes them.
7. Paint the scroll corner or ResizeGrabber above the scrollbar junction.
8. Paint the active-window focus treatment and transient attention signals.

The application surface cannot cover, clip, or receive pointer events from the outer frame, title bar, window controls, scrollbars owned by the root viewport, or resize grabber. The window manager maintains a separate furniture hit map so an application-drawn lookalike inside the client area cannot impersonate or intercept the actual frame controls.

---

## 9. Motion Model

Reactive Alloy motion is magnetic, not liquid.

Controls may lift, compress, brighten, or expose a seam. They must not wobble, slosh, stretch text, detach from the pointer, or shift layout unexpectedly.

### Timing targets

| Interaction | Target duration |
|---|---:|
| Hover enter | 90-120 ms |
| Hover exit | 80-110 ms |
| Press compression | 60-90 ms |
| Release settle | 90-130 ms |
| Panel open | 180-240 ms |
| Menu open | 120-180 ms |
| Job progress pulse | 120-180 ms, event driven |
| Recovery latch reveal | 180-260 ms |
| Window activate or deactivate | 90-140 ms |
| Minimize, restore, or size toggle | 160-240 ms |
| Scrollbar wake or settle | 70-120 ms |
| Theme switch repaint | One coherent frame sequence, no mixed half-theme frame |

### Reduced motion

The active `MotionTheme` must include a reduced-motion mode. In reduced motion, state still changes visibly, but through static contrast, rail thickness, shape marks, and text labels rather than animated transitions.

### Event-driven animation

Animation starts from state changes: pointer input, focus change, progress update, theme switch, or typed system state update. Controls must not run idle decorative loops.

Pointer-coupled movement is not animated behind the pointer. Window move, window resize, and scrollbar-thumb drag update geometry on the next available frame with no easing or delayed interpolation. A short release transition is permitted only for an explicit snap, maximize, restore, or minimize result, and it must be removed in reduced-motion mode.

---

## 10. Themeable Control Anatomy

A control is composed from a small, shared set of parts.

```text
ControlBounds
  AlloyPlate
  SignalRim
  PressureRail
  HeatSeam
  ContentGroup
    LeadingIcon
    Label
    ValueText
    ShortcutText
    TrailingIcon
  SignalBead
  FocusRing
```

### Required anatomy rules

- Content remains aligned while edge signals change.
- Label text never shifts because a Signal Rim brightened.
- Progress seams do not change the measured size of the control.
- Signal Beads reserve space only when persistent. Transient beads overlay inside the existing trailing inset.
- Focus rings are visible in every theme and do not rely on color alone.
- Destructive controls remain readable before and during confirmation.

### Window furniture anatomy

The order below is illustrative. A theme may place or reorder the command group while preserving command identity.

```text
WindowFrame
  FrameRim
  TitleBar
    ApplicationGlyph (optional)
    TitleText
    DragRegion
    WindowControls
      PutToBack
      Minimize
      SizeToggle
      Close
  ClientViewport
    ClientSurface
    VerticalScrollBar
    HorizontalScrollBar
    ScrollCorner or ResizeGrabber
```

### Required window-anatomy rules

- The title text truncates before it displaces a window control or removes the minimum drag region.
- Visible glyph size and pointer hit-target size are separate theme metrics; compact glyphs still receive a usable target.
- Hover, active, inactive, maximized, and attention states do not change the client origin or measured frame extents.
- A ResizeGrabber never overlaps a scrollbar thumb or client content. At a two-scrollbar junction, the corner cell belongs to the grabber or a neutral ScrollCorner.
- Vertical and horizontal scrollbars are one behavioral component parameterized by orientation. Their separate names exist for layout, accessibility, and testing, not as duplicated implementations.
- Root-viewport scrollbar visibility follows one declared policy: reserved gutter or overlay. The policy must not switch while the pointer is captured or while doing so would move content under an active interaction.

---

## 11. Component Specifications

Every component in this section is a **first-class control that must be fully
implemented** (§1): each ships with all the states listed for it, its dark/light
theme coverage, its high-contrast and reduced-motion behaviour, its complete
pointer/keyboard/focus handling, and its §20 tests. No component here is a
placeholder, an optional extra, or a "minimal for now" core (`AGENTS.md` §27,
§2.19). A component that omits a specified state or behaviour is incomplete and
is a defect (§20, `AGENTS.md` §23).

### 11.1 Button

Buttons are Alloy Plates with a Signal Rim and optional Heat Seam.

| State | Rendering |
|---|---|
| Idle | Matte plate, quiet rim, readable label. |
| Hover | Slight edge brightening, no layout movement. |
| Pressed | Firm compression, darker inner plate, label stable. |
| Primary | Accent rim, no broad glow unless focused. |
| Recommended | Action Warmth on the leading or lower edge. |
| Destructive | Danger rim, deliberate press timing, confirmation-aware. |
| Working | Heat Seam on the lower edge. |
| Denied | Authority Mark and explanatory text through tooltip or inline caption. |

A button should not use a spinner unless the action itself owns the work. If the work belongs to another object, use a linked Heat Seam instead.

### 11.2 IconButton

Icon buttons use the same state model as buttons. The icon must come from a theme-aware glyph source and must support high-contrast rendering.

Persistent badges sit on the trailing top corner. Transient beads charge from the nearest rim and settle into the badge position.

### 11.3 SplitButton

A split button contains a primary action region and a disclosure region. The two regions share one plate but expose separate focus and pointer states. The Signal Rim belongs to the whole control; the Heat Seam belongs to the primary action when the primary action is running.

### 11.4 Toggle

Toggles snap between states like a powered contact.

- The track is the Alloy Plate.
- The thumb is a smaller raised plate.
- The active side glows through an accent contact, not through a large wash.
- A denied toggle remains in its previous state and shows an Authority Mark.
- A pending toggle uses a Heat Seam while the backing service confirms the change.

### 11.5 Checkbox and Radio

Checkboxes and radio buttons must not rely on color alone. Use shape and fill:

- Checkbox checked: filled square mark.
- Checkbox mixed: horizontal mark.
- Radio selected: center bead.
- Warning or denied state: rail or rim plus label text.

### 11.6 Slider

Sliders are measured controls with a rail, value track, thumb, and optional semantic markers.

- The active range uses the theme accent.
- Resource sliders may use semantic rails, such as disk or memory.
- Dragging updates visual state immediately but commits through the owning model.
- A privileged or bounded value displays a lock or cap marker at the constrained edge.

### 11.7 Progress

Progress is an instrument trace, not decoration.

- Known progress: Heat Seam or bar with a stable percentage/value label.
- Indeterminate progress: bounded moving trace, disabled by reduced motion.
- Completed: success bead and static completion line.
- Failed: recovery or warning rim with concise reason.

Progress surfaces should expose throughput or remaining work only when the source model provides typed values.

### 11.8 TextField and SearchField

Text fields use a quiet Alloy Plate with a clear focus ring.

- Validation state appears as a rim segment and inline message.
- Search fields may show active query state through a small leading seam.
- Denied or read-only fields must be visually distinct from disabled fields.
- Cursor, selection, and text rendering are theme-driven and DPI-scaled.

### 11.9 ComboBox

A combo box is a field plus disclosure action. It uses the text field focus model and the menu model for expanded choices. Selection state belongs to the choice list, not to string parsing inside the control.

### 11.10 Menu and MenuItem

Menus are pinned command plates. They are not floating ornament.

- The menu plate uses elevated surface tokens.
- Each menu item is a row control with label, optional icon, shortcut, and state.
- Dangerous items use a danger rim only on their item row.
- Disabled items show the reason when focused or inspected.
- Nested menus open from the row edge with a short anchor trace.

### 11.11 Toolbar and Toolstrip

Toolbars are containers for IconButtons, SplitButtons, fields, and grouped actions.

- Group boundaries use quiet vertical gutters.
- The active tool has a persistent accent rim or lower seam.
- Background work belonging to a tool appears as a Heat Seam on that tool, not across the full toolbar.

### 11.12 Tabs

Tabs use a lower seam for selected state.

- Selected tab: strong lower seam and clear label weight.
- Modified tab: small Signal Bead.
- Loading tab: Heat Seam on lower edge.
- Error tab: warning or recovery bead with accessible label.

### 11.13 ListRow and TableRow

Rows are controls. They can be selected, focused, inspected, dragged, or linked to actions.

- Hover uses a quiet plate tint.
- Selection uses a left rail plus background tint.
- Live activity uses a Heat Seam at the bottom of the row.
- Resource pressure uses a semantic rail on the leading edge.
- Recovery state uses a sharper bead or latch affordance.

Tables must keep columns aligned while row state changes.

### 11.14 TableCell

A table cell may expose its own state only when that state is cell-specific. Row-wide state belongs to the row. Numeric cells should use a tabular numeric font role when available.

### 11.15 Card

Cards group state and actions.

- The leading edge carries the dominant state.
- The bottom edge carries progress.
- The top trailing corner carries count or alert beads.
- Footer actions share the card's semantic state but keep their own pointer and focus states.
- A card may carry an optional identifying glyph above its title (e.g. a file
  manager grid tile's file-type icon); when present, the title and body centre
  beneath it. A card with no glyph keeps its title at the top, so
  notification/resource cards are unaffected.

### 11.16 Panel

Panels are containers with stable layout. A panel may have a Focus Field, header state, grouped actions, and scrollable content. A panel opening from a taskbar or tray item should retain an anchor notch or route line to its invoker while open.

### 11.17 WindowFrame

A `WindowFrame` is the window-manager-owned boundary around one client viewport.

- The active frame uses the active Frame Rim, stronger title contrast, and a non-color focus distinction such as a double rim or title-weight change.
- The inactive frame remains legible and structurally complete, but its accent treatment is quieter.
- An attention request adds a bounded Signal Bead or rim segment. It does not steal focus and does not pulse indefinitely.
- Client pixels are clipped to the client viewport and never paint into the title bar, borders, root scrollbars, or resize grabber.
- Frame activation, theme change, and hover do not change the client origin or outer dimensions.
- Maximized geometry uses the session work area and therefore respects taskbars, reserved screen edges, and the current logical scale.
- The frame owns the hit map for move, resize, command buttons, and any root-viewport scrollbars.

### 11.18 TitleBar

The title bar combines application identity, title text, a stable drag region, and the window-control group.

- The title text uses a single line and truncates with an ellipsis before it overlaps controls or removes the minimum drag region.
- Pressing an inactive title bar activates the window. Movement beyond the theme drag threshold begins a move and captures the pointer until release or cancel.
- A title-bar drag follows the pointer without easing. Snap previews may appear as container-owned overlays without moving the pointer target.
- A double-click or equivalent gesture may invoke `SizeToggle` only when session policy enables it. The explicit size-toggle button remains required.
- The title bar exposes the application name and current window title to accessibility tools even when the visible title is truncated.
- Window titles are untrusted application data: the window manager bounds their length, renders them as plain text, rejects or replaces control characters, and applies the text engine's directional-isolation rules rather than interpreting markup.
- Attention state is shown with a bounded bead or rim segment, not a decorative loop.

#### Shared window-control states

The close, minimize, put-to-back, and size-toggle controls are compact `WindowControl` instances built from the shared `IconButton` behavior.

| State | Rendering and behavior |
|---|---|
| Idle, active frame | Quiet plate, readable glyph, and frame-consistent rim. |
| Idle, inactive frame | Lower contrast than the active frame while remaining legible. |
| Hover | Glyph and local rim brighten without changing title-bar geometry. |
| Pressed | Firm compression and captured press state until release or cancel. |
| Keyboard focus | Visible focus ring distinct from hover and window activation. |
| Disabled | Muted plate and glyph, no command dispatch, and an inspectable reason. |

Pressing a window control on an inactive frame activates that frame and arms the same control in one interaction. Releasing over the armed control invokes it; moving away or cancelling does not. The press is never forwarded into the client surface.

### 11.19 CloseButton

The close button represents `WindowControlKind::Close` and is not a force-termination control.

- Activation sends a typed cooperative close request to the owning application.
- The application may close immediately, reject the request with a user-facing reason, or present an unsaved-work decision surface while keeping the window open.
- A non-responsive application remains a recovery case. `Force Action` or process termination uses the separate destructive recovery path and its capability checks.
- The close glyph and accessible label identify `Close <window title>`. A theme may use danger emphasis on hover or press, but the idle button need not appear permanently destructive.
- A non-closable surface retains the control slot and renders it disabled with an explanation. Close availability must not shift neighboring title-bar controls.

### 11.20 MinimizeButton

The minimize button represents `WindowControlKind::Minimize`.

- Activation removes the window from the current workspace view while keeping the application, task, and background work alive.
- The corresponding taskbar item remains available and exposes the minimized state. Restoring through the taskbar returns the same window rather than creating a new one.
- The restored rectangle is preserved independently of the maximized rectangle.
- A minimize transition may route visually toward the taskbar when motion is enabled. Reduced-motion mode changes state immediately without a travel animation.
- Minimize is distinct from `PutToBack`: minimized windows are hidden from the workspace; put-to-back windows remain visible when not covered.

### 11.21 PutToBackButton

The put-to-back button represents `WindowControlKind::PutToBack`.

- Activation moves the window to the bottom of the normal stacking order for its current workspace and activates the next eligible window.
- The window remains mapped, visible where not occluded, and represented by the same taskbar item. Its process and jobs are unaffected.
- The glyph uses stacked plates with a backward or downward cue, and the accessible label is `Put window to back`.
- Modal ownership, pinned system surfaces, or session policy may disable the action. The disabled state explains the constraint.
- Repeated activation is idempotent once the window is already at the back of its allowed stack.

### 11.22 SizeToggleButton

The size-toggle button represents `WindowControlKind::SizeToggle`.

- In `WindowSizeState::Restored`, the glyph and accessible label describe the next action: `Maximize`.
- In `WindowSizeState::Maximized`, the glyph and accessible label describe the next action: `Restore`.
- Maximize fills the current session work area, not the physical display bounds, and is not fullscreen.
- Restore returns to the saved logical rectangle. If the work area, scale, or display arrangement changed, the window manager revalidates and clamps that rectangle so a usable title bar remains reachable.
- Fixed-size or otherwise non-resizable windows render the control disabled with a concise reason.
- The transition preserves client content and scroll position. Reduced-motion mode uses an immediate geometry change.

### 11.23 ResizeGrabber

The resize grabber is an explicit, visible corner affordance for resizable windows.

- It appears at the logical bottom-trailing corner by default and uses Grip Teeth or another shape mark that remains visible without color.
- Its visible size and pointer hit region are separate. The hit region may extend invisibly into the frame but never into another control or scrollbar thumb.
- Press and drag capture the pointer until release or cancel. Geometry follows the pointer on the next frame with no easing.
- The window manager enforces typed minimum, maximum, aspect, and work-area constraints before presenting each new rectangle.
- When both root scrollbars are visible, the grabber owns their junction cell. A non-resizable window uses a neutral `ScrollCorner` there instead.
- Maximized and non-resizable windows hide or disable the grabber consistently with the active theme.
- A keyboard resize command remains available through the window or system menu, so resize does not depend on precise pointer use.
- Frame edges may also expose resize zones, but they share this same constraint, cursor, pointer-capture, and test model rather than implementing a second resize path.

### 11.24 Dialog

Dialogs are decision surfaces.

- The primary action is visually warm only when it is the recommended safe action.
- Destructive actions use a Recovery Latch or deliberate confirmation step.
- Disabled actions explain why through inline copy or a focused explanation.
- Dialogs must not hide capability denial behind generic disabled state.

### 11.25 Notification

Notifications use cards with semantic beads. They should remain compact and actionable.

- Informational: quiet rim.
- Background job: Heat Seam.
- Warning: warning rail.
- Recovery available: recovery bead and clear action.
- Denied action: Authority Mark with source application or service name.

### 11.26 TaskbarItem

Taskbar items combine application identity (icon and label), activity,
attention, and window-visibility state on one Alloy Plate.

#### Presentation

A taskbar item supports two presentations ([`TaskbarPresentation`]):

- **IconAndLabel** — leading icon beside a truncated label. Used for wide
  running-task buttons.
- **Icon** — a centred icon filling the plate. Used for compact pinned-shortcut
  slots. The label stays part of the model for context surfaces (tooltips,
  menus) to read.

#### Visibility states

A taskbar item's window-visibility state ([`TaskVisibility`]):

- **Running** — visible but not the active window. Plate visible.
- **Active** — the focused window. Shown with a lower accent seam.
- **Minimized** — recessed plate and a distinct non-color mark (short muted
  tick on the leading edge).
- **Closed** — a pinned shortcut whose application is not running. The plate
  stays quiet (bar-coloured, no rim) until hovered or focused, so a
  launcher-only slot never masquerades as a running task.

#### Anatomy and artwork

The item draws its identity from a built-in class glyph tinted for the
resolved frame, or from **owner-supplied artwork** (pre-rasterised pixels).
The control never parses image bytes; [`icon_side`] exposes the exact pixel
geometry (sized off the text line for labelled items, or the plate for
icon-only items) so owners rasterise at exactly the drawn size.

#### Statusfurniture

Background work shows a Heat Seam on the lower edge (just above the active
seam if present). An attention request or recovery/denied state shows a
shape-coded Signal Bead on the top-trailing corner.

### 11.27 TraySignal

A tray signal is a compact live status capsule.

- Normal: calm glyph and quiet rim.
- Background work: lower Heat Seam.
- Pressure: side rail in semantic role.
- Recovery: recovery bead.
- Multiple states: stacked mini beads, ordered by severity.
- As built, live badge: an optional top-trailing filled count/alert badge —
  a count capped at "9+", or an exclamation mark for a countless urgent
  state (a hung app) — toned accent (background job), warning (pressure),
  danger (hung, the destructive role's red), or recovery. It shares the one
  badge painter with the §11.25 card count badge; the mini-bead stack starts
  after it, hiding nothing.

A tray signal expands to an instrument readout on hover or focus. The readout must be short: state name, count or value, and primary safe action.

### 11.28 ScrollBar Common Behavior

Vertical and horizontal scrollbars are one orientation-parameterized control. Window-level and embedded variants share the same range validation, thumb math, input behavior, focus model, and theme values.

```text
ScrollBar
  DecrementButton
  TrackBeforeThumb
  Thumb
  TrackAfterThumb
  IncrementButton
```

- The thumb length is proportional to `viewport_extent / content_extent`, subject to the theme's minimum thumb length and the space required by end controls.
- The thumb position maps the clamped `offset` across the draggable track. The same mapping is used in paint, hit testing, keyboard updates, and tests.
- When the viewport covers the content, the offset is zero and the bar follows the declared layout policy: hidden, or a reserved quiet gutter with a non-draggable thumb. It must not oscillate between policies as content changes by a pixel.
- Idle: low-contrast thumb and quiet Scroll Channel.
- Hover, keyboard focus, wheel input, or active scrolling: thumb and relevant end control brighten without changing geometry.
- Thumb drag captures the pointer and preserves the initial pointer-to-thumb anchor so the content does not jump when the drag begins.
- A decrement or increment control performs one typed line step. A track region performs one page step in its direction. Press-and-hold repetition uses a one-shot timer and event-driven wakeups, never a polling loop.
- Mouse wheel, touchpad, keyboard, and accessibility actions update the same scroll model. The control does not maintain a private offset separate from the owning viewport.
- If content extent changes during thumb drag, the control recomputes the range from the preserved drag anchor, clamps the result, and never produces an invalid offset.
- Content updates do not animate the thumb unless the user is actively looking at or manipulating the scrollbar. Reduced-motion mode uses immediate position changes.
- A focused scrollbar supports arrow keys for line steps, Page Up or Page Down for page steps, and Home or End for the range bounds, interpreted by orientation.

### 11.29 VerticalScrollBar

A vertical scrollbar controls the viewport's vertical offset.

- It sits on the logical trailing edge by default. A right-to-left session policy may mirror it to the leading edge without changing command meaning.
- The decrement control means `Scroll up`; the increment control means `Scroll down`.
- The thumb moves only on the vertical axis, and its accessible value reports the vertical position and range.
- Vertical wheel or touchpad input routes to the nearest eligible vertical viewport under the existing input-routing rules.
- Edge Wake may appear on the top or bottom client edge to show that more content exists beyond the viewport.

### 11.30 HorizontalScrollBar

A horizontal scrollbar controls the viewport's horizontal offset.

- It sits on the logical bottom edge by default.
- The decrement control means `Scroll toward the logical start`; the increment control means `Scroll toward the logical end`. Glyph direction mirrors with layout direction while accessible names remain semantic.
- The thumb moves only on the horizontal axis, and its accessible value reports the horizontal position and range.
- Horizontal touchpad input and any session-defined modified-wheel gesture route to the nearest eligible horizontal viewport.
- Edge Wake may appear on the leading or trailing client edge to show off-screen content.

### 11.31 ScrollCorner

A `ScrollCorner` occupies the junction between visible vertical and horizontal scrollbars.

- On a resizable top-level window it is replaced by, or visually integrated with, the `ResizeGrabber` while retaining one unambiguous hit target.
- On a non-resizable window it is a neutral Alloy Plate with no hidden scroll or resize action.
- It never overlaps either thumb and never receives line-step or page-step input intended for a scrollbar track.

### 11.32 Tooltip and HelpTip

Tooltips explain immediate affordance. HelpTips explain why an action is unavailable or recommended.

- Tooltips are short and anchored.
- HelpTips may include one reason and one safe next step.
- Security-sensitive denial text must avoid secrets and capability tokens.

---

## 12. Reactive State Patterns

### 12.1 Edge Wake

When content scrolls or rearranges near an anchored control, the edge nearest the movement can briefly brighten. The control does not move. This confirms that the control stayed anchored while the surrounding state changed.

Use Edge Wake for taskbar controls, sticky table headers, panel actions, and pinned toolbars.

### 12.2 Progress Seam

A related job paints progress on the lower edge of its object and on actions that operate on that job.

Example: a file copy row and its `Pause`, `Cancel`, and `OpenDestination` actions share the same progress identity. The `Pause` button shows the strongest seam because it operates on the running job. `Cancel` shows a weaker seam and a danger hover rim.

### 12.3 Pressure Rail

Resource pressure is directional and semantic.

- CPU pressure: compute rail.
- Memory pressure: memory rail.
- Disk pressure: storage rail.
- Network activity: transfer rail.
- Thermal or power pressure: system rail.

The rail appears on the object causing or experiencing the pressure and on the recommended action.

### 12.4 Signal Bead

A Signal Bead is a compact state lamp.

- Count bead: number of queued or active items.
- Alert bead: warning or recovery mark.
- Authority bead: lock or denied mark.
- Completion bead: success mark.

A bead must have an accessible text equivalent.

### 12.5 Trace Line

Trace lines connect cause to action briefly. They are owned by the container, not by individual controls.

Example: selecting a high-memory process may briefly route from the row to the memory pressure card and then to a `SleepApp` action.

Trace lines are short-lived, reduced-motion aware, and never required to understand the UI.

### 12.6 Action Warmth

When the model can identify a safe recommended action, that action receives a warmer rim or leading edge. Competing actions remain visible but quieter.

Action Warmth must never imply authority. A recommended action can still be denied by capability checks after activation.

### 12.7 Recovery Latch

A recovery action is a deliberate control state for hung or broken work.

- Soft recovery: normal button with recovery rim.
- Restart: Recovery Latch with stronger perimeter and deliberate press timing.
- Force action: danger rim, confirmation posture, no playful movement.

### 12.8 Frame Activation

The active window receives the strongest Frame Rim and title treatment. Inactive windows retain complete furniture with quieter contrast. An application requesting attention receives a bounded bead or rim segment without stealing focus or starting an indefinite pulse. Activation state never changes frame measurements.

### 12.9 Scroll Edge Wake

When scrolling starts, the relevant Scroll Channel and the client edge in the direction of travel may brighten briefly. The thumb remains the authoritative position indicator. Reduced-motion mode keeps the wake static only while input is active, then returns directly to idle.

---

## 13. Authority and Security Rendering

Controls must distinguish these cases:

| Case | Rendering | Behavior |
|---|---|---|
| `DisabledByState` | Muted plate and label | No action because the object state makes it invalid. |
| `DeniedByAuthority` | Authority Mark plus reason | No action because the caller lacks authority. |
| `NeedsConfirmation` | Active control with deliberate confirmation posture | Action is possible but consequential. |
| `PendingCheck` | Heat Seam or verification mark | Awaiting backing service response. |
| `FailedClosed` | Warning or recovery state with typed reason | Action was refused safely. |

Never render an authority denial as though the control is merely inactive. Users should be able to understand whether they cannot act because the object is done, because the action is not valid, or because they lack authority.

Security-sensitive controls must not display secrets, raw capability tokens, or hidden policy internals. They may show concise user-facing reasons such as "requires system permission" or "action blocked by policy".

Window furniture does not create authority. The window manager validates that a furniture event targets a live window owned by the addressed client and that the client cannot issue frame commands against another owner's window. Cooperative close, minimize, put-to-back, maximize, restore, move, resize, and scroll dispatch remain userland window operations. Force termination remains the distinct capability-checked recovery path. Root viewport ranges and resize constraints are validated and clamped before they influence geometry.

---

## 14. Layout, Density, and Scale

### Logical pixels

All dimensions are logical. The control code receives `Scale` and derives physical sizes through the shared conversion path.

### Density modes

| Density | Intended use |
|---|---|
| Compact | Tables, task lists, sidebars, dense system panels. |
| Normal | Default desktop applications. |
| Comfortable | Touch-adjacent or distance-viewed surfaces. |

Density changes metrics, not state semantics.

### Minimum targets

Interactive controls must meet the active theme's minimum target size. A dense table row may have smaller visual height only when a larger row target is supplied by row selection or keyboard focus behavior.

### Text stability

Labels, shortcuts, values, and icons keep their position while rims, rails, beads, and seams animate. Any value that changes frequently should use fixed-width numeric glyphs when available.

### Window frame geometry

- Title-bar height, frame inset, control extent, scrollbar breadth, corner cell, and resize hit slop are logical theme metrics.
- Active, inactive, hover, attention, and maximized states do not change the client origin or frame extents.
- The work-area clamp always leaves a usable title-bar region reachable after display, scale, or taskbar changes.
- When space is constrained, title text truncates first. Window-command hit targets and the minimum drag region remain usable.
- Overlay scrollbar hit regions must not cover title-bar controls, the resize grabber, or unrelated client actions. Reserved-gutter scrollbars must not resize the client in response to hover alone.

---

## 15. Accessibility

Reactive Alloy must be usable without color, without motion, and with keyboard input.

### Required accessibility behavior

- Every semantic color role has a non-color mark.
- Focus is visible and distinct from hover.
- Keyboard navigation reaches every action that pointer input can reach.
- Reduced motion converts animation into static state changes.
- High contrast increases rim, rail, and text contrast before adding more glow.
- Progress exposes text or numeric state when known.
- Count beads have text equivalents.
- Denied and destructive states have explicit labels or descriptions.
- Every window command has an accessible name that describes the action, not only its glyph.
- The size-toggle name and glyph describe the next action: Maximize or Restore.
- Active and inactive windows remain distinguishable without color.
- Window move, close, minimize, put-to-back, size toggle, and keyboard resize are reachable without a pointer through the established window or system menu path.
- Scrollbars expose orientation, current value, minimum, maximum, and page extent, and support keyboard line, page, and bound navigation.
- Resize grabbers use a visible shape mark and an enlarged target in comfortable density.

### Shape fallbacks

| Semantic state | Shape fallback |
|---|---|
| CPU pressure | short vertical rail ticks |
| Memory pressure | double rail |
| Disk pressure | lower seam plus storage glyph |
| Network activity | alternating dot marks |
| Recovery | diamond bead or latch outline |
| Success | check bead |
| Denied | lock bead |
| Active window | double Frame Rim, title-weight change, or another non-color frame distinction |
| Resize affordance | Grip Teeth in the corner |
| Scroll position | proportional thumb with accessible numeric range |

---

## 16. System Integration

### Active theme flow

`userland/gui/session` owns the active theme selection and relays it to the window manager, taskbar, and GUI applications. Controls listen for theme changes through the existing session or application model, then repaint through the normal surface path.

### Window furniture flow

`userland/gui/wm` owns the frame composition, furniture hit map, activation, stacking, move and resize capture, minimize state, maximize and restore geometry, and root-viewport scrollbar composition. The existing window path carries typed application metadata and events; it does not add a GUI-specific syscall.

The owning application provides the current title, optional application glyph reference, sizing constraints, and declared close, minimize, and resize support for that window class. The window manager and session derive put-to-back and size-toggle availability from stacking, modal, work-area, and sizing policy rather than accepting arbitrary z-order policy from the client. When a top-level client exposes a root viewport, it also provides a bounded scroll model and receives typed scroll requests. The window manager validates every range and constraint, then emits application-directed actions only to that window's owner.

Close is cooperative and application-directed. Minimize, put-to-back, activation, and size state are window-manager/session state. A hung close request may make recovery available, but it never silently converts into force termination. Nested application scrollbars use the same control specification and theme data while remaining inside the client surface.

### Application bundles

Applications may ship resources in their own bundle. Control visuals that are part of the shared TAIRiX design language belong in the OS-provided shared crates or curated assets, not copied into every application bundle.

### System state models

Controls that render live CPU, memory, disk, network, task, device, or limit information consume typed TAIRiX data. A control should receive a view model such as `TaskSummary`, `JobProgress`, `PressureSample`, `AuthorityStatus`, or `RecoveryRecommendation`, rather than opening devices or probing system state itself.

### Actions

Controls emit typed userland actions. The receiving service performs the operation, checks authority, validates input, logs security-relevant decisions, and returns typed success or error state. The control updates itself from that returned state.

---

## 17. Switchboard as a Reference Composition

Switchboard should use the same general controls as every other TAIRiX surface.

### Window frame and viewport

- The top-level Switchboard window uses the standard `WindowFrame` and `TitleBar`; it does not ship custom application-painted chrome.
- Close, minimize, put-to-back, and size toggle are the standard window-manager controls with the same glyph, focus, tooltip, and keyboard semantics as every other TAIRiX window.
- When content exceeds the client viewport, the standard vertical or horizontal scrollbar appears according to the root viewport model.
- A resizable window exposes the standard ResizeGrabber at the frame corner or scrollbar junction.
- Minimize keeps background jobs active and visible through the taskbar item; close remains a cooperative application request.

### Task list

- `ListRow` for each task.
- Activity sparkline as row content, not a custom state engine.
- Resource pressure as `PressureRail` on the row.
- Hung or recovery state as `SignalBead` and `RecoveryState`.
- Row actions as standard `Button` or `IconButton` controls.

### Background jobs

- `Card` or `ListRow` for each job.
- Known progress as `HeatSeam` and numeric text.
- Job actions as `Button` controls sharing the job's progress identity.
- Open destination or inspect output actions use quiet Action Warmth only when useful.

### Recovery

- Hung object rows use recovery beads and leading recovery rails.
- Restart uses `RecoveryLatch` treatment.
- Force action uses destructive role with confirmation posture.
- Timeline and logs use standard tab, row, and panel controls.

### System overview

- Resource cards use semantic rails and numeric content.
- Service rows use `ListRow` with state bead and capability-aware actions.
- System actions use `Button`, `IconButton`, and `MenuItem` roles.
- Shutdown or lock actions use destructive or system roles, not custom artwork.

---

## 18. Rendering Examples in Text

These examples describe shape and state. They are not implementation syntax.

### Idle primary button

```text
[ Restart ]
quiet plate + accent rim
```

### Recommended action under memory pressure

```text
[ Sleep App ]
left memory rail + warm rim
```

### Running job action

```text
[ Pause ]
lower heat seam follows job progress
```

### Destructive recovery action

```text
[ Force Action ]
recovery latch + danger rim + deliberate press
```

### Tray signal with multiple states

```text
[ Signals ] 2 jobs + 1 recovery
lower heat seam + recovery bead
```

### Active window furniture

The order is illustrative; theme data may place the command group elsewhere.

```text
+--[back]--[min]-- Switchboard --[restore]--[close]--+
| client viewport                                  |^|
|                                                  |#|
|<----------- horizontal thumb ----------->| grip |v|
+---------------------------------------------------+
active Frame Rim + stable title + separate hit targets
```

### Inactive window furniture

```text
same geometry + quieter Frame Rim + complete controls
```

---

## 19. Do and Do Not

### Do

- Use typed Rust state for control state.
- Resolve visuals from `Theme`, `Scale`, and semantic roles.
- Keep layout stable while state indicators react.
- Share constants, metrics, drawing helpers, and semantic mappings.
- Keep live state in view models supplied by services or owning containers.
- Make every state accessible without color or motion.
- Let services enforce authority and return typed results.
- Keep outer frame furniture and its hit map owned by the window manager.
- Keep Close, Minimize, PutToBack, and SizeToggle as distinct typed commands.
- Preserve restored geometry and validate it when the work area or scale changes.
- Share one scrollbar range, mapping, and input implementation across orientations and owners.
- Make the resize grabber and scrollbar thumb visible, focusable where appropriate, and usable at every density.

### Do not

- Hard-code colors, radii, timings, or scale conversions in application controls.
- Duplicate a visual recipe across multiple crates.
- Add a GUI-specific syscall for a control action.
- Make non-GUI code depend on `userland/gui/*`.
- Use random pulsing, idle shimmer, wobble, or layout drift.
- Treat a denied action as a generic disabled state.
- Hide live system information behind an untyped text scrape.
- Add a new public interface solely to make a control easier to draw.
- Let application content paint over or intercept window-manager furniture.
- Treat Close as force termination, or treat Minimize and PutToBack as the same action.
- Duplicate vertical and horizontal scrollbar logic.
- Animate window move, resize, or thumb drag behind the pointer.
- Hide the only resize affordance in a one-pixel invisible border.
- Change the title-bar or client geometry merely because activation, hover, or attention state changed.
- Defer, stub, or no-op any specified input path (most commonly the mouse wheel): a scrollable surface handles keyboard, thumb drag, and the wheel in the same change, never "keyboard now, wheel later".
- Delete a genuinely useful public control or window-furniture API (for example a viewport's `clear_root_viewport`) merely because its in-tree call sites are few; it stays for the developers and complete UIs that depend on it.

---

## 20. Implementation Checklist

A control or control family is ready when the following are true:

- State is represented by small Rust types with clear ownership.
- Visuals resolve from the active `Theme` and `Scale`.
- Drawing uses the shared raster and compositor path.
- The control has dark and light theme coverage.
- High contrast and reduced motion are defined.
- Pointer, keyboard, and focus behavior are specified.
- Authority-denied, pending, failed-closed, and destructive states are specified where relevant.
- Progress and pressure state come from typed models.
- Tests cover measurement, state transitions, theme switching, reduced motion, and denied actions.
- Documentation explains the control's public behavior and the meaning of each state.
- Window-frame tests cover active, inactive, attention, maximize, restore, minimize, put-to-back, cooperative close, and disabled command states.
- Move, resize, and scrollbar-thumb tests cover pointer capture, cancellation, exact pointer tracking, and constraint clamping.
- Scrollbar tests cover zero overflow, proportional thumb math, minimum thumb size, line and page steps, range changes during drag, both orientations, and keyboard access.
- Restore-rectangle tests cover work-area, display, and logical-scale changes while keeping the title bar reachable.
- Hit-map tests prove that client content cannot receive outer-furniture input and that the resize corner does not overlap either scrollbar.

---

## 21. Acceptance Criteria for Reactive Alloy

Reactive Alloy succeeds when a user can answer these questions without reading a manual:

- What is active?
- What changed?
- What is under pressure?
- What action is safe?
- What action is consequential?
- What action is blocked by authority?
- What will keep running if I leave this panel?
- Which window is active?
- How do I close, minimize, put to back, maximize, restore, or resize this window?
- Will Close ask the application to finish safely, or is a separate force action required?
- Where am I in vertically or horizontally scrolled content, and how much remains?
- Will the size toggle return me to the window's previous usable rectangle?

The design is allowed to be rich. It is not allowed to be noisy. TAIRiX controls should feel grounded, typed, secure, and alive at the edges.

