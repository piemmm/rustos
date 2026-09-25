//! The three-principal document hand-over vertical's shared contract
//! (`plans/VIEW.md`, `plans/APPWIN.md` AW5).
//!
//! The freestanding guest kernel (`src/main.rs`) and the host runner's
//! enrolment (`tools/xtask/src/commands/qemu_tests.rs`) both read these
//! definitions, so the gesture the host injects and the witness the guest
//! latches can never drift apart.
//!
//! # What the vertical proves that no host test can
//!
//! Every layer of the hand-over is host-tested already: the `HandOverLaunch`
//! and open-target wire shapes and their refusals, the window engine's
//! per-client queue and its wake, the session's launch resolution and the
//! relay's fail-closed paths, the kernel's non-widening pass-through of a
//! *held* delegation, and the viewer's drain. What no host test can state is
//! that **three** principals and the kernel wire up to each other — that a
//! double-click in the file manager's own window makes the *manager* mint a
//! delegation for a file the viewer holds no authority to open, the *session*
//! redeem that delegation and hand the same authority on without lending any
//! of its own larger reach, and the *viewer* redeem what arrives.
//!
//! Each of the three runs under its own kernel-attested identity, and it is
//! the kernel that decides each grant is legitimate, so the claim is exactly
//! the one a host test cannot make.
//!
//! # Who states what
//!
//! The two sides observe different streams, and each gates on the one that can
//! answer its question honestly:
//!
//! - The **host** reads the serial transcript, so it gates each gesture on the
//!   session's own announcement that the surface it aims at is on screen — a
//!   drawn icon-bar slot for a resident application, a presented window for a
//!   click into one. Only the session knows either.
//! - The **guest** kernel's audit sink sees kernel audit records only, so it
//!   gates on those: the four dispatched syscalls of a relay, each attributed
//!   to the process the kernel says made it. It reports each step it latches
//!   ([`RELAY_STEP_MARKERS`]), because a gate that can only pass or time out
//!   cannot say which hop was missing.
//!
//! Neither side can name the *kind* of a pointer event: the kernel's delivery
//! record carries a destination port and a length, and no record anywhere
//! names a pointer action. So the script activates through the item's own
//! context menu, whose drawn plate is the session's own witness that the press
//! reached an entry, rather than resting the run on two presses pairing inside
//! the guest's interval — a fact nothing here could observe either way.
//!
//! Neither side infers the other's facts, and neither counts a cumulative
//! event an unrelated subsystem's cadence could shift under it
//! (`plans/OPEN-DEFECTS.md` D19/D20).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Bare name of the viewer bundle the file manager launches — the bundle is
/// `<system application store>/<name>.app`, composed from the shared
/// `lib/abi` spellings on both sides rather than written out here.
///
/// The picture and document viewer, because it is the application built to
/// hold **no** filesystem capability of its own, and because it declares a
/// single instance: a document reaching it is proof of the delegation rather
/// than of any authority it already had, and a *second* document reaching the
/// same process is proof the desktop's single-instance funnel was used rather
/// than a fresh viewer started.
///
/// Nothing launches it in advance. The **file manager** starts it, on the
/// first activation, because the desktop's funnel answers "not running" — so
/// the instance every later document must reach is one the desktop itself
/// never spawned, and the only thing that can name it is the identity the
/// kernel attested for it. Pre-launching it from the program library, as this
/// script once did, put it in the desktop's own launch table and hid exactly
/// that.
pub const VIEWER_APP_NAME: &str = tairix_test_filepick_qemu_aarch64::PICK_APP_NAME;

/// Process name (`comm`) the kernel attests for the file manager — the
/// principal that **mints** the delegation, from a descriptor it opened under
/// the logged-in user's own identity.
///
/// Its bundle stem, named once by the icon-bar contract the autoload vertical
/// owns; the kernel attests exactly that stem for a bundle's `Run`.
pub const MANAGER_COMM: &str = tairix_test_autoload_input_qemu_aarch64::FILES_BAR_APP_NAME;

/// Process name (`comm`) the kernel attests for the desktop session — the
/// middle principal, which redeems what the manager minted and grants the
/// same authority on to the instance that will show it.
pub const SESSION_COMM: &str = tairix_test_filepick_qemu_aarch64::SESSION_COMM;

/// Process name (`comm`) the kernel attests for the viewer — the principal
/// that **redeems** what the session relayed.
pub const VIEWER_COMM: &str = VIEWER_APP_NAME;

/// Name of the syscall a delegation is minted with, as the syscall audit field
/// renders it.
pub const GRANT_SYSCALL: &str = tairix_test_filepick_qemu_aarch64::GRANT_SYSCALL;

/// Name of the syscall a delegation is redeemed with, as the syscall audit
/// field renders it.
pub const REDEEM_SYSCALL: &str = tairix_test_filepick_qemu_aarch64::REDEEM_SYSCALL;

/// Name of the syscall the session redeems the manager's delegation with —
/// bound to the manager it relays for, so a relay can never consume a
/// delegation some other process minted — as the syscall audit field renders
/// it.
pub const RELAY_REDEEM_SYSCALL: &str = tairix_test_filepick_qemu_aarch64::BOUND_REDEEM_SYSCALL;

