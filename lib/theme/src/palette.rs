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
    /// The Frame Rim of the active (focused) window: a neutral tone, never
    /// the accent, so the accent stays reserved for a chosen action while
    /// focus reads as the brighter of two greys.
    pub frame_active: Rgba,
    /// The Frame Rim of an inactive window — the quieter of the two neutral
    /// tones, still legible against the desktop behind it.
    pub frame_inactive: Rgba,
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
