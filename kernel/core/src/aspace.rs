//! Per-task address-space registry (increment **B** of the staged
//! user-memory copy path, `PLAN.md` Stage 7).
//!
//! The kernel's `copy_from_user` / `copy_to_user` boundary
//! ([`rustos_kernel_mem::uaccess`], `AGENTS.md` §5.4 /
//! `tests/SECURITY.md` §5) walks the *calling task's* address space.
//! A syscall handler therefore needs to turn the caller's
//! [`rustos_kernel_sec::TaskId`] into the pair the copy path consumes:
//! the task's user [`AddressSpace`](rustos_kernel_mem::AddressSpace)
//! and the kernel [`PhysMap`] that backs it. This module owns that
//! mapping.
//!
//! # Why trait objects
//!
//! [`rustos_kernel_mem::AddressSpace`] is generic over its
//! [`PageTableOps`](rustos_kernel_mem::PageTableOps) backend, so the
//! kernel cannot hold a `BTreeMap<TaskId, AddressSpace<P>>` for a
//! single `P` — different tasks may run on different architecture page
//! tables, and the orchestrator that composes this registry into
//! `KernelState` is architecture-neutral. Each entry is therefore
//! stored behind the object-safe
//! [`UserAddressSpace`] (the read-only translate view the copy walk
//! needs) and a boxed [`PhysMap`]. The same erasure the kernel
//! already applies to the
//! direct map (`&dyn PhysMap`) is applied to the address space, so the
//! registry stays one concrete, non-generic type
//! (`AGENTS.md` §2.2 / §2.3).
//!
//! # Lifecycle
//!
//! An entry is [`register`](AddressSpaceRegistry::register)ed when a
//! task's `rxe` image is mapped (the loader's
//! [`map_image`](rustos_kernel_mem::map_image) result handed to the
//! spawner) and [`withdraw`](AddressSpaceRegistry::withdraw)n when the
//! task exits. Both are fail-closed: registering an id that is already
//! present is refused (`AGENTS.md` §5.4) rather than silently
//! replacing a live mapping, and resolving an unknown id yields
//! `None`. The registry is a pure data structure with no ambient
//! authority and no audit sink of its own — the call sites that drive
//! the lifecycle (the spawner, the `exit` handler) own the
//! security-relevant logging, exactly as the dispatcher audits IPC
//! endpoint lookups rather than [`PortRegistry`] doing so internally.
//!
//! [`PortRegistry`]: rustos_kernel_ipc::PortRegistry

use alloc::boxed::Box;
use alloc::collections::BTreeMap;

use rustos_kernel_mem::{PhysMap, UserAddressSpace};
use rustos_kernel_sec::TaskId;

/// Why registering a task's address space was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AspaceError {
    /// An address space is already registered for this task id. The
    /// registry never silently replaces a live mapping — the caller
    /// must [`withdraw`](AddressSpaceRegistry::withdraw) the old task
    /// first (`AGENTS.md` §5.4 — fail closed).
    AlreadyPresent,
}

/// One task's stored user address space and the physical map backing it.
///
/// Held only inside [`AddressSpaceRegistry`]; exposed to callers solely
/// as the borrowed pair returned by
/// [`resolve`](AddressSpaceRegistry::resolve).
struct TaskAddressSpace {
    space: Box<dyn UserAddressSpace + Send + Sync>,
    physmap: Box<dyn PhysMap + Send + Sync>,
}

/// Maps each live task's [`TaskId`] to its user address space and the
/// kernel [`PhysMap`] backing it.
///
/// Composed into `KernelState` as a `RwLock`-wrapped field (mirroring
/// the `caps` and `ipc` registries, `AGENTS.md` §2.1 — the registry
/// owns no lock of its own, so the synchronisation policy lives with
/// `KernelState`). It boots empty: entries appear only as tasks are
/// spawned and disappear as they exit.
#[derive(Default)]
pub struct AddressSpaceRegistry {
    tasks: BTreeMap<TaskId, TaskAddressSpace>,
}

