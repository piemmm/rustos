# GUI Controls Design Specification: Reactive Alloy

Status: Design specification  
Audience: RustOS desktop, window manager, taskbar, application, and shared GUI crate contributors  
Primary product context: RustOS graphical session  
Scope: General GUI controls across RustOS, including but not limited to Switchboard  
Design language name: Reactive Alloy  
Tagline: Stable surfaces. Live edges. Clear intent. Confident actions.

---

## Assumptions

- This document specifies RustOS graphical controls, not kernel behavior and not a new system-call surface.
- The implementation belongs in the RustOS graphical userland and shared Rust crates already described by the charter: `userland/gui/wm`, `userland/gui/taskbar`, `userland/gui/session`, `lib/theme`, `lib/geometry`, `lib/raster`, `lib/icon`, `lib/input`, and application crates that render their own GUI controls.
- Theme values, metrics, motion timings, and semantic colors are shared data. They are not duplicated per application.
- Controls render state and suggest actions, but authority remains enforced by the existing capability-checked syscall and IPC paths.
- Exact public Rust item names are established during implementation review. The Rust identifiers used here are specification vocabulary and must not be treated as committed API names until they are added to the tree with tests and documentation.

---

## 1. Purpose

Reactive Alloy is the RustOS GUI control design language for systems where the state around a control changes continuously: tasks appear and exit, background jobs progress, resource pressure rises, devices arrive, permissions differ, panels resize, and recovery actions become available.

The goal is to make controls feel alive without making them feel unstable.

A Reactive Alloy control communicates three things at a glance:

1. What action is available.
2. What surrounding state makes that action relevant.
3. Whether the action is safe, recommended, delayed, privileged, or destructive.

Switchboard is the flagship example because it exposes live task, job, recovery, and system state, but this specification is deliberately broader. The same language applies to buttons, toggles, sliders, fields, menus, tables, toolbars, taskbar items, notifications, dialogs, and application controls.

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

## 3. RustOS Charter Alignment

Reactive Alloy must preserve the existing RustOS architecture.

### Rust-only implementation

All implementation is Rust. UI control logic is expressed as typed Rust state, Rust enums, Rust structs, Rust traits where justified, and Rust drawing code using RustOS crates. No design requirement in this document requires non-Rust source.

### Optional desktop

The graphical desktop remains optional. Controls live in userland GUI code and shared GUI-adjacent `lib/*` crates. Headless builds must not depend on GUI crates.

### One drawing path

Controls are drawn through the existing compositor and raster path. Rounded corners, alpha blending, vector glyph rasterisation, icon drawing, and cached assets must use the shared RustOS drawing stack rather than per-control copies.

### Theme data, not code forks

Dark, light, high-contrast, reduced-motion, and density variants are theme data. Adding a theme must not require adding a sibling control implementation or duplicating constants.

### DPI and scale

All lengths are authored in logical pixels and converted through `rustos_geometry::Scale`. A control must never carry a private scale conversion or assume a fixed physical pixel density.

### No ambient authority

A button can render `ActionDenied`, `ActionUnavailable`, or `NeedsCapability`, but it must not bypass permission checks. The service that performs the action remains responsible for identity, capability checks, validation, logging, and fail-closed behavior.

### No pseudo-files for live system state

Controls that display tasks, resources, device state, or limits consume typed RustOS state from the appropriate model or System Information API client. They must not scrape a fabricated process or device tree.

---

## 4. Ownership and Crate Boundaries

Reactive Alloy should be implemented as shared control behavior and theme data, not duplicated visual recipes.

| Concern | RustOS owner |
|---|---|
| Active theme, palette, metrics, motion timings, cursor selection | `lib/theme` and `userland/gui/session` |
| Logical geometry, rectangles, points, scaling | `lib/geometry` |
| Premultiplied-alpha surfaces, fills, polygons, blits | `lib/raster` |
| Shared icons and vector glyphs | `lib/icon` and curated asset pipeline |
| Pointer and keyboard input vocabulary | `lib/input` |
| Compositing, clipping, window surfaces, rounded windows | `userland/gui/wm` |
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
| Palette roles | `surface`, `surface_elevated`, `surface_pressed`, `text`, `text_muted`, `rim`, `rim_active`, `accent`, `danger`. |
| Semantic signal roles | `cpu_pressure`, `memory_pressure`, `disk_pressure`, `network_activity`, `recovery`, `success`, `warning`, `denied`. |
| Metrics | Control height, inset, gap, corner radius, border width, seam thickness, rail thickness, bead size. |
| Typography | Font family token, label size, caption size, numeric size, weight roles. |
| Motion | Open duration, hover duration, press duration, progress tick cadence, reduced-motion policy. |
| Density | Compact, normal, comfortable. |
| Contrast | Normal, high contrast, monochrome-safe signal shape fallback. |

