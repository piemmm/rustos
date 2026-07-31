//! The production desktop session's fixed configuration — one definition
//! shared by the `Run` binary (`src/run.rs`), the host tests, and the QEMU
//! vertical's host-side runner (`tools/xtask`), which reconstructs the
//! shell's layout from these values to compute its click coordinates, so
//! the driver of the desktop and its observers can never drift
//! (`plans/APPWIN.md` AW3).
//!
//! Only the file manager is wired by constant: the taskbar's permanent
//! Files button opens it, so the session must know its bundle without
//! consulting the program-library catalog. Every other application reaches
//! the desktop through the catalog (`plans/NEW-TASKBAR.md`), which names
//! each entry's bundle on disk — there is no compiled-in application list.

/// Label of the file manager, for launch diagnostics.
pub const FILES_LABEL: &str = "Files";

/// The file-manager bundle's entry-point path in the system app store
/// (an OS-provided app, discovered on disk like every other bundle). The
/// taskbar's permanent Files button launches it — or raises its window
/// when one is already open.
pub const FILES_RUN_PATH: &str = "/System/Apps/files.app/Run";