impl AddressSpaceRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tasks: BTreeMap::new(),
        }
    }

    /// Register `task`'s user address space and the physical map that
    /// backs it.
    ///
    /// # Errors
    ///
    /// [`AspaceError::AlreadyPresent`] if an address space is already
    /// registered for `task`; the existing entry is left untouched.
    pub fn register(
        &mut self,
        task: TaskId,
        space: Box<dyn UserAddressSpace + Send + Sync>,
        physmap: Box<dyn PhysMap + Send + Sync>,
    ) -> Result<(), AspaceError> {
        if self.tasks.contains_key(&task) {
            return Err(AspaceError::AlreadyPresent);
        }
        self.tasks.insert(task, TaskAddressSpace { space, physmap });
        Ok(())
    }

    /// Withdraw `task`'s entry, returning `true` if one was present.
    ///
    /// Idempotent: withdrawing a task with no entry (e.g. a kernel task
    /// that never had a user address space, or a double `exit`) is a
    /// no-op that returns `false`.
    pub fn withdraw(&mut self, task: TaskId) -> bool {
        self.tasks.remove(&task).is_some()
    }

    /// Resolve `task` to the `(address space, physical map)` pair the
    /// [`rustos_kernel_mem::uaccess`] copy path consumes, or `None` if
    /// no entry is registered.
    #[must_use]
    pub fn resolve(&self, task: TaskId) -> Option<(&dyn UserAddressSpace, &dyn PhysMap)> {
        self.tasks.get(&task).map(|entry| {
            // Drop the `Send + Sync` auto-trait bounds the stored boxes
            // carry: the copy path only needs the bare read-only views,
            // and the registry's own `RwLock` already governs sharing.
            let space: &dyn UserAddressSpace = &*entry.space;
            let physmap: &dyn PhysMap = &*entry.physmap;
            (space, physmap)
        })
    }

    /// Whether an address space is registered for `task`.
    #[must_use]
    pub fn contains(&self, task: TaskId) -> bool {
        self.tasks.contains_key(&task)
    }

    /// Number of tasks with a registered address space.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Whether the registry holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_kernel_mem::{
        AddressSpace, Frame, HostPageTable, MapFlags, Page, PhysAddr, SimPhysMap, VirtAddr,
        PAGE_SIZE,
    };

    fn page(n: u64) -> Page {
        Page::from_addr(VirtAddr::new(n * PAGE_SIZE as u64)).expect("aligned page")
    }

    /// Build a user address space with one mapped, user-readable page at
    /// page `n` → frame `frame`, boxed behind the object-safe trait.
    fn user_space(n: u64, frame: usize) -> Box<dyn UserAddressSpace + Send + Sync> {
        let mut space = AddressSpace::new(HostPageTable::new());
        space
            .map(page(n), Frame(frame), MapFlags::READ | MapFlags::USER)
            .expect("mapped");
        Box::new(space)
    }

    fn sim() -> Box<dyn PhysMap + Send + Sync> {
        Box::new(SimPhysMap::new(PhysAddr::new(0), PAGE_SIZE))
    }

    #[test]
    fn new_registry_is_empty() {
        let reg = AddressSpaceRegistry::new();
        assert!(reg.is_empty());
        assert_eq!(reg.len(), 0);
        assert!(!reg.contains(TaskId(1)));
        assert!(reg.resolve(TaskId(1)).is_none());
    }

    #[test]
    fn register_then_resolve_returns_the_pair() {
        let mut reg = AddressSpaceRegistry::new();
        reg.register(TaskId(7), user_space(1, 9), sim())
            .expect("first registration succeeds");
        assert!(reg.contains(TaskId(7)));
        assert_eq!(reg.len(), 1);

        let (space, _physmap) = reg.resolve(TaskId(7)).expect("registered task resolves");
        // The boxed trait object forwards `translate` to the underlying
        // `AddressSpace<HostPageTable>`.
        let (frame, flags) = space.translate(page(1)).expect("page resolves");
        assert_eq!(frame, Frame(9));
        assert!(flags.contains(MapFlags::USER));
    }

    #[test]
    fn duplicate_registration_is_rejected_and_keeps_first_entry() {
        let mut reg = AddressSpaceRegistry::new();
        reg.register(TaskId(3), user_space(1, 100), sim())
            .expect("first registration succeeds");
        let err = reg
            .register(TaskId(3), user_space(2, 200), sim())
            .expect_err("second registration for same task is refused");
        assert_eq!(err, AspaceError::AlreadyPresent);
        // The original entry survives untouched: page 1 → frame 100 is
        // still mapped, page 2 (from the rejected entry) is not.
        let (space, _) = reg.resolve(TaskId(3)).expect("first entry intact");
        assert_eq!(space.translate(page(1)).expect("page 1").0, Frame(100));
        assert!(space.translate(page(2)).is_none());
    }

    #[test]
    fn withdraw_removes_only_the_named_task() {
        let mut reg = AddressSpaceRegistry::new();
        reg.register(TaskId(1), user_space(1, 1), sim()).unwrap();
        reg.register(TaskId(2), user_space(1, 2), sim()).unwrap();

        assert!(reg.withdraw(TaskId(1)));
        assert!(!reg.contains(TaskId(1)));
        assert!(reg.resolve(TaskId(1)).is_none());
        assert!(reg.contains(TaskId(2)));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn withdrawing_unknown_task_is_a_noop() {
        let mut reg = AddressSpaceRegistry::new();
        assert!(!reg.withdraw(TaskId(42)));
        reg.register(TaskId(1), user_space(1, 1), sim()).unwrap();
        // Double withdraw: the second call finds nothing.
        assert!(reg.withdraw(TaskId(1)));
        assert!(!reg.withdraw(TaskId(1)));
    }

    #[test]
    fn re_register_after_withdraw_succeeds() {
        let mut reg = AddressSpaceRegistry::new();
        reg.register(TaskId(5), user_space(1, 10), sim()).unwrap();
        assert!(reg.withdraw(TaskId(5)));
        // A new task reusing the same id (after the old one exited) can
        // register again — withdrawal fully clears the slot.
        reg.register(TaskId(5), user_space(3, 30), sim())
            .expect("re-registration after withdraw succeeds");
        let (space, _) = reg.resolve(TaskId(5)).expect("re-registered");
        assert_eq!(space.translate(page(3)).expect("page 3").0, Frame(30));
    }
}
