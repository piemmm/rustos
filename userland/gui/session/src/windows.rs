//! The session-side composition of served application windows
//! (`plans/APPWIN.md` AW3).
//!
//! [`SessionWindows`] owns the session's window bookkeeping — the map
//! between the window channel's session-minted ids and the compositor's
//! [`WindowId`]s, plus each window's persistent content surface — and
//! [`ShellWindowHost`] is the [`WindowHost`](rustos_window::WindowHost) bridge the
//! `rustos_window::WindowServer` drives: an accepted `Create` opens a
//! desktop window (cascaded, focused, listed on the taskbar), a
//! validated `Present` converts exactly the damaged pixels of the app's
//! shared frame into the window's surface, and a `Close` (or a dead
//! client's teardown) removes the window and its task entry.
//!
//! The engine has already validated everything that reaches this bridge
//! (ownership, frame bounds, damage-in-surface); the bridge still
//! indexes fail-closed and refuses rather than guesses when a record and
//! its frame disagree.

use alloc::collections::BTreeMap;

use rustos_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
use rustos_abi::Errno;
use rustos_wm::{Color, Compositor, Point, Surface, WindowId};

use crate::picker::PickerSlot;
use crate::shell::DesktopShell;

/// The freshly opened window's fill until the app's first present lands:
/// an opaque near-black, so an app that is slow to render shows a blank
/// window body rather than stale or transparent pixels.
const OPEN_FILL: Color = Color::rgb(0x20, 0x20, 0x24);

/// Top-left of the first opened window, in screen pixels. Public so a
/// host-side observer (the AW3 QEMU vertical's screendump assertion)
/// measures the served window where the session actually places it,
/// never a re-derived guess.
pub const CASCADE_ORIGIN: i32 = 48;

/// Cascade step between successively opened windows, in screen pixels.
const CASCADE_STEP: i32 = 32;

/// Number of cascade steps before the placement wraps back to
/// [`CASCADE_ORIGIN`], so late windows never walk off screen.
const CASCADE_WRAP: i32 = 8;

/// One served window's session-side state.
struct WindowRecord {
    /// The compositor window presenting this served window.
    wm: WindowId,
    /// The window's persistent content surface: the master copy the
    /// damaged pixels of each present are converted into, cloned to the
    /// compositor so undamaged content survives partial presents.
    surface: Surface,
}

/// The session's bookkeeping for every live served window.
#[derive(Default)]
pub struct SessionWindows {
    /// Window-channel id → session-side record.
    records: BTreeMap<u64, WindowRecord>,
    /// Compositor id → window-channel id, for routing input back to the
    /// owning app.
    by_wm: BTreeMap<WindowId, u64>,
    /// Monotonic count of opens, driving the cascade placement.
    opened: u64,
}

impl SessionWindows {
    /// An empty window table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The window-channel id of the served window shown as `wm`, if any
    /// (the taskbar and popup windows are not served windows).
    #[must_use]
    pub fn ipc_id(&self, wm: WindowId) -> Option<u64> {
        self.by_wm.get(&wm).copied()
    }

    /// Number of live served windows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// `true` when no served window is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The cascade origin for the next opened window.
    fn next_origin(&self) -> Point {
        cascade_origin_for(self.opened)
    }
}

/// Top-left of the `opened`-th served window (zero-based), in screen
/// pixels: the diagonal cascade from [`CASCADE_ORIGIN`], wrapping so late
/// windows never walk off screen. The one placement rule the session
/// applies and a host-side observer (the AW3/AW4 QEMU vertical's click
/// script and screendump assertions) measures against — never a
/// re-derived guess.
#[must_use]
pub fn cascade_origin_for(opened: u64) -> Point {
    #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
    // Wrapped modulo `CASCADE_WRAP`, so the value is always tiny.
    let step = (opened % CASCADE_WRAP as u64) as i32;
    Point::new(
        CASCADE_ORIGIN + step * CASCADE_STEP,
        CASCADE_ORIGIN + step * CASCADE_STEP,
    )
}

