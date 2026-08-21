//! The production desktop session's fixed configuration — one definition
//! shared by the `Run` binary (`src/run.rs`), the host tests, and the QEMU
//! vertical's host-side runner (`tools/xtask`), which reconstructs the
//! shell's layout from these values to compute its click coordinates, so
//! the driver of the desktop and its observers can never drift
//! (`plans/APPWIN.md` AW3).
//!
//! Only the desktop's four fixed companions are wired by constant: the
//! file manager (autostarted at bring-up as a core desktop component), the
//! Switchboard monitor service (spawned at bring-up to feed the tray
//! capsule, `plans/NEW-TASKBAR.md` T10), the wallpaper chooser (the
//! backdrop menu's *Change Background* row opens it, `plans/PINBOARD.md`
//! §8), and the Date & Time app (the clock menu's set-time row runs it
//! through the console's elevation broker, `plans/NEW-TASKBAR.md` T17),
//! because the session must know their bundles without consulting the
//! program-library catalog — and the Date & Time app is deliberately absent
//! from it, being reached from the clock rather than the launcher. Every
//! other application reaches the desktop through the catalog
//! (`plans/NEW-TASKBAR.md`), which names each entry's bundle on disk —
//! there is no compiled-in application list.

/// Label of the file manager, for launch diagnostics.
pub const FILES_LABEL: &str = "Files";

/// The file-manager bundle's entry-point path in the system application
/// store (an OS-provided app, discovered on disk like every other bundle).
/// The session autostarts it at desktop bring-up, and also launches it to
/// open a folder the user activated on their desktop.
pub const FILES_RUN_PATH: &str = "/System/Applications/files.app/Run";

/// Label of the Switchboard monitor service, for launch diagnostics.
pub const SWITCHBOARD_LABEL: &str = "Switchboard";

/// The Switchboard service bundle's entry-point path in the system service
/// store (`kind = "service"`, so it is planted under `/System/Services` and
/// the shell never resolves it as a command word). The session spawns it at
/// desktop bring-up as the logged-in user; its summaries feed the taskbar's
/// tray capsule and an absent or dead service simply leaves the capsule
/// calm.
pub const SWITCHBOARD_RUN_PATH: &str = "/System/Services/switchboard.app/Run";

/// Label of the wallpaper chooser, for launch diagnostics.
pub const WALLPAPER_LABEL: &str = "Wallpaper";

/// The wallpaper chooser bundle's entry-point path in the system
/// application store (`kind = "application"`, like the file manager). The
/// backdrop menu's *Change Background* row launches it; the chooser then
/// asks the session to adopt what the user picked over the pinboard
/// rendezvous, holding no authority to write the store itself.
pub const WALLPAPER_RUN_PATH: &str = "/System/Applications/wallpaper.app/Run";

/// Label of the Date & Time app, for launch diagnostics.
pub const DATETIME_LABEL: &str = "Date & Time";

/// The Date & Time app bundle's entry-point path in the system application
/// store.
///
/// The session never launches this itself: it names it to the console's
/// elevation broker, which re-authenticates an account holding
/// `CAP_TIME_SET` and starts it as that account. The session holds no such
/// capability and must never hold one.
pub const DATETIME_RUN_PATH: &str = "/System/Applications/datetime.app/Run";
