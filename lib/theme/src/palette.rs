//! The colour roles a theme defines.
//!
//! A [`Palette`] is a fixed set of *semantic* colour roles, not a free-form
//! map: every role is a named field, so a theme cannot omit one and a
//! consumer cannot ask for a role that does not exist (illegal states are
//! unrepresentable). The window manager, the taskbar,
//! and the default apps all read these same roles, which is what makes a
//! theme switch apply consistently everywhere from one definition.

use crate::color::Rgba;

/// The semantic colours every theme provides.
///
/// Roles are intentionally generic ("surface", "on-surface") rather than
/// per-widget ("button", "menu") so that adding a widget needs no new role
/// and adding a theme needs no new code — a theme is data.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Palette {
    /// The empty desktop behind every window (the compositor root).
    pub desktop: Rgba,
    /// The base fill of a window or app content area.
    pub surface: Rgba,
    /// A raised/alternate fill (the taskbar, menus, headers) that must
    /// read as distinct from [`surface`](Self::surface).
    pub surface_raised: Rgba,
    /// How opaque a *floating desktop-chrome* surface is — the taskbar and
    /// the popups it puts on screen, laid over a backdrop blurred by
    /// [`Metrics::chrome_backdrop_blur`](crate::Metrics::chrome_backdrop_blur).
    ///
    /// The surface keeps whichever colour role it would wear solid and takes
    /// this alpha instead, so a frosted bar is recognisably the same grey as a
    /// solid one and the wallpaper behind it reads through as a wash. Anything
    /// that reads as *part* of that surface — a list row, a menu row — takes
    /// it too, which is what keeps a resting row exactly its ground.
    ///
    /// `255` draws chrome solid.
    pub chrome_alpha: u8,
    /// How opaque a control *plate* raised on floating chrome is — a button,
    /// a text field, a card.
    ///
    /// A step more solid than [`chrome_alpha`](Self::chrome_alpha), so a
    /// control reads as furniture standing on the glass rather than a hole cut
    /// in it, while the backdrop still shows through.
    pub chrome_plate_alpha: u8,
    /// Primary foreground (text, icons) drawn on
    /// [`surface`](Self::surface) and
    /// [`surface_raised`](Self::surface_raised).
    pub on_surface: Rgba,
    /// Secondary foreground for less prominent text on a surface.
    pub on_surface_muted: Rgba,
    /// The accent used for selection, focus, and the active task.
    pub accent: Rgba,
    /// Foreground drawn on top of [`accent`](Self::accent).
    pub on_accent: Rgba,
    /// The plate a selected item is filled with, laid crisply over a backdrop
    /// blurred by
    /// [`Metrics::selection_backdrop_blur`](crate::Metrics::selection_backdrop_blur).
    ///
    /// Translucent, so what lies behind a selected item — a window's
    /// surface, the desktop wallpaper — still reads through its selection
    /// rather than being replaced by a block of accent. It is authored per
    /// theme rather than derived from [`accent`](Self::accent) at the draw
    /// site so a theme can tune the fill's weight against its own surfaces.
    pub selection_fill: Rgba,
    /// Window and control borders / separators.
    pub border: Rgba,

    // --- Reactive Alloy control roles -----------------------------------
    /// The inner plate of a control while the pointer rests on it.
    ///
    /// A hovered control lightens its plate rather than only its edge, which
    /// is the only feedback available to a control that wears no perimeter of
    /// its own — an icon seated in the taskbar. It is therefore one clear step
    /// away from [`surface_raised`](Self::surface_raised), in whichever
    /// direction the appearance requires: brighter on a dark theme, deeper on
    /// a light one.
    pub surface_hover: Rgba,
    /// The inner plate of a control while it is pressed (darker than
    /// [`surface`](Self::surface)).
    pub surface_pressed: Rgba,
    /// The quiet perimeter (Signal Rim) of a resting control.
    pub rim: Rgba,
    /// The reactive perimeter of a hovered, focused, or active control.
    pub rim_active: Rgba,
    /// The danger role for destructive actions and refusals.
    pub danger: Rgba,

    // --- Semantic signal roles (spec §6) --------------------------------
    /// Compute saturation / CPU pressure.
    pub cpu_pressure: Rgba,
    /// Memory pressure.
    pub memory_pressure: Rgba,
    /// Storage throughput / disk pressure.
    pub disk_pressure: Rgba,
    /// Network transfer / remote I/O activity.
    pub network_activity: Rgba,
    /// Power / battery pressure.
    pub power_pressure: Rgba,
    /// Thermal pressure.
    pub thermal_pressure: Rgba,
    /// Hung, not-responding, repair, restart, or force-action state.
    pub recovery: Rgba,
    /// Completed, verified, recovered.
    pub success: Rgba,
    /// Elevated impact, caution, delayed risk.
    pub warning: Rgba,
    /// Missing authority or blocked action.
    pub denied: Rgba,

    // --- Scroll and window-frame roles ----------------------------------
    /// The quiet Scroll Channel (track) behind a scrollbar thumb.
    pub scroll_track: Rgba,
    /// The scrollbar thumb.
    pub scroll_thumb: Rgba,
    /// The Frame Rim every window wears: one quiet neutral, a step lighter
    /// than [`surface`](Self::surface) on a dark appearance and a step deeper
    /// on a light one, so a window's edge separates it from the desktop
    /// without drawing the eye.
    ///
    /// The rim is deliberately *not* a focus signal. A frame that brightened
    /// when focused made the boundary — the one part of the chrome the eye
    /// tracks a window's shape by — the loudest thing on the desktop, and
    /// left every unfocused window looking switched off. Focus is carried by
    /// the title bar instead: its text sits at
    /// [`on_surface`](Self::on_surface) while active and
    /// [`on_surface_muted`](Self::on_surface_muted) while not, and under
    /// heavy contrast the active frame gains a second, inner rim line so the
    /// distinction is a difference in shape and not only in tone.
    pub frame: Rgba,

    // --- Window-command highlight roles ---------------------------------
    //
    // A hue per title-bar command, so a lit one says *which* command it is
    // before its glyph is read. Kept separate from `danger` / `warning` /
    // `success`: retuning a signal hue for legibility must not repaint a
    // window button, and a command's hue is its identity, not a severity.
    /// The wash the close command highlights with, red.
    ///
    /// Authored translucent, like [`selection_fill`](Self::selection_fill), so
    /// the highlight tints the title bar rather than covering it.
    pub window_close: Rgba,
    /// The wash the minimize command highlights with, yellow.
    pub window_minimize: Rgba,
    /// The wash the size toggle highlights with, green — maximizing and
    /// restoring alike, because they are one command.
    pub window_maximize: Rgba,
    /// The wash the put-to-back command highlights with, blue.
    pub window_put_to_back: Rgba,

    /// How opaque the hue a title bar takes from its window's identity icon is
    /// where it is strongest, at the icon itself.
    ///
    /// An opacity rather than a colour, because the colour is the icon's: the
    /// bar carries its application's own hue, so the theme sets only how far
    /// through it reads. Low, and deliberately so — the wash has to stay a tint
    /// on the chrome rather than a second, blurrier copy of the icon beside the
    /// real one. `0` turns the wash off for a theme that wants plain chrome.
    pub title_hue_alpha: u8,
}