### Theme variants

RustOS must ship dark and light variants. Additional variants are data over the same typed model. A variant may alter color, radius, density, and motion, but must not change the meaning of state.

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
| Theme switch repaint | One coherent frame sequence, no mixed half-theme frame |

### Reduced motion

The active `MotionTheme` must include a reduced-motion mode. In reduced motion, state still changes visibly, but through static contrast, rail thickness, shape marks, and text labels rather than animated transitions.

### Event-driven animation

Animation starts from state changes: pointer input, focus change, progress update, theme switch, or typed system state update. Controls must not run idle decorative loops.

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

---

## 11. Component Specifications

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

### 11.16 Panel

Panels are containers with stable layout. A panel may have a Focus Field, header state, grouped actions, and scrollable content. A panel opening from a taskbar or tray item should retain an anchor notch or route line to its invoker while open.

### 11.17 Dialog

Dialogs are decision surfaces.

- The primary action is visually warm only when it is the recommended safe action.
- Destructive actions use a Recovery Latch or deliberate confirmation step.
- Disabled actions explain why through inline copy or a focused explanation.
- Dialogs must not hide capability denial behind generic disabled state.

### 11.18 Notification

Notifications use cards with semantic beads. They should remain compact and actionable.

- Informational: quiet rim.
- Background job: Heat Seam.
- Warning: warning rail.
- Recovery available: recovery bead and clear action.
- Denied action: Authority Mark with source application or service name.

### 11.19 TaskbarItem

Taskbar items combine application identity, activity, and attention state.

- Running: plate visible.
- Focused: lower accent seam.
- Background work: Heat Seam.
- Attention requested: Signal Bead.
- Recovery state: sharper recovery bead.
- Denied optional action: lock or authority mark, not a generic error color.

### 11.20 TraySignal

A tray signal is a compact live status capsule.

- Normal: calm glyph and quiet rim.
- Background work: lower Heat Seam.
- Pressure: side rail in semantic role.
- Recovery: recovery bead.
- Multiple states: stacked mini beads, ordered by severity.

A tray signal expands to an instrument readout on hover or focus. The readout must be short: state name, count or value, and primary safe action.

### 11.21 ScrollBar

Scrollbars should be quiet until useful.

- Idle: low-contrast thumb, no rail noise.
- Hover or scroll: thumb brightens and edge wake may appear near anchored controls.
- Drag: thumb compresses and uses focus-visible state if keyboard-driven.
- Content update: no animation unless the scroll extent changes while the user is looking at it.

### 11.22 Tooltip and HelpTip

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

---

## 16. System Integration

### Active theme flow

`userland/gui/session` owns the active theme selection and relays it to the window manager, taskbar, and GUI applications. Controls listen for theme changes through the existing session or application model, then repaint through the normal surface path.

### Application bundles

Applications may ship resources in their own bundle. Control visuals that are part of the shared RustOS design language belong in the OS-provided shared crates or curated assets, not copied into every application bundle.

### System state models

Controls that render live CPU, memory, disk, network, task, device, or limit information consume typed RustOS data. A control should receive a view model such as `TaskSummary`, `JobProgress`, `PressureSample`, `AuthorityStatus`, or `RecoveryRecommendation`, rather than opening devices or probing system state itself.

### Actions

Controls emit typed userland actions. The receiving service performs the operation, checks authority, validates input, logs security-relevant decisions, and returns typed success or error state. The control updates itself from that returned state.

---

## 17. Switchboard as a Reference Composition

Switchboard should use the same general controls as every other RustOS surface.

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

### Do not

- Hard-code colors, radii, timings, or scale conversions in application controls.
- Duplicate a visual recipe across multiple crates.
- Add a GUI-specific syscall for a control action.
- Make non-GUI code depend on `userland/gui/*`.
- Use random pulsing, idle shimmer, wobble, or layout drift.
- Treat a denied action as a generic disabled state.
- Hide live system information behind an untyped text scrape.
- Add a new public interface solely to make a control easier to draw.

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

The design is allowed to be rich. It is not allowed to be noisy. RustOS controls should feel grounded, typed, secure, and alive at the edges.