/// The [`WindowHost`] bridge one serve pass borrows: the desktop shell,
/// the compositor, the session's window table, and the trusted picker
/// slot a validated `PickFile` opens.
///
/// [`WindowHost`]: rustos_window::WindowHost
pub struct ShellWindowHost<'a> {
    /// The desktop shell (taskbar, focus, window list).
    pub shell: &'a mut DesktopShell,
    /// The compositor the windows are composed into.
    pub compositor: &'a mut Compositor,
    /// The session's served-window bookkeeping.
    pub windows: &'a mut SessionWindows,
    /// The session's single trusted-picker slot
    /// ([`SessionPicker`](crate::SessionPicker) in production): a
    /// validated `PickFile` opens it, and a closing window takes its own
    /// pick down with it.
    pub picker: &'a mut dyn PickerSlot,
}

impl rustos_window::WindowHost for ShellWindowHost<'_> {
    fn window_opened(
        &mut self,
        window_id: u64,
        surface: &DisplayMode,
        title: &str,
    ) -> Result<(), Errno> {
        // The engine validated the geometry (non-zero, stride covers a
        // row); a surface too large to allocate is refused, never a
        // panic.
        let Some(content) =
            Surface::filled(surface.width_px, surface.height_px, OPEN_FILL.premultiply())
        else {
            return Err(Errno::LengthOutOfRange);
        };
        let origin = self.windows.next_origin();
        let Some(wm) = self
            .shell
            .open_window(self.compositor, origin, content.clone(), title)
        else {
            return Err(Errno::LengthOutOfRange);
        };
        self.windows.opened += 1;
        self.windows.records.insert(
            window_id,
            WindowRecord {
                wm,
                surface: content,
            },
        );
        self.windows.by_wm.insert(wm, window_id);
        Ok(())
    }

    fn window_presented(
        &mut self,
        window_id: u64,
        surface: &DisplayMode,
        frame: &[u8],
        damage: DamageRect,
    ) -> Result<(), Errno> {
        let Some(record) = self.windows.records.get_mut(&window_id) else {
            return Err(Errno::NotFound);
        };
        // Convert exactly the damaged pixels of the presented frame into
        // the master surface. The engine validated the damage against the
        // window's surface and handed a frame slice sized from the mode,
        // but every index below is still checked: a disagreement refuses
        // the present rather than reading out of bounds.
        convert_damage(&mut record.surface, surface, frame, damage)?;
        // Hand the compositor the updated content; it tracks the damage
        // for the next composite. A window the compositor no longer knows
        // fails closed.
        if !self
            .compositor
            .set_surface(record.wm, record.surface.clone())
        {
            return Err(Errno::NotFound);
        }
        Ok(())
    }

    fn window_closed(&mut self, window_id: u64) {
        // A window that dies mid-pick takes its picker down with it: the
        // engine already dropped the pending pick with the record, so no
        // conclusion is (or could be) delivered.
        self.picker
            .abort_for(window_id, self.shell, self.compositor);
        if let Some(record) = self.windows.records.remove(&window_id) {
            self.windows.by_wm.remove(&record.wm);
            let _ = self.shell.close_window(self.compositor, record.wm);
        }
    }

    fn pick_requested(&mut self, window_id: u64) -> Result<(), Errno> {
        // The engine already validated ownership and the per-window
        // single-pending rule; the slot enforces the session's own
        // modality (one picker at a time) and brings the UI up under the
        // session's authority, refusing fail-closed when it cannot.
        self.picker.begin(window_id, self.shell, self.compositor)
    }
}

