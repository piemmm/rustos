//! The window thumbnails the icon bar's hover window picker shows.
//!
//! A picker cell states one window by a scaled copy of that window's last
//! presented frame, and scaling a frame is real work: every source pixel is
//! read once by the shared area filter. An application with a screenful of
//! windows therefore cannot have its whole picker built in one go — the
//! desktop's serve loop would stop serving for as long as it took, which is
//! exactly the freeze this exists to prevent.
//!
//! So the work is *sliced*: while the pointer rests out the picker's opening
//! dwell, the session scales one of that application's windows per turn of
//! its loop and keeps the result here. The picker then opens already drawn,
//! and the loop was free to serve input, presents and IPC between the slices.
//! A slice still owed shortens the loop's park to nothing, so the work
//! finishes as fast as the machine allows without ever being polled for.
//!
//! Nothing is retained past the hover: the pointer leaving drops every
//! prepared pixel, so a picker's thumbnails cost memory only while the user
//! is looking at (or about to look at) them. A re-hover pays the slices
//! again, which the dwell hides.

use alloc::vec::Vec;

use tairix_raster::Surface;
use tairix_taskbar::TaskId;

/// The thumbnails prepared for one application's hover picker.
#[derive(Debug, Default)]
pub struct WindowThumbnails {
    /// Strip index the preparation is aimed at, or `None` when idle.
    app: Option<usize>,
    /// Windows still to scale, in cell order, most recent last.
    remaining: Vec<TaskId>,
    /// Windows already scaled, waiting for the picker to open.
    ready: Vec<(TaskId, Surface)>,
}

impl WindowThumbnails {
    /// An idle preparation: no application aimed at, nothing owed, nothing
    /// held.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            app: None,
            remaining: Vec::new(),
            ready: Vec::new(),
        }
    }

    /// Aim the preparation at the application at strip index `app`, whose
    /// windows are `windows`.
    ///
    /// A target that has not moved keeps its progress; a new one starts over,
    /// dropping the previous application's pixels — the picker that would
    /// have shown them is not going to open.
    pub fn aim(&mut self, app: usize, windows: &[TaskId]) {
        if self.app == Some(app) {
            return;
        }
        self.app = Some(app);
        self.remaining.clear();
        self.remaining.extend(windows.iter().copied().rev());
        self.ready.clear();
    }

    /// Drop the preparation: the pointer has left, so no picker is coming.
    pub fn forget(&mut self) {
        self.app = None;
        self.remaining.clear();
        self.ready.clear();
    }

    /// Whether a thumbnail is still owed, so the caller's next park is due
    /// immediately.
    #[must_use]
    pub fn owed(&self) -> bool {
        !self.remaining.is_empty()
    }

    /// Whether the preparation is already aimed at the application at strip
    /// index `app`, and so has queued its windows.
    ///
    /// A target it is *not* aimed at yet is work owed just as a queued window
    /// is: the queue for it has not even been built.
    #[must_use]
    pub fn aimed_at(&self, app: usize) -> bool {
        self.app == Some(app)
    }

    /// Take the next window owed a thumbnail, dropping it from the queue.
    ///
    /// Taken rather than peeked so a window whose pixels cannot be scaled —
    /// one that has not presented yet, or whose content the memory-pressure
    /// ladder released — is asked for once and then leaves the queue: its
    /// cell draws the application's glyph instead, and the slices go on
    /// finishing.
    pub fn next_owed(&mut self) -> Option<TaskId> {
        self.remaining.pop()
    }

    /// Hold `thumbnail` for `window` until the picker opens.
    pub fn store(&mut self, window: TaskId, thumbnail: Surface) {
        self.ready.push((window, thumbnail));
    }

    /// Take the prepared thumbnail for `window`, if one was scaled.
    ///
    /// Moved out rather than copied: the picker's cell owns the pixels from
    /// here on, so a picker that has opened holds exactly one copy of each
    /// thumbnail.
    pub fn take(&mut self, window: TaskId) -> Option<Surface> {
        let at = self.ready.iter().position(|(held, _)| *held == window)?;
        Some(self.ready.swap_remove(at).1)
    }
}
