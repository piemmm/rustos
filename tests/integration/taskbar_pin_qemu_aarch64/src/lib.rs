//! The taskbar pin + Switchboard vertical's shared interaction contract
//! (`plans/NEW-TASKBAR.md` T15).
//!
//! The freestanding guest kernel (`src/main.rs`) and the host runner's
//! enrolment (`tools/xtask/src/commands/qemu_tests.rs`) both read these
//! definitions, so the gesture the host injects and the witness the guest
//! latches can never drift apart.
//!
//! Every stage is sequenced on a **uniquely attributable** witness — the
//! exact bundle a load names, a directory only the pin writer creates, a
//! marker the guest emits once — never on a cumulative event count that an
//! unrelated subsystem's cadence can shift under it
//! (`plans/OPEN-DEFECTS.md` D19/D20).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Bare name of the application the vertical pins and then launches from
/// its new pin — the bundle is `<system application store>/<name>.app`, composed
/// from the shared `lib/abi` spellings on both sides rather than written
/// out here.
///
/// It is deliberately an application **no other stage of any vertical
/// launches**, so the `APP_LOADED` record naming it can only have come
/// from the pin the script created: the file manager and the terminal are
/// already launched by the autoload vertical, and this one lists itself in
/// the program library, requests no ambient authority, reads no
/// filesystem, and opens exactly one window with no prompt of its own.
pub const PIN_APP_NAME: &str = "widgets";

/// Guest-emitted marker announcing that the Switchboard panel has been
/// created **and painted**: the guest saw the panel's own create reply,
/// then the reply completing the present that first drew into the frame
/// that create mapped.
///
/// It is anchored on the create reply, identified by its distinctive wire
/// length, and never on a reply's position in the endpoint's traffic. The
/// reserved window rendezvous serves every client, so an ordinal is not the
/// panel's to own: when the Switchboard gained a start-up desktop query,
/// that extra reply shifted the old two-reply count onto the *create*, and
/// the screendump was captured a full round trip before the panel had been
/// painted — an empty cascade slot on a passing guest. An anchored gate
/// cannot be moved by a call some other client, or an earlier phase of this
/// one, adds.
///
/// It is the pin click's gate, and it is a **causal** one rather than a
/// timed one: serving a window-endpoint call is a different wake of the
/// session's serve loop from the input wake that pinned, and the session
/// re-resolves its pin strip before it parks at the end of every wake. So
/// this marker cannot appear until the pin the earlier click persisted is
/// live in the bar the guest hit-tests, whatever the host's load.
pub const SWITCHBOARD_PANEL_MARKER: &str = "TASKBAR-PIN switchboard panel presented";
