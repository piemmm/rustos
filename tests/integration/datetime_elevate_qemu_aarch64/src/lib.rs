//! The elevated Date & Time vertical's shared interaction contract.
//!
//! The freestanding guest kernel (`src/main.rs`) and the host runner's
//! enrolment (`tools/xtask/src/commands/qemu_tests.rs`) both read these
//! definitions, so the gesture the host injects and the witness the guest
//! latches can never drift apart.
//!
//! Every stage is sequenced on a **uniquely attributable** witness — the
//! exact bundle a load names, a reply only one request shape produces —
//! never on a cumulative event count that an unrelated subsystem's cadence
//! can shift under it.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Bare name of the application the elevated launch starts — the bundle is
/// `<system application store>/<name>.app`, composed from the shared
/// `lib/abi` spellings on both sides rather than written out here.
pub const DATETIME_APP_NAME: &str = "datetime";

/// Number of windows the elevated application opens once the vertical's
/// whole script has been delivered: the one it opens on launch.
///
/// Each is a create reply on the reserved window endpoint. In this world the
/// elevated Date & Time application is the only client that creates a window
/// after the script starts (the desktop's own surfaces are session-painted
/// compositor windows that never call the window channel; the autostarted
/// file manager is already up before the script runs and is not counted by
/// the guest's latch). Reaching this count is the guest's PASS.
pub const WINDOWS_OPENED: u32 = 1;
