//! Shared **Reactive Alloy** GUI control behaviour for the TAIRiX desktop
//! (`lib/controls` — `plans/GUI-CONTROLS-DESIGN.md`).
//!
//! Reactive Alloy is TAIRiX's GUI control design language. Its controls are
//! typed Rust state resolved against the shared theme and drawn through the
//! shared raster/compositor path; nothing about a control's *behaviour* is
//! duplicated per application. This crate is the shared home for that
//! behaviour, living in `lib/*` because its consumers — the compositing
//! window manager (`userland/gui/wm`), the taskbar (`userland/gui/taskbar`),
//! and the default graphical apps — may not depend on one another
//! (the layering rule), exactly as `lib/geometry` owns the shared coordinate
//! types and `lib/theme` owns the shared design tokens.
//!
//! # The theme chooses the face, not the caller
//!
//! No control accepts a typeface. A control names the job its text does — a
//! `tairix_theme::TextRole` — and the active theme answers with the family,
//! size, and weight, converted to physical pixels through the one shared DPI
//! scale. An application therefore cannot substitute a face of its own, so a
//! menu, button, or dialog reads as the desktop's own furniture wherever it
//! is drawn: inside the file manager, inside a terminal whose screen is
//! monospace, or on the pinboard. A control that took a face from its caller
//! would make the desktop's typography a convention each application could
//! break, and one of them would.
//!
//! An application still draws *its own* text — a document, a terminal grid,
//! its own labels — in whatever face it needs; the rule binds the shared
//! controls, not the application's content.
//!
//! # Scope
//!
//! The first module is the **scroll geometry engine** ([`scroll`]): the one
//! orientation-independent definition of how a viewport's content extent,
//! viewport extent, and offset map to a draggable thumb, and how pointer,
//! wheel, and keyboard input map back to a clamped offset. The design
//! language mandates a single scrollbar behaviour shared by the
//! window-manager root viewport and by nested application content, over one
//! range validation, thumb math, and input model rather than separate
//! vertical, horizontal, window-manager, and application recipes. This
//! module is that single definition.
//!
//! The engine is pure integer arithmetic with no rendering: it computes a
//! one-dimensional thumb *span* along an abstract track and the offset a
//! pointer position implies. The owning viewport maps that span onto a
//! `tairix_geometry::Rect` for its chosen [`ScrollOrientation`] at the
//! edge, so the same math serves both axes.
//!
//! Every input is validated and every result is clamped: an empty,
//! overflowing, or non-scrollable range yields a zero-offset, non-draggable
//! scrollbar rather than out-of-bounds geometry.
//!
//! The [`scrollbar`] module is the drawn scrollbar family — [`ScrollBar`]. It
//! is the one orientation-parameterized scrollbar control (a decrement button,
//! a track, a draggable thumb, and an increment button) drawn over the shared
//! [`scroll`] geometry engine, serving both the window-manager root viewport
//! and nested application content. It holds the owning viewport's
//! [`ScrollModel`] rather than a private offset, brightens the thumb and the
//! relevant end control when awake, preserves the pointer-to-thumb anchor on a
//! drag, and maps end-button line steps, track paging, wheel ticks, and
//! arrow/page/home/end keys back to a clamped offset, emitting a typed
//! [`ScrollAction`]; a denied or disabled bar keeps its position and ignores
//! input (spec §13).
//!
//! The [`state`] module is the typed control-state vocabulary: [`ControlKind`]
//! and [`ControlRole`], the composed [`ControlState`] (focus/pointer/selection/
//! validation/authority/activity/pressure/recovery), the derived
//! [`ControlDisposition`] taxonomy that keeps an authority denial distinct from
//! a plain disabled control, and the window-furniture states. Controls are
//! *composed* from these small typed fields, never one giant enum.
//!
//! The [`button`] module is the first drawn control family — [`Button`],
//! [`IconButton`], and [`SplitButton`]. They resolve every visible property
//! from the active `tairix_theme::Theme` and `tairix_geometry::Scale`, round
//! their plates through the shared `tairix_raster` rounded-rect fill (never a
//! second rounding path), draw their labels/icons through `tairix_font`/
//! `tairix_icon`, and consume the shared `tairix_input` pointer/keyboard
//! vocabulary. A control renders state and emits a typed action; it performs
//! no privileged work — the owning service enforces authority.
//!
//! The [`selector`] module is the boolean-selector family — [`Toggle`],
//! [`Checkbox`], and [`Radio`]. Each is a labelled boolean control that reads
//! by *shape* as well as colour (a toggle's thumb slides to the active side, a
//! checkbox draws a filled square when on and a horizontal bar when mixed, a
//! radio draws a centre bead when selected), so its state is legible without
//! relying on hue. They share the button family's shared plate helpers and
//! interaction model, resolve every visible property from the active
//! theme, and — like every control — emit a typed [`SelectorAction`] rather
//! than performing the change themselves; a denied selector keeps its value
//! and shows an Authority Mark.
//!
//! The [`value`] module is the value-control family — [`Slider`] and
//! [`Progress`]. Both are measured controls whose value is a validated permille
//! fraction. A [`Slider`] is interactive (a rail, a value track that fills to a
//! draggable thumb, drag and keyboard stepping, an optional bounded-cap marker)
//! and emits a typed [`SliderAction`] that the owner commits; a [`Progress`] is
//! a read-only instrument trace of known, working, indeterminate, complete, or
//! failed work, driven only by the state its owner sets — it runs no idle loop
//! and renders an indeterminate trace statically under reduced motion.
//!
//! The [`chart`] module is the resource-history instrument — [`Chart`]. Like
//! [`Progress`], it is a read-only measured control with no pointer or
//! keyboard handling, and unlike [`Progress`] it is always tinted by the
//! resource it represents ([`PressureKind`]) rather than the plain accent. It
//! plots a bounded oldest-to-newest series as a line across the whole box it
//! is given — a trend needs vertical room, and a series confined to a
//! [`MetricTile`]'s proportional-track instrument cannot rise more than a
//! pixel or two whatever its values are. The owner's [`PressureState`] still
//! drives the shared Pressure Rail emphasis exactly as it does for [`Card`] —
//! the resource tint is the instrument's fixed identity, the pressure state
//! is its transient severity.
//!
//! The [`text`] module is the text-entry family — [`TextField`] and
//! [`SearchField`]. Both are single-line entries on a quiet Alloy Plate with a
//! caret, selection, and horizontally-scrolled clipped text; a [`SearchField`]
//! adds a leading magnifier that reads as active when a query is present. A
//! read-only field stays legible and selectable but refuses edits, distinct
//! from a disabled field (muted) and a denied field (Authority Mark); both emit
//! a typed [`TextAction`] the owner validates and commits.
//!
//! The [`menu`] module is the menu command surface — [`Menu`] and
//! [`MenuItem`]. A menu is an elevated command plate carrying a column of row
//! controls (label, optional icon, shortcut, submenu chevron, and a disabled
//! row's reason); it owns keyboard navigation, pointer hover/click, the spec §13
//! authority rendering (a denied row keeps its slot and shows an Authority
//! Mark), and a destructive row's danger rail, emitting a typed [`MenuAction`].
//!
//! The [`toolbar`] module is the toolbar / toolstrip — [`Toolbar`]. It is a
//! horizontal container of [`IconButton`] / [`SplitButton`] tools grouped with
//! quiet gutters, marks the active tool with a persistent lower accent seam,
//! and routes pointer and keyboard input to the tools it owns, emitting a typed
//! [`ToolbarAction`].
//!
//! The [`tabs`] module is the tab strip — [`Tabs`] and [`Tab`]. Tabs select one
//! of several views: the selected tab carries a strong lower seam, a loading
//! tab a Heat Seam, and a modified or error tab a shape-coded Signal Bead, all
//! legible without colour; it emits a typed [`TabsAction`]. A
//! [`TabsOrientation`] lays the same strip out as a column instead, so a
//! sidebar that selects one of several views is this control turned on its side
//! rather than a second selection model: the selected tab then carries a
//! leading seam, and the arrow keys follow the strip's own axis.
//!
//! The [`combo`] module is the choice-entry control — [`ComboBox`]. It composes
//! the text-field focus model and the [`Menu`] model rather than re-deriving
//! either: the popup *is* a [`Menu`] built from the choices, and selecting one
//! emits a typed [`ComboAction`].
//!
//! The [`collection`] module is the collection controls — [`ListRow`],
//! [`TableRow`], [`TableCell`], [`IconTile`], [`Card`], and [`Panel`]. These are
//! the surfaces that group other state and actions: a row is a hoverable,
//! selectable, focusable, activatable control drawn from one shared row chrome
//! (background tint, leading selection/pressure rails, a bottom activity Heat
//! Seam, a trailing Signal Bead, and a focus ring); a table keeps its columns
//! aligned while a row's state changes; an icon tile is one item of an icon view
//! — a picture over its wrapped, centred name, with no plate of its own, so a
//! folder reads as a field of pictures, and only state paints behind one: the
//! pointer's wash, or a selection's half-opaque accent fill over a frosted
//! backdrop; a card carries its dominant state, progress, and a count/alert
//! on three edges with footer action [`Button`]s; and a panel is a stable-layout
//! container with a header, grouped actions, a content region, and an anchor
//! notch back to its invoker. Each interactive one emits a typed action
//! ([`RowAction`], [`CardAction`],
//! [`PanelAction`]); the owner enforces authority. [`TableHeader`] names a
//! table's columns and reports the sort the reader asked for, which the owner
//! commits; it derives its column spans from the same one model a [`TableRow`]
//! does, so a header can never drift out of alignment with its own rows, and it
//! reorders nothing itself.
//!
//! The [`window`] module is the window-manager furniture family —
//! [`WindowFrame`], [`TitleBar`], the compact [`WindowControl`] command
//! buttons (close, minimize, put-to-back, size-toggle), the [`ResizeGrabber`],
//! and the neutral [`ScrollCorner`]. The window manager owns frame rendering,
//! hit testing, pointer capture, move/resize, stacking, minimization, and
//! size-state transitions; the frame's hit map keeps the client viewport and
//! the furniture strictly separate so an application can neither receive
//! furniture input nor impersonate a frame control, and the resize corner
//! never overlaps a scrollbar thumb. Each item emits a typed action
//! ([`WindowControlAction`], [`TitleBarEvent`], [`ResizeEvent`]); the window
//! manager enforces authority and performs the cooperative window operation.
//!
//! The [`shell`] module is the shell-surface family — [`Notification`],
//! [`TaskbarItem`], and [`TraySignal`]. A notification is a [`Card`] carrying
//! semantic beads plus a source attribution; a taskbar item combines
//! application identity with a [`TaskVisibility`] window state, activity,
//! attention, and recovery/authority beads; and a tray signal is a compact
//! status capsule that shows an optional [`TrayBadge`] (a count or alert
//! encoding the dominant live state) on its top-trailing corner, stacks
//! severity-ordered beads starting after it, and expands to an instrument
//! readout on hover or focus. Each emits a typed action; the owner enforces
//! authority.
//!
//! The [`decision`] module is the decision-surface family — [`Dialog`],
//! [`Tooltip`], and [`HelpTip`]. A dialog is a modal choice surface whose
//! recommended action is warm only when its role says so and whose denied
//! actions show the Authority Mark rather than a plain disabled look; a tooltip
//! is a short anchored affordance hint; and a help tip explains why an action
//! is unavailable or recommended, with one optional safe next-step action.
//!
//! The [`nav`] module is the navigation trail — [`Breadcrumb`] and [`Crumb`].
//! It names where the reader is as a path: the trailing crumb is the current
//! location and is deliberately inert, every earlier crumb is an activatable
//! ancestor, and a trail wider than its box elides oldest-first behind a
//! leading ellipsis that itself reaches the newest ancestor it hides — so the
//! current location is never the thing that gets dropped. One layout serves the
//! render, the measurement, and the hit test, so a press can never land on a
//! crumb that was not drawn; it emits a typed [`BreadcrumbAction`].
//!
//! The [`metric`] module is the metric-readout family — [`MetricTile`],
//! [`MetricInstrument`], and [`StatusPill`]. A tile states one resource at a
//! glance: an optional identity icon, a quiet label, a large reading with a
//! quieter unit beside it, an optional detail line, and an optional instrument
//! beneath — either a proportional track over the shared [`MeterValue`], so a
//! resource that cannot be measured shows a bare groove rather than a
//! fabricated zero, or a [`Chart`] trend. Its [`MetricLayout`]
//! chooses between the stacked form, which fills a column of its own, and the
//! inline form, which puts label and reading on one line so several readings
//! can be scanned down a narrow column; an unplated tile draws no plate of its
//! own, so a stack of them shares one container's surface instead of nesting
//! plates. A [`StatusPill`] is the compact capsule that names a state, toned by
//! the theme's own signal roles. Both are read-only instruments: they report,
//! and offer nothing to press.
//!
//! The [`record`] module is the record-list family — [`FactList`] with
//! [`Fact`], and [`Timeline`] with [`TimelineEvent`]. A fact list reports what
//! a thing *is*: the label quiet on the left, the value emphasised on the right
//! and optionally toned, and under a narrow width the label truncates first,
//! because the reading is what the reader came for. A timeline reports what
//! *happened*: a spine spanning only the first mark to the last, shape-coded
//! [`EventMark`]s so the record reads without colour, and a stamp column
//! measured to the widest stamp so every stamp aligns. An empty collection
//! draws nothing at all, since an empty frame would assert a record these
//! controls cannot know exists.
//!
//! The [`rail`] module is the action rail — [`ActionRail`]. It is the vertical
//! counterpart of [`Toolbar`]: the column of full-width [`Button`] commands a
//! surface offers about whatever it is showing. It composes the button family
//! rather than restating plate, press, role, disabled, or Authority Mark
//! rendering, and owns only the stacking geometry, the hover and focus
//! bookkeeping, and the typed [`RailAction`] it reports.
//!
//! The [`damage`] module is the repaint seam every family reports through. An
//! input or update call takes a sink, a control pushes its own bounds when a
//! drawn state field changes, and the host renders and presents only what came
//! back — so a hover crossing one control no longer costs the whole surface.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

