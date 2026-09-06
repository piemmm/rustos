//! The filesystem-mutation vertical's shared contract
//! (`plans/NEW-FILEMANAGER.md` FM9).
//!
//! The freestanding guest kernel (`src/main.rs`) and the host runner's
//! enrolment (`tools/xtask/src/commands/qemu_tests.rs`) both read these
//! definitions, so the gesture the host injects and the witness the guest
//! latches can never drift apart.
//!
//! # What the vertical proves that no host test can
//!
//! The write syscalls emit a kernel-attested mutation record
//! (`FsNodeMutated`, the audit trail a security review reads to see who
//! changed what), and that emission is host-tested per operation. What no host
//! test can show is that a **user's own pointer gesture** reaches it: that a
//! click on a drawn menu row is routed to the surface that owns it, becomes
//! the write the desktop intended, is authorised under the logged-in
//! account's own identity, and lands in the trail naming the path the user
//! acted on.
//!
//! Until this vertical there was no guest coverage of that at all — no run in
//! the tree produced a single mutation record — so the whole path from gesture
//! to audit trail was unexercised on a running kernel.
//!
//! # Who states what
//!
//! - The **host** reads the serial transcript and gates each gesture on the
//!   surface it targets being on screen: the session's own menu and window
//!   announcements, which only the session can make.
//! - The **guest** kernel's audit sink sees kernel audit records only, so it
//!   gates on those: the mutation operations below, each attributed by the
//!   path the record carries rather than by a count of how many mutations have
//!   gone by (`plans/OPEN-DEFECTS.md` D19/D20).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The `op` field value the kernel records for a directory creation.
///
/// Spelled as the mutation audit site renders it, so the witness matches the
/// emitter rather than a paraphrase of it.
pub const MKDIR_OP: &str = "mkdir";

/// Trailing component of the path the desktop's New Folder command creates.
///
/// The mutation witness matches on this rather than on how many mutations
/// have gone by, because the file manager legitimately creates its Trash
/// directory when a window opens: a count would latch on that instead, and
/// would shift the moment the manager's start-up changed.
///
/// The creator's own definition is `tairix_browse::NEW_FOLDER_BASE`, which
/// this crate cannot depend on: the browse engine links the userland runtime,
/// and its allocator and panic handler cannot coexist with the guest kernel
/// binary. So the name is mirrored here and pinned equal by a host test, which
/// is what stops the two drifting.
pub const CREATED_LEAF: &str = "New Folder";

#[cfg(test)]
mod tests {
    /// The mirrored name is the one the desktop's New Folder command actually
    /// creates. Pinned here because the guest cannot link the engine that
    /// defines it, so a change there would otherwise leave this witness
    /// matching a name nothing produces — and the run would fail as a timeout
    /// rather than as the drift it is.
    #[test]
    fn the_created_leaf_is_the_creators_own_name() {
        assert_eq!(super::CREATED_LEAF, tairix_browse::NEW_FOLDER_BASE);
    }
}
