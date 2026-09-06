//! The picker-delegation vertical's shared contract
//! (`plans/CAPABILITY_USE.md` CU6, `plans/APPWIN.md` AW5).
//!
//! The freestanding guest kernel (`src/main.rs`) and the host runner's
//! enrolment (`tools/xtask/src/commands/qemu_tests.rs`) both read these
//! definitions, so the gesture the host injects and the witness the guest
//! latches can never drift apart.
//!
//! # What the vertical proves that no host test can
//!
//! The delegation's *pieces* are host-tested already: the kernel's mint, the
//! instance gate, one-shot redemption, the grantor-identity re-check and the
//! extent ceiling; the picker's browse model; the viewer's view engine. What
//! only a running machine can show is that they are **wired to each other and
//! to two separate principals** — that a user's click in a window the session
//! owns causes the *session* to mint a descriptor for a file the viewer holds
//! no capability to open, and that the *viewer* redeems it and reads the file.
//!
//! That is a capability-bearing hand-off across a process boundary, so it is
//! exactly the claim a host test cannot make: both halves run under their own
//! kernel-attested identity, and it is the kernel that decides the grant is
//! legitimate.
//!
//! # Who states what
//!
//! The two sides observe different streams, and each gates on the one that can
//! answer its question honestly:
//!
//! - The **host** reads the serial transcript, so it gates its pick-click on
//!   the *session's* own announcement that the picker is on screen with rows
//!   in it (the session's own `PICKER_SHOWN`). Only the session knows
//!   that: the picker is a window the session owns, so the window channel says
//!   nothing about it, and the viewer learns only that its request was
//!   *accepted* — which is not the same as "there is a row to click", because
//!   the listing arrives on a worker.
//! - The **guest** kernel's audit sink sees kernel audit records only, so it
//!   gates on those: the two dispatched syscalls below, each attributed to the
//!   process the kernel says made it.
//!
//! Neither side infers the other's facts, and neither counts a cumulative
//! event an unrelated subsystem's cadence could shift under it
//! (`plans/OPEN-DEFECTS.md` D19/D20).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Bare name of the application the library row launches — the bundle is
/// `<system application store>/<name>.app`, composed from the shared
/// `lib/abi` spellings on both sides rather than written out here.
///
/// The viewer, because it is the application built to hold **no** filesystem
/// capability of its own: handed no document it asks the session's trusted
/// picker, so a file reaching it is proof of the delegation rather than of any
/// authority it already had.
pub const PICK_APP_NAME: &str = "viewer";

/// Process name (`comm`) the kernel attests for the desktop session — the
/// principal that mints the delegation.
///
/// The session runs as the logged-in account, so this is what distinguishes
/// the *grantor* from the recipient in the audit trail. Matching it is what
/// makes the first witness a statement about which principal delegated, not
/// merely that some process did.
pub const GRANTOR_COMM: &str = "desktop";

/// Process name (`comm`) the kernel attests for the launched viewer — the
/// principal that redeems the delegation.
///
/// The recipient half of the same claim: the redeeming process is the
/// application the library row launched, not the session re-reading its own
/// descriptor.
pub const RECIPIENT_COMM: &str = PICK_APP_NAME;

/// Name of the syscall the session mints the one-shot delegation with, as the
/// syscall audit field renders it.
pub const GRANT_SYSCALL: &str = "fd_grant";

/// Name of the syscall the viewer redeems the delegation with, as the syscall
/// audit field renders it.
pub const REDEEM_SYSCALL: &str = "fd_redeem";
