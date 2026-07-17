//! The production desktop session's fixed configuration — one definition
//! shared by the `Run` binary (`src/run.rs`), the host tests, and the QEMU
//! vertical's host-side runner (`tools/xtask`), which reconstructs the
//! shell's layout from these values to compute its click coordinates, so
//! the driver of the desktop and its observers can never drift
//! (`plans/APPWIN.md` AW3).

use tairix_taskbar::LauncherId;

/// The start menu's light/dark appearance entry label.
pub const APPEARANCE_LABEL: &str = "Toggle Light/Dark";

/// The start menu's file-browser launcher: its menu identity. Selecting
/// the entry spawns [`FILES_RUN_PATH`].
pub const FILES_LAUNCHER: LauncherId = LauncherId(1);

/// Label of the file-browser launcher entry.
pub const FILES_LABEL: &str = "Files";

/// The file-browser bundle's entry-point path in the system app store
/// (an OS-provided app, discovered on disk like every other bundle).
pub const FILES_RUN_PATH: &[u8] = b"/System/Apps/files.app/Run";

/// The start menu's terminal launcher: its menu identity. Selecting the
/// entry spawns [`TERMINAL_RUN_PATH`].
pub const TERMINAL_LAUNCHER: LauncherId = LauncherId(2);

/// Label of the terminal launcher entry.
pub const TERMINAL_LABEL: &str = "Terminal";

/// The terminal bundle's entry-point path in the system app store (an
/// OS-provided app, discovered on disk like every other bundle).
pub const TERMINAL_RUN_PATH: &[u8] = b"/System/Apps/terminal.app/Run";

/// The start menu's file-viewer launcher: its menu identity. Selecting
/// the entry spawns [`VIEWER_RUN_PATH`].
pub const VIEWER_LAUNCHER: LauncherId = LauncherId(3);

/// Label of the file-viewer launcher entry.
pub const VIEWER_LABEL: &str = "Viewer";

/// The file-viewer bundle's entry-point path in the system app store (an
/// OS-provided app, discovered on disk like every other bundle).
pub const VIEWER_RUN_PATH: &[u8] = b"/System/Apps/viewer.app/Run";