impl Palette {
    /// The semantic signal colour for a resource pressure.
    ///
    /// One place maps a pressure to its role so no consumer restates the
    /// mapping. The kind is taken as a small local enum
    /// ([`SignalRole`]) rather than a `lib/controls` type, keeping this crate
    /// at the bottom of the layering.
    #[must_use]
    pub const fn signal(&self, role: SignalRole) -> Rgba {
        match role {
            SignalRole::Cpu => self.cpu_pressure,
            SignalRole::Memory => self.memory_pressure,
            SignalRole::Disk => self.disk_pressure,
            SignalRole::Network => self.network_activity,
            SignalRole::Power => self.power_pressure,
            SignalRole::Thermal => self.thermal_pressure,
            SignalRole::Recovery => self.recovery,
            SignalRole::Success => self.success,
            SignalRole::Warning => self.warning,
            SignalRole::Denied => self.denied,
        }
    }
}

/// A semantic signal a control can display, used to resolve one palette
/// colour and (in the renderer) one shape fallback.
///
/// This is the theme-side vocabulary of the spec §6 semantic roles. The
/// resource-pressure subset lines up one-to-one with `lib/controls`'
/// `PressureKind`; a renderer maps its typed state to a `SignalRole` and asks
/// the palette for the colour, so the mapping lives in exactly one place.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum SignalRole {
    /// Compute saturation.
    Cpu,
    /// Memory pressure.
    Memory,
    /// Storage throughput.
    Disk,
    /// Network transfer.
    Network,
    /// Power / battery pressure.
    Power,
    /// Thermal pressure.
    Thermal,
    /// Recovery / repair / restart.
    Recovery,
    /// Completed / verified.
    Success,
    /// Caution.
    Warning,
    /// Missing authority.
    Denied,
}
