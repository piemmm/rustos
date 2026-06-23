//! The kernel-held hardware-tree source the `hw_tree_read` /
//! `hw_tree_wait` syscalls serve (`AGENTS.md` §16.6 / §18.1 / §18.4,
//! Design D — `.junie/next-pi-prompt.md`).
//!
//! The discovered hardware inventory itself lives in the binding kernel
//! (`rustos-kernel`'s `HwTreeStore` / `HW_TREE`); this trait is the seam
//! `kernel/core` reaches it through, exactly as [`crate::users`]'s
//! [`UsersDbSource`](crate::users::UsersDbSource) is the seam for the user
//! database. Keeping the seam here, returning an *already wire-encoded*
//! snapshot, keeps `kernel/core` ignorant of the inventory's storage and
//! of the `lib/abi` wire layout — the single encoder lives beside the
//! store it serialises (`AGENTS.md` §2.2).
//!
//! Both reads fail closed: a build with no source installed answers
//! [`Errno::NotImplemented`] so an early `hw_tree_read` / `hw_tree_wait`
//! announces an inert interface rather than fabricating a tree
//! (`AGENTS.md` §2.9 / §5.4).

use alloc::vec::Vec;

use rustos_abi::{Errno, HwNode};

/// The kernel-held discovered hardware tree the hardware-tree syscalls
/// serve (`AGENTS.md` §18.1 / §18.4).
///
/// The boot path installs an implementation backed by the binding
/// kernel's authoritative `HwTreeStore`; the `hw_tree_read` handler copies
/// [`Self::snapshot`]'s bytes out to the (capability-gated,
/// `CAP_SYSINFO_HW`) caller, and the `hw_tree_wait` handler blocks on
/// [`Self::generation`] advancing.
///
/// `Sync` because the single installed source is shared by the per-CPU
/// syscall handlers, exactly like [`crate::users::UsersDbSource`].
pub trait HwTreeSource: Sync {
    /// The store's current mutation generation (`AGENTS.md` §18.4).
    ///
    /// Monotonically increasing; a `hw_tree_wait` caller blocks while this
    /// equals the value it last observed and wakes when it differs.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullHwTreeSource`] to
    /// mark an inert interface (`AGENTS.md` §2.9).
    fn generation(&self) -> Result<u64, Errno>;

    /// An owned, wire-encoded snapshot of the current tree: a
    /// [`rustos_abi::hwtree::HwTreeHeader`] (the generation it was taken at
    /// and the node count) followed by that many
    /// [`rustos_abi::hwtree::HwNode`] records, all little-endian.
    ///
    /// The generation in the returned header and the node bytes are read
    /// together so a `hw_tree_read` caller's header generation always
    /// matches the nodes it received (`AGENTS.md` §18.4).
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullHwTreeSource`].
    fn snapshot(&self) -> Result<Vec<u8>, Errno>;

    /// Publish a discovered child `node` under parent `parent_id` into the
    /// live tree, bumping the generation so every parked `hw_tree_wait`
    /// caller (the device manager) re-reads and re-evaluates it
    /// (`AGENTS.md` §18.1 / §18.3 — recursive, user-space hardware
    /// discovery).
    ///
    /// This is the store side of the `hw_emit_node` syscall: the handler in
    /// [`crate::syscalls`] has already verified the calling driver holds
    /// [`rustos_abi::CapabilityId::HW_EMIT`], resolved `parent_id` to the
    /// emitter's *own* matched node (so a driver cannot forge its tree
    /// position), and checked that every
    /// [`rustos_abi::hwtree::HwResource`] the node requests is covered by
    /// one of the caller's minted grants (`AGENTS.md` §4 — no ambient
    /// authority). The store **owns identity**: it assigns the node a
    /// fresh, collision-free [`id`](rustos_abi::HwNode::id) and sets its
    /// parent to `parent_id` ([`HwNode::set_identity`]) before recording
    /// it, so an emitter-chosen id can never collide with an existing node
    /// (`AGENTS.md` §5.4) — load-bearing, since the driver-store load path
    /// resolves a matched node by its id. The node is always added, never
    /// dropped, and only the generation advances.
    ///
    /// # Errors
    ///
    /// [`Errno::NotImplemented`] from the default [`NullHwTreeSource`] — a
    /// build with no store wired never accepts a published node
    /// (`AGENTS.md` §2.9 / §5.4).
    fn publish(&self, parent_id: u32, node: HwNode) -> Result<(), Errno>;
}

/// The hardware-tree source installed before any real store is wired.
///
/// Every read fails closed with [`Errno::NotImplemented`] — a kernel build
/// with no hardware-tree store wired never fabricates an inventory
/// (`AGENTS.md` §2.9 / §5.4).
#[derive(Debug, Default, Copy, Clone)]
pub struct NullHwTreeSource;

impl HwTreeSource for NullHwTreeSource {
    fn generation(&self) -> Result<u64, Errno> {
        Err(Errno::NotImplemented)
    }

    fn snapshot(&self) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotImplemented)
    }

    fn publish(&self, _parent_id: u32, _node: HwNode) -> Result<(), Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullHwTreeSource`] instance the syscall handler defaults to
/// until a boot path installs a real store through
/// `KernelSyscallHandlers::with_hw_tree` (mirrors [`crate::users::NULL_USERS_DB`]).
pub static NULL_HW_TREE: NullHwTreeSource = NullHwTreeSource;