/// Complete relays the PASS gate wants.
///
/// Two, because one relay proves the chain and the second proves *whose*: the
/// viewer redeems both from the same kernel-attested task, so the later
/// document went to the instance that was already running rather than to a
/// viewer started for it. That is the desktop's single-instance funnel doing
/// its job — one process, a window per document.
pub const RELAY_ROUNDS: u32 = 2;

/// Documents the pointer script activates in the manager's window.
///
/// One more than [`RELAY_ROUNDS`], because the **first** activation is the one
/// that starts the viewer: the desktop's funnel answers "not running", so the
/// manager spawns it and hands the document over on `STDIN` rather than through
/// the relay. Only from the second activation on is there a live instance for
/// the authority to travel to — and that instance was started by the *manager*,
/// so a desktop resolving a slot's application from its own launch bookkeeping
/// could not find it and would spawn a second viewer per document.
pub const ACTIVATIONS: u32 = RELAY_ROUNDS + 1;

/// Syscalls one relay is recognised by, in the order the chain makes them:
/// the manager mints, the session redeems, the session mints again, the viewer
/// redeems. Each pair is `(comm, sc)` as the kernel-attested audit fields
/// render them.
///
/// The order is the claim. Read as a set these four records would be
/// consistent with three processes touching descriptors of their own; read as
/// a sequence they can only be one authority travelling from the principal
/// that opened the file to the principal with no authority to open it.
pub const RELAY_CHAIN: [(&str, &str); 4] = [
    (MANAGER_COMM, GRANT_SYSCALL),
    (SESSION_COMM, RELAY_REDEEM_SYSCALL),
    (SESSION_COMM, GRANT_SYSCALL),
    (VIEWER_COMM, REDEEM_SYSCALL),
];

/// Index in [`RELAY_CHAIN`] of the viewer's redeem — the step whose attested
/// task identifies the instance the document landed in.
pub const VIEWER_REDEEM_STEP: usize = RELAY_CHAIN.len() - 1;

/// What the guest prints as each step of [`RELAY_CHAIN`] latches, in the same
/// order — so a run that does *not* pass says how far the authority travelled
/// instead of merely falling silent.
///
/// The gate is four records that must arrive in order, and the failure the
/// vertical is most exposed to is an early step never arriving at all. Read
/// against the transcript these localise that immediately: no first marker
/// means the manager never minted, so the gesture never reached an item; a
/// first without a second means the desktop never redeemed what it was handed.
/// A passing run prints the whole set once per relay, so [`RELAY_ROUNDS`]
/// times.
///
/// Sized from [`RELAY_CHAIN`], so a step added to the chain without a witness
/// to report it does not compile.
pub const RELAY_STEP_MARKERS: [&str; RELAY_CHAIN.len()] = [
    "HANDOVER manager minted a delegation",
    "HANDOVER desktop redeemed the manager's delegation",
    "HANDOVER desktop granted the document on",
    "HANDOVER viewer redeemed the document",
];

/// What the guest prints when a redeem reaches the viewer from a task other
/// than the instance this run has been watching.
///
/// Its own witness rather than silence, because it is the one failure that
/// looks like success from outside: the authority did travel all four hops,
/// but it landed in a *second* viewer process, so the desktop's
/// single-instance funnel did not reach the instance already running. The
/// relay is refused and a fresh one watched for, exactly as the PASS gate
/// requires.
pub const FOREIGN_VIEWER_MARKER: &str = "HANDOVER a foreign viewer redeemed; relay refused";

#[cfg(test)]
mod tests {
    use super::{
        GRANT_SYSCALL, MANAGER_COMM, REDEEM_SYSCALL, RELAY_CHAIN, RELAY_REDEEM_SYSCALL,
        SESSION_COMM, VIEWER_COMM, VIEWER_REDEEM_STEP,
    };

    /// The chain crosses three *distinct* principals, the session redeems
    /// bound to the manager, and the chain ends at the viewer.
    ///
    /// A chain whose principals collapsed — two of these names becoming
    /// equal — would still latch four records while proving nothing about a
    /// hand-off between processes, so the distinctness is pinned rather than
    /// assumed from the bundle names happening to differ today. A session
    /// step naming the unbound redemption would latch a relay that could have
    /// been steered into another process's delegation.
    #[test]
    fn the_chain_crosses_three_distinct_principals_and_ends_at_the_viewer() {
        assert_ne!(MANAGER_COMM, SESSION_COMM);
        assert_ne!(SESSION_COMM, VIEWER_COMM);
        assert_ne!(MANAGER_COMM, VIEWER_COMM);
        assert_eq!(RELAY_CHAIN[0], (MANAGER_COMM, GRANT_SYSCALL));
        assert_eq!(RELAY_CHAIN[1], (SESSION_COMM, RELAY_REDEEM_SYSCALL));
        assert_eq!(
            RELAY_CHAIN[VIEWER_REDEEM_STEP],
            (VIEWER_COMM, REDEEM_SYSCALL)
        );
    }
}