pub mod button;
pub mod chart;
pub mod collection;
pub mod combo;
pub mod damage;
pub mod decision;
pub mod menu;
pub mod metric;
pub mod nav;
mod paint;
pub mod rail;
pub mod record;
pub mod scroll;
pub mod scrollbar;
pub mod selector;
pub mod shell;
pub mod state;
pub mod tabs;
#[cfg(any(test, feature = "test-support"))]
pub mod testkit;
pub mod text;
pub mod toolbar;
pub mod value;
pub mod window;

pub use button::{Button, ButtonAction, ButtonContent, IconButton, SplitAction, SplitButton};
pub use chart::{Chart, MAX_CHART_SAMPLES};
pub use collection::{
    Card, CardAction, CellAlign, HeaderAction, HeaderColumn, IconTile, ListRow, Panel, PanelAction,
    PanelEdge, RowAction, SortOrder, TableCell, TableHeader, TableRow,
};
pub use combo::{ComboAction, ComboBox};
pub use decision::{Dialog, DialogAction, HelpTip, HelpTipAction, Tooltip};
pub use menu::{Menu, MenuAction, MenuItem};
pub use metric::{MetricInstrument, MetricLayout, MetricTile, StatusPill};
pub use nav::{Breadcrumb, BreadcrumbAction, Crumb};
pub use rail::{ActionRail, RailAction};
pub use record::{EventMark, Fact, FactList, Timeline, TimelineEvent};
pub use scroll::{
    ScrollGeometry, ScrollModel, ScrollOrientation, ScrollRange, ThumbSpan, TrackHit,
};
pub use scrollbar::{ScrollAction, ScrollBar, ScrollPart};
pub use selector::{Checkbox, Radio, SelectorAction, Toggle};
pub use shell::{
    Notification, NotificationAction, TaskVisibility, TaskbarItem, TaskbarItemAction,
    TaskbarPresentation, TrayBadge, TrayBadgeContent, TrayBadgeTone, TraySignal, TraySignalAction,
};
pub use state::{
    ActivityState, AuthorityState, ControlDisposition, ControlKind, ControlRole, ControlState,
    FocusState, MeterValue, PlateSeating, PointerState, PressureKind, PressureState, ProgressValue,
    RecoveryState, RenderInvariant, SelectionState, SizeAction, ValidationState,
    WindowActivationState, WindowControlKind, WindowFurnitureState, WindowSizeState,
};
pub use tabs::{Tab, Tabs, TabsAction, TabsOrientation};
pub use text::{SearchField, TextAction, TextField};
pub use toolbar::{ToolActivation, Toolbar, ToolbarAction};
pub use value::{Progress, Slider, SliderAction};
pub use window::{
    ControlPlacement, FrameInsets, FrameLayout, FurniturePart, ResizeEdge, ResizeEvent,
    ResizeGrabber, ScrollCorner, TitleBar, TitleBarEvent, TitleBarLayout, TitleHit, WindowControl,
    WindowControlAction, WindowFrame,
};

#[cfg(test)]
mod button_tests;
#[cfg(test)]
mod chart_tests;
#[cfg(test)]
mod collection_tests;
#[cfg(test)]
mod combo_tests;
#[cfg(test)]
mod damage_tests;
#[cfg(test)]
mod decision_tests;
#[cfg(test)]
mod menu_tests;
#[cfg(test)]
mod metric_tests;
#[cfg(test)]
mod nav_tests;
#[cfg(test)]
mod paint_tests;
#[cfg(test)]
mod rail_tests;
#[cfg(test)]
mod record_tests;
#[cfg(test)]
mod scrollbar_tests;
#[cfg(test)]
mod selector_tests;
#[cfg(test)]
mod shell_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod tabs_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod text_tests;
#[cfg(test)]
mod toolbar_tests;
#[cfg(test)]
mod value_tests;
#[cfg(test)]
mod window_tests;
