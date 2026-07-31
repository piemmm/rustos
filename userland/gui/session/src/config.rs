//! The production desktop session's fixed configuration — one definition
//! shared by the `Run` binary (`src/run.rs`), the host tests, and the QEMU
//! vertical's host-side runner (`tools/xtask`), which reconstructs the
//! shell's layout from these values to compute its click coordinates, so
//! the driver of the desktop and its observers can never drift
//! (`plans/APPWIN.md` AW3).
//!
//! Only the desktop's two fixed companions are wired by constant: the file
//! manager (the taskbar's permanent Files button opens it) and the
//! Switchboard monitor service (spawned at bring-up to feed the tray
//! capsule, `plans/NEW-TASKBAR.md` T10), because the session must know
//! their bundles without consulting the program-library catalog. Every
//! other application reaches the desktop through the catalog
//! (`plans/NEW-TASKBAR.md`), which names each entry's bundle on disk —
//! there is no compiled-in application list.

/// Label of the file manager, for launch diagnostics.
pub const FILES_LABEL: &str = "Files";

/// The file-manager bundle's entry-point path in the system app store
/// (an OS-provided app, discovered on disk like every other bundle). The
/// taskbar's permanent Files button launches it — or raises its window
/// when one is already open.
pub const FILES_RUN_PATH: &str = "/System/Apps/files.app/Run";

/// Label of the Switchboard monitor service, for launch diagnostics.
pub const SWITCHBOARD_LABEL: &str = "Switchboard";

/// The Switchboard service bundle's entry-point path in the system service
/// store (`kind = "service"`, so it is planted under `/System/Services` and
/// the shell never resolves it as a command word). The session spawns it at
/// desktop bring-up as the logged-in user; its summaries feed the taskbar's
/// tray capsule and an absent or dead service simply leaves the capsule
/// calm.
pub const SWITCHBOARD_RUN_PATH: &str = "/System/Services/switchboard.app/Run";