/// Convert the pixels of `damage` from the presented `frame` (laid out
/// per `mode`) into `surface`, fail-closed on any out-of-bounds index.
fn convert_damage(
    surface: &mut Surface,
    mode: &DisplayMode,
    frame: &[u8],
    damage: DamageRect,
) -> Result<(), Errno> {
    let bpp = mode.format.bytes_per_pixel();
    let x_end = damage
        .x
        .checked_add(damage.width_px)
        .ok_or(Errno::OutOfRange)?;
    let y_end = damage
        .y
        .checked_add(damage.height_px)
        .ok_or(Errno::OutOfRange)?;
    if x_end > surface.width() || y_end > surface.height() {
        return Err(Errno::OutOfRange);
    }
    for y in damage.y..y_end {
        // Checked arithmetic throughout: a hostile stride/geometry
        // combination refuses the present, never indexes past the frame.
        let row = (y as usize)
            .checked_mul(mode.stride_bytes as usize)
            .ok_or(Errno::OutOfRange)?;
        for x in damage.x..x_end {
            let offset = row
                .checked_add(
                    (x as usize)
                        .checked_mul(bpp as usize)
                        .ok_or(Errno::OutOfRange)?,
                )
                .ok_or(Errno::OutOfRange)?;
            let bytes = frame
                .get(offset..offset.checked_add(4).ok_or(Errno::OutOfRange)?)
                .ok_or(Errno::OutOfRange)?;
            // The alpha byte is the app's own and honoured (premultiplied
            // for the compositor's blend), so an app can render
            // translucent regions. A format this bridge does not know how
            // to convert is refused, never guessed (the enum is
            // non-exhaustive by design).
            let color = match mode.format {
                DisplayFormat::Rgba8888 => Color::rgba(bytes[0], bytes[1], bytes[2], bytes[3]),
                DisplayFormat::Bgra8888 => Color::rgba(bytes[2], bytes[1], bytes[0], bytes[3]),
                _ => return Err(Errno::OutOfRange),
            };
            surface.set(x, y, color.premultiply());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_abi::driver::display::{DamageRect, DisplayFormat, DisplayMode};
    use rustos_taskbar::TaskbarConfig;
    use rustos_window::WindowHost;

    fn mode(width: u32, height: u32, format: DisplayFormat) -> DisplayMode {
        DisplayMode {
            width_px: width,
            height_px: height,
            stride_bytes: width * 4,
            format,
        }
    }

    fn desktop() -> (DesktopShell, Compositor) {
        let shell = DesktopShell::new(TaskbarConfig::bottom_bar(640, 480), "Toggle");
        let compositor =
            Compositor::new(mode(640, 480, DisplayFormat::Rgba8888), Color::rgb(0, 0, 0))
                .expect("compositor builds");
        (shell, compositor)
    }

    /// A picker slot recording the bridge's calls: these tests exercise
    /// the window lifecycle, not the picker (which has its own suite in
    /// `crate::tests`), so the slot only observes.
    #[derive(Default)]
    struct RecordingSlot {
        begun: alloc::vec::Vec<u64>,
        aborted: alloc::vec::Vec<u64>,
    }

    impl crate::picker::PickerSlot for RecordingSlot {
        fn begin(
            &mut self,
            for_window: u64,
            _shell: &mut DesktopShell,
            _compositor: &mut Compositor,
        ) -> Result<(), Errno> {
            self.begun.push(for_window);
            Ok(())
        }

        fn abort_for(
            &mut self,
            window_id: u64,
            _shell: &mut DesktopShell,
            _compositor: &mut Compositor,
        ) {
            self.aborted.push(window_id);
        }
    }

    /// An accepted open composes a focused desktop window, records both
    /// id mappings, and cascades successive origins.
    #[test]
    fn open_composes_a_window_and_maps_both_ids() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        {
            let mut host = ShellWindowHost {
                shell: &mut shell,
                compositor: &mut compositor,
                windows: &mut windows,
                picker: &mut picker,
            };
            host.window_opened(7, &mode(64, 48, DisplayFormat::Rgba8888), "files")
                .expect("opens");
            host.window_opened(9, &mode(64, 48, DisplayFormat::Rgba8888), "terminal")
                .expect("opens");
        }
        assert_eq!(windows.len(), 2);
        let wm_of_7 = windows.records.get(&7).expect("recorded").wm;
        assert_eq!(windows.ipc_id(wm_of_7), Some(7));
        let origin_7 = compositor.window(wm_of_7).expect("live").origin();
        let wm_of_9 = windows.records.get(&9).expect("recorded").wm;
        let origin_9 = compositor.window(wm_of_9).expect("live").origin();
        assert_ne!(origin_7, origin_9, "successive opens cascade");
    }

    /// A present converts exactly the damaged pixels — in both channel
    /// orders — into the composed window's surface, leaving undamaged
    /// content intact.
    #[test]
    fn present_converts_damaged_pixels_in_both_formats() {
        for (format, bytes, want) in [
            (
                DisplayFormat::Rgba8888,
                [0x11u8, 0x22, 0x33, 0xFF],
                Color::rgba(0x11, 0x22, 0x33, 0xFF),
            ),
            (
                DisplayFormat::Bgra8888,
                [0x33u8, 0x22, 0x11, 0xFF],
                Color::rgba(0x11, 0x22, 0x33, 0xFF),
            ),
        ] {
            let (mut shell, mut compositor) = desktop();
            let mut windows = SessionWindows::new();
            let mut picker = RecordingSlot::default();
            let m = mode(4, 4, format);
            {
                let mut host = ShellWindowHost {
                    shell: &mut shell,
                    compositor: &mut compositor,
                    windows: &mut windows,
                    picker: &mut picker,
                };
                host.window_opened(1, &m, "w").expect("opens");
                // One frame with the probe pixel at (2, 1).
                let mut frame = [0u8; 4 * 4 * 4];
                let offset = (4 + 2) * 4;
                frame[offset..offset + 4].copy_from_slice(&bytes);
                host.window_presented(
                    1,
                    &m,
                    &frame,
                    DamageRect {
                        x: 2,
                        y: 1,
                        width_px: 1,
                        height_px: 1,
                    },
                )
                .expect("presents");
            }
            let record = windows.records.get(&1).expect("live");
            assert_eq!(record.surface.get(2, 1), Some(want.premultiply()));
            // Undamaged pixels keep the open fill.
            assert_eq!(record.surface.get(0, 0), Some(OPEN_FILL.premultiply()));
        }
    }

    /// A present whose damage or frame disagrees with the recorded
    /// surface refuses fail-closed instead of indexing out of bounds,
    /// and an unknown window is `NotFound`.
    #[test]
    fn present_refuses_bad_damage_short_frames_and_unknown_windows() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
        };
        let m = mode(4, 4, DisplayFormat::Rgba8888);
        host.window_opened(1, &m, "w").expect("opens");
        let frame = [0u8; 4 * 4 * 4];
        let full = DamageRect {
            x: 0,
            y: 0,
            width_px: 4,
            height_px: 4,
        };
        // Damage outside the surface.
        assert_eq!(
            host.window_presented(
                1,
                &m,
                &frame,
                DamageRect {
                    x: 3,
                    y: 3,
                    width_px: 2,
                    height_px: 2
                }
            ),
            Err(Errno::OutOfRange)
        );
        // A frame shorter than the damage needs.
        assert_eq!(
            host.window_presented(1, &m, &frame[..8], full),
            Err(Errno::OutOfRange)
        );
        // An unknown window.
        assert_eq!(
            host.window_presented(99, &m, &frame, full),
            Err(Errno::NotFound)
        );
    }

    /// A close removes the window from the compositor and both maps; a
    /// second close of the same id is a no-op.
    #[test]
    fn close_removes_the_window_and_its_mappings() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
        };
        let m = mode(8, 8, DisplayFormat::Rgba8888);
        host.window_opened(1, &m, "w").expect("opens");
        let wm = host.windows.records.get(&1).expect("live").wm;
        host.window_closed(1);
        assert!(host.windows.is_empty());
        assert_eq!(host.windows.ipc_id(wm), None);
        assert!(host.compositor.window(wm).is_none());
        host.window_closed(1);
        assert!(host.windows.is_empty());
    }

    /// The bridge forwards a validated pick request to the slot and
    /// aborts the window's pick when the window closes.
    #[test]
    fn pick_requests_and_closures_reach_the_picker_slot() {
        let (mut shell, mut compositor) = desktop();
        let mut windows = SessionWindows::new();
        let mut picker = RecordingSlot::default();
        let mut host = ShellWindowHost {
            shell: &mut shell,
            compositor: &mut compositor,
            windows: &mut windows,
            picker: &mut picker,
        };
        let m = mode(8, 8, DisplayFormat::Rgba8888);
        host.window_opened(1, &m, "w").expect("opens");
        host.pick_requested(1).expect("slot accepts");
        host.window_closed(1);
        assert_eq!(picker.begun, alloc::vec![1]);
        assert_eq!(picker.aborted, alloc::vec![1]);
    }
}
