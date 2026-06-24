//! Per-task address-space registry (increment **B** of the staged
//! user-memory copy path, `PLAN.md` Stage 7).
//!
//! The kernel's `copy_from_user` / `copy_to_user` boundary
//! ([`rustos_kernel_mem::uaccess`] /
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
//! [`PageTable`](rustos_kernel_mem::PageTable) backend, so the
//! kernel cannot hold a `BTreeMap<TaskId, AddressSpace<P>>` for a
//! single `P` — different tasks may run on different architecture page
//! tables, and the orchestrator that composes this registry into
//! `KernelState` is architecture-neutral. Each entry is therefore
//! stored behind the object-safe
//! [`UserAddressSpace`] (the read-only translate view the copy walk
//! needs) and a boxed [`PhysMap`]. The same erasure the kernel
//! already applies to the
//! direct map (`&dyn PhysMap`) is applied to the address space, so the
//! registry stays one concrete, non-generic type.
//!
//! # Lifecycle
//!
//! An entry is [`register`](AddressSpaceRegistry::register)ed when a
//! task's `rxe` image is mapped (the loader's
//! [`map_image`](rustos_kernel_mem::map_image) result handed to the
//! spawner) and [`withdraw`](AddressSpaceRegistry::withdraw)n when the
//! task exits. Both are fail-closed: registering an id that is already
//! present is refused rather than silently
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
use alloc::vec::Vec;

use rustos_abi::hwtree::{GrantedResource, HwResource};
use rustos_abi::{DescriptorTable, LimitKind, ResourceLimit};
use rustos_kernel_mem::{PhysMap, UserAddressSpace};
use rustos_kernel_sec::TaskId;

use crate::rlimit::LimitSet;

/// Why registering a task's address space was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AspaceError {
    /// An address space is already registered for this task id. The
    /// registry never silently replaces a live mapping — the caller
    /// must [`withdraw`](AddressSpaceRegistry::withdraw) the old task
    /// first (fail closed).
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
/// the `caps` and `ipc` registries — the registry
/// owns no lock of its own, so the synchronisation policy lives with
/// `KernelState`). It boots empty: entries appear only as tasks are
/// spawned and disappear as they exit.
#[derive(Default)]
pub struct AddressSpaceRegistry {
    tasks: BTreeMap<TaskId, TaskAddressSpace>,
    /// Each live task's standard-stream descriptor table. Co-located with the address space because it shares the
    /// exact per-process lifecycle — established at spawn, withdrawn at
    /// exit — and is keyed by the same [`TaskId`]; a parallel registry +
    /// lock would be near-duplicate plumbing.
    /// A task with no entry resolves to the fail-closed
    /// [`DescriptorTable::closed`] default, so an unestablished process
    /// can reach no stream backing.
    streams: BTreeMap<TaskId, DescriptorTable>,
    /// Each live task's effective resource limits. Held
    /// here for the same reason as [`Self::streams`]: it shares the exact
    /// per-process lifecycle (inherited at spawn, withdrawn at exit) and is
    /// keyed by the same [`TaskId`], so a parallel registry + lock would be
    /// near-duplicate plumbing. A task with no
    /// entry resolves to the [`LimitSet::DEFAULT`] policy via
    /// [`Self::limits`].
    limits: BTreeMap<TaskId, LimitSet>,
    /// Each live task's device-resource grants (the unforgeable, kernel-issued handles a driver task may map with
    /// `mmio_map`). Co-located with the address space for the same reason
    /// as [`Self::streams`] and [`Self::limits`]: a grant shares the exact
    /// per-process lifecycle — minted when a driver is admitted, reclaimed
    /// when the task exits — and is keyed by the same [`TaskId`], so a
    /// parallel registry + lock would be near-duplicate plumbing. A task with no entry owns no grants, so
    /// [`Self::grant`] resolves to `None` — fail closed: a task can
    /// map only the windows it was actually granted.
    grants: BTreeMap<TaskId, TaskGrants>,
    /// The discovered hardware-tree node each autoloaded **driver** task was
    /// loaded for. Recorded when a driver is spawned for
    /// a matched node, beside its grants, and keyed by the same kernel-trusted
    /// [`TaskId`]; an ordinary `spawn` (no matched node) records nothing.
    ///
    /// This is the security spine of `hw_emit_node`'s tree placement: when a driver publishes a discovered child,
    /// the kernel sets the child's parent to *this* node — the emitter's own —
    /// so a driver can neither forge its position in the tree nor parent a
    /// child under a node it was not loaded for. A task with no entry resolves
    /// to `None` via [`Self::loaded_node`], so a non-driver task (or one with
    /// no matched node) cannot emit a child at all (fail closed).
    /// Dropped at [`withdraw`](Self::withdraw) so a reused id never inherits a
    /// dead driver's node.
    loaded_nodes: BTreeMap<TaskId, u32>,
}

/// One task's device-resource grants: the handles it may pass to
/// `mmio_map`, each naming exactly one granted [`HwResource`].
///
/// Handles are minted per task, monotonically from `1` (handle `0` is the
/// reserved invalid value and is never issued), and are never reused
/// within a task's lifetime — so a stale handle from a reclaimed grant
/// can never alias a later one. The whole record is dropped when the task
/// is [`withdraw`](AddressSpaceRegistry::withdraw)n, so a reused [`TaskId`]
/// starts from an empty grant set and cannot inherit a dead task's windows
/// (fail closed).
#[derive(Default)]
struct TaskGrants {
    /// The next handle value to issue. Starts at `1`; only ever increases,
    /// so handles are unique for the task's whole lifetime.
    next_handle: u64,
    /// The granted resource behind each issued handle.
    by_handle: BTreeMap<u64, HwResource>,
}

impl AddressSpaceRegistry {
    /// Construct an empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tasks: BTreeMap::new(),
            streams: BTreeMap::new(),
            limits: BTreeMap::new(),
            grants: BTreeMap::new(),
            loaded_nodes: BTreeMap::new(),
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

    /// Replace `task`'s registered address-space snapshot with `space`,
    /// keeping its existing physical map, and return `true` if an entry was
    /// present to update.
    ///
    /// The registry stores a `Send + Sync`
    /// [`FrozenAddressSpace`](rustos_kernel_mem::vmm::FrozenAddressSpace)
    /// snapshot rather than the live, `!Sync` arch space (see
    /// [`rustos_kernel_mem::LiveUserSpace`]). A snapshot frozen at spawn
    /// describes only the task's spawn-time image and stack; once the task
    /// maps its own heap (`mem_map`), unmaps it, or a driver maps a granted
    /// window/DMA buffer, the snapshot is stale and the
    /// [`rustos_kernel_mem::uaccess`] copy path can no longer see the new
    /// (or freed) pages. The mutating syscall handler re-freezes the live
    /// space and calls this to publish the fresh snapshot, so the very next
    /// `copy_in` / `copy_out` reflects the current mappings (the copy path must see exactly the task's live memory; the
    /// behaviour
    /// [`FrozenAddressSpace`](rustos_kernel_mem::vmm::FrozenAddressSpace)'s
    /// docs prescribe for a remap path).
    ///
    /// The physical map is left untouched: it is the kernel direct map,
    /// identical across every snapshot of the same task, so re-freezing only
    /// the mappings is sufficient and avoids re-boxing it. A task with no
    /// registered entry is **not** created here — re-freezing concerns only
    /// a task that already has a space (a kernel task has none and reaches no
    /// user copy path), so the call is a no-op returning `false` (fail
    /// closed).
    pub fn reregister_space(
        &mut self,
        task: TaskId,
        space: Box<dyn UserAddressSpace + Send + Sync>,
    ) -> bool {
        match self.tasks.get_mut(&task) {
            Some(entry) => {
                entry.space = space;
                true
            }
            None => false,
        }
    }

    /// Withdraw `task`'s entry, returning `true` if one was present.
    ///
    /// Idempotent: withdrawing a task with no entry (e.g. a kernel task
    /// that never had a user address space, or a double `exit`) is a
    /// no-op that returns `false`. The task's standard-stream descriptor
    /// table is dropped at the same time so a reused id never inherits a
    /// dead task's streams (fail closed).
    pub fn withdraw(&mut self, task: TaskId) -> bool {
        let had_streams = self.streams.remove(&task).is_some();
        let had_limits = self.limits.remove(&task).is_some();
        let had_grants = self.grants.remove(&task).is_some();
        let had_node = self.loaded_nodes.remove(&task).is_some();
        self.tasks.remove(&task).is_some() || had_streams || had_limits || had_grants || had_node
    }

    /// Record that the autoloaded driver `task` was loaded for the discovered
    /// hardware-tree node `node_id`.
    ///
    /// Called by the privileged driver-spawn path beside
    /// [`mint_grant`](Self::mint_grant), under the same write lock, so a
    /// driver's matched node and its grants are established together. The
    /// `node_id` is kernel-sourced (the matched node the device manager
    /// resolved), never caller-supplied. The ordinary
    /// `spawn` path records nothing, so a non-driver task has no loaded node
    /// and cannot publish a child (fail closed).
    pub fn set_loaded_node(&mut self, task: TaskId, node_id: u32) {
        self.loaded_nodes.insert(task, node_id);
    }

    /// The discovered hardware-tree node `task` was loaded for, or `None`
    /// when `task` is not an autoloaded driver bound to a node.
    ///
    /// The security spine of `hw_emit_node`'s parent assignment: the kernel parents a driver's published
    /// child under *this* node, and a `task` with no loaded node may publish
    /// nothing (fail closed). The `task` argument is the kernel-trusted
    /// caller id, never caller-supplied.
    #[must_use]
    pub fn loaded_node(&self, task: TaskId) -> Option<u32> {
        self.loaded_nodes.get(&task).copied()
    }

    /// Mint a device-resource grant for `task`, returning the unforgeable,
    /// kernel-issued handle the task passes to `mmio_map` to reach exactly
    /// `resource` and nothing else (resources are
    /// capability-grant requests, never ambient handles; — a driver
    /// reaches only the resources its matched node requested).
    ///
    /// Called by the driver-admission path when a node's requested
    /// resources are granted to the driver task it loads. The returned
    /// handle is unique for `task`'s whole lifetime (monotonic from `1`),
    /// so it never aliases a previously reclaimed grant, and is meaningful
    /// only when presented by `task` itself: [`Self::grant`] is keyed by
    /// the kernel-trusted caller id, so another task passing the same
    /// numeric value resolves to nothing (handle forgery is refused).
    pub fn mint_grant(&mut self, task: TaskId, resource: HwResource) -> u64 {
        let entry = self.grants.entry(task).or_default();
        // Handle 0 is the reserved invalid value; the first minted handle
        // is 1. `next_handle` only ever increases within a task's life, so
        // a handle is never reused even after its grant is reclaimed.
        entry.next_handle += 1;
        let handle = entry.next_handle;
        entry.by_handle.insert(handle, resource);
        handle
    }

    /// Resolve the device-resource grant identified by `handle` for the
    /// owning `task`, or `None` (fail closed).
    ///
    /// Returns the granted [`HwResource`] iff `handle` was minted for
    /// `task`; `None` for an unknown handle, the reserved `0` handle, a
    /// handle minted for a *different* task (forgery — a driver cannot
    /// reach another driver's window by guessing a handle value), or a
    /// grant since reclaimed on exit. The `task` argument is the
    /// kernel-trusted caller id, never a caller-supplied value, so it is
    /// the security spine of the `mmio_map` handler (no
    /// trusted-caller shortcut;).
    #[must_use]
    pub fn grant(&self, task: TaskId, handle: u64) -> Option<HwResource> {
        self.grants.get(&task)?.by_handle.get(&handle).copied()
    }

    /// Serialise `task`'s device-resource grants as consecutive
    /// [`GrantedResource`] records (each [`GrantedResource::WIRE_LEN`]
    /// bytes), in ascending handle order, for delivery to the task through
    /// the `resource_grants` syscall.
    ///
    /// Returns an empty vector for a task with no grants — a valid, empty
    /// result, not an error (an unbound node is
    /// normal). The set is bounded by construction: handles are minted only
    /// by the kernel's driver-admission path, one per [`HwResource`] the
    /// matched node requested (no ambient authority), so a
    /// node's fixed resource maximum bounds the record count. Ascending
    /// handle order makes the delivered sequence deterministic
    /// ([`BTreeMap`] iterates by key).
    #[must_use]
    pub fn grants_to_le_bytes(&self, task: TaskId) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(entry) = self.grants.get(&task) {
            out.reserve(entry.by_handle.len() * GrantedResource::WIRE_LEN);
            for (&handle, &resource) in &entry.by_handle {
                out.extend_from_slice(&GrantedResource::new(handle, resource).to_le_bytes());
            }
        }
        out
    }

    /// Returns `true` iff one of `task`'s minted device-resource grants
    /// fully covers `resource` (`HwResource::covers`).
    ///
    /// This is the security spine of `hw_emit_node`: a
    /// user-space bus driver may publish a child node requesting `resource`
    /// only when it already holds a grant covering it, so an autoloaded
    /// child can never be minted authority its emitter lacks (no ambient authority). A `task` with no grants covers nothing,
    /// so an ungranted task fails closed. The `task` argument is the
    /// kernel-trusted caller id, never a caller-supplied value.
    #[must_use]
    pub fn grant_covers(&self, task: TaskId, resource: &HwResource) -> bool {
        self.grants
            .get(&task)
            .is_some_and(|entry| entry.by_handle.values().any(|grant| grant.covers(resource)))
    }

    /// Establish `task`'s standard-stream descriptor table.
    ///
    /// Called by the spawner when it admits a process, recording which
    /// inherited streams the child may read or write. Replacing an
    /// existing table is permitted: re-establishing the streams of a live
    /// task is the spawner's prerogative, and unlike the address space
    /// there is no live mapping to protect. A task whose table is never
    /// set resolves to [`DescriptorTable::closed`] via [`Self::streams`].
    pub fn set_streams(&mut self, task: TaskId, table: DescriptorTable) {
        self.streams.insert(task, table);
    }

    /// Resolve `task`'s standard-stream descriptor table, or the
    /// fail-closed [`DescriptorTable::closed`] default when none is
    /// established.
    ///
    /// The `stream_read` / `stream_write` handlers consult this to turn a
    /// caller's `fd` into the direction its backing supports. An unregistered task (a kernel task, or one withdrawn on
    /// `exit`) has every descriptor closed, so it can reach no backing.
    #[must_use]
    pub fn streams(&self, task: TaskId) -> DescriptorTable {
        self.streams.get(&task).copied().unwrap_or_default()
    }

    /// Establish `task`'s full effective resource-limit set.
    ///
    /// Called by the spawner when it admits a process, recording the limits
    /// the child inherited (already intersected against the system default,
    /// [`LimitSet::inherit`]). Replacing an existing set is permitted, as
    /// for [`Self::set_streams`]; a task whose set is never established
    /// resolves to [`LimitSet::DEFAULT`] via [`Self::limits`].
    pub fn set_limits(&mut self, task: TaskId, limits: LimitSet) {
        self.limits.insert(task, limits);
    }

    /// Update `task`'s effective limit for a single [`LimitKind`],
    /// leaving the other kinds untouched.
    ///
    /// The `rlimit_set` handler calls this once a request has been
    /// authorised ([`crate::authorize_set`]). A task with no established
    /// set starts from [`LimitSet::DEFAULT`], so the first imposed bound on
    /// any kind leaves every other kind at the default policy.
    pub fn set_limit(&mut self, task: TaskId, kind: LimitKind, limit: ResourceLimit) {
        let mut set = self.limits.get(&task).copied().unwrap_or_default();
        set.set(kind, limit);
        self.limits.insert(task, set);
    }

    /// Resolve `task`'s effective resource-limit set, or the
    /// [`LimitSet::DEFAULT`] policy when none is established.
    ///
    /// The `rlimit_get` / `rlimit_set` handlers consult this to read a
    /// caller's own effective limit. An unregistered
    /// task (a kernel task, or one withdrawn on `exit`) reads the default
    /// policy — reading one's own limit grants no authority.
    #[must_use]
    pub fn limits(&self, task: TaskId) -> LimitSet {
        self.limits.get(&task).copied().unwrap_or_default()
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
    fn reregister_space_swaps_the_snapshot_and_keeps_the_physmap() {
        let mut reg = AddressSpaceRegistry::new();
        // Register a snapshot that maps only page 1 (the "spawn-time" view).
        reg.register(TaskId(5), user_space(1, 100), sim())
            .expect("first registration succeeds");

        // The freeze-time snapshot cannot see page 2 yet — exactly the stale
        // state a `login` hit when its heap was mapped after spawn.
        let (space, _) = reg.resolve(TaskId(5)).expect("registered");
        assert!(space.translate(page(2)).is_none());

        // Re-freeze: a fresh snapshot that now also maps page 2 (the grown
        // heap). `reregister_space` reports the task was present.
        assert!(reg.reregister_space(TaskId(5), user_space(2, 200)));

        // The copy path now sees the newly-mapped page through the same task.
        let (space, physmap) = reg.resolve(TaskId(5)).expect("still registered");
        let (frame, flags) = space.translate(page(2)).expect("page 2 now resolves");
        assert_eq!(frame, Frame(200));
        assert!(flags.contains(MapFlags::USER));
        // The physical map survived the swap (its window still translates).
        assert!(physmap.translate(PhysAddr::new(0), PAGE_SIZE).is_some());
    }

    #[test]
    fn reregister_space_of_an_unregistered_task_is_a_no_op() {
        let mut reg = AddressSpaceRegistry::new();
        // A task with no entry is never created by a re-freeze (a kernel task
        // reaches no user copy path); the call fails closed.
        assert!(!reg.reregister_space(TaskId(9), user_space(1, 1)));
        assert!(!reg.contains(TaskId(9)));
        assert!(reg.resolve(TaskId(9)).is_none());
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

    #[test]
    fn unset_streams_resolve_to_the_closed_default() {
        let reg = AddressSpaceRegistry::new();
        // A task with no established table can reach no backing.
        assert_eq!(reg.streams(TaskId(9)), DescriptorTable::closed());
    }

    #[test]
    fn set_streams_then_resolve_returns_the_table() {
        let mut reg = AddressSpaceRegistry::new();
        reg.set_streams(TaskId(2), DescriptorTable::standard());
        assert_eq!(reg.streams(TaskId(2)), DescriptorTable::standard());
        // A different task is unaffected and stays fail-closed.
        assert_eq!(reg.streams(TaskId(3)), DescriptorTable::closed());
    }

    #[test]
    fn withdraw_clears_the_stream_table() {
        let mut reg = AddressSpaceRegistry::new();
        reg.set_streams(TaskId(4), DescriptorTable::standard());
        // Withdrawing a task with streams but no address space still
        // reports the slot was present and clears the table.
        assert!(reg.withdraw(TaskId(4)));
        assert_eq!(reg.streams(TaskId(4)), DescriptorTable::closed());
    }

    #[test]
    fn unset_limits_resolve_to_the_default_policy() {
        let reg = AddressSpaceRegistry::new();
        // A task with no established set runs under the default policy.
        assert_eq!(reg.limits(TaskId(9)), LimitSet::DEFAULT);
    }

    #[test]
    fn set_limit_updates_one_kind_and_leaves_the_rest_at_default() {
        let mut reg = AddressSpaceRegistry::new();
        let lo = ResourceLimit::new(4, 8).expect("well-formed");
        reg.set_limit(TaskId(2), LimitKind::Processes, lo);
        let set = reg.limits(TaskId(2));
        assert_eq!(set.get(LimitKind::Processes), lo);
        // Every other kind stays at the default policy.
        assert_eq!(set.get(LimitKind::OpenStreams), ResourceLimit::UNLIMITED);
        // A different task is unaffected and stays at the default policy.
        assert_eq!(reg.limits(TaskId(3)), LimitSet::DEFAULT);
    }

    #[test]
    fn set_limits_replaces_the_full_set() {
        let mut reg = AddressSpaceRegistry::new();
        let mut wanted = LimitSet::DEFAULT;
        wanted.set(
            LimitKind::StackBytes,
            ResourceLimit::new(1024, 4096).expect("well-formed"),
        );
        reg.set_limits(TaskId(7), wanted);
        assert_eq!(reg.limits(TaskId(7)), wanted);
    }

    #[test]
    fn withdraw_clears_the_limit_set() {
        let mut reg = AddressSpaceRegistry::new();
        reg.set_limit(
            TaskId(4),
            LimitKind::Processes,
            ResourceLimit::new(1, 2).expect("well-formed"),
        );
        // Withdrawing a task with limits but no address space still reports
        // the slot was present and resets it to the default policy.
        assert!(reg.withdraw(TaskId(4)));
        assert_eq!(reg.limits(TaskId(4)), LimitSet::DEFAULT);
    }

    // --- device-resource grants ----------------

    /// A register window resource used across the grant tests.
    fn window() -> HwResource {
        HwResource::mmio(0xFE98_0000, 0x4000)
    }

    #[test]
    fn unminted_grant_resolves_to_none() {
        let reg = AddressSpaceRegistry::new();
        // A task with no grants can map nothing, and handle 0 (the reserved
        // invalid value) is never a live grant.
        assert_eq!(reg.grant(TaskId(9), 1), None);
        assert_eq!(reg.grant(TaskId(9), 0), None);
    }

    #[test]
    fn mint_then_grant_returns_the_resource_for_its_owner() {
        let mut reg = AddressSpaceRegistry::new();
        let handle = reg.mint_grant(TaskId(2), window());
        // The first minted handle is 1 (handle 0 stays reserved-invalid).
        assert_eq!(handle, 1);
        assert_eq!(reg.grant(TaskId(2), handle), Some(window()));
        // The reserved handle still resolves to nothing for the owner.
        assert_eq!(reg.grant(TaskId(2), 0), None);
    }

    #[test]
    fn handles_are_unique_per_task_and_name_distinct_resources() {
        let mut reg = AddressSpaceRegistry::new();
        let a = HwResource::mmio(0xFE98_0000, 0x4000);
        let b = HwResource::bus_window(0x6000_0000, 0x40_0000, 0xF800_0000);
        let h_a = reg.mint_grant(TaskId(2), a);
        let h_b = reg.mint_grant(TaskId(2), b);
        assert_ne!(h_a, h_b, "each grant gets its own handle");
        assert_eq!(reg.grant(TaskId(2), h_a), Some(a));
        assert_eq!(reg.grant(TaskId(2), h_b), Some(b));
    }

    #[test]
    fn grant_is_owner_bound_against_handle_forgery() {
        let mut reg = AddressSpaceRegistry::new();
        let handle = reg.mint_grant(TaskId(2), window());
        // The owner resolves its grant; an unknown handle value does not.
        assert_eq!(reg.grant(TaskId(2), handle), Some(window()));
        assert_eq!(reg.grant(TaskId(2), handle + 1), None);
        // A *different* task passing the same numeric handle reaches
        // nothing — a driver cannot map another driver's window by reusing
        // its handle value.
        assert_eq!(reg.grant(TaskId(3), handle), None);
    }

    #[test]
    fn withdraw_reclaims_every_grant() {
        let mut reg = AddressSpaceRegistry::new();
        let handle = reg.mint_grant(TaskId(4), window());
        assert_eq!(reg.grant(TaskId(4), handle), Some(window()));
        // Withdrawing a task with grants but no address space still reports
        // the slot was present and clears the grants (reclaimed on
        // exit).
        assert!(reg.withdraw(TaskId(4)));
        assert_eq!(reg.grant(TaskId(4), handle), None);
    }

    #[test]
    fn reused_task_id_starts_from_an_empty_grant_set() {
        let mut reg = AddressSpaceRegistry::new();
        let old = reg.mint_grant(TaskId(5), window());
        assert!(reg.withdraw(TaskId(5)));
        // A new task reusing the id mints from handle 1 again and never
        // inherits the dead task's grant.
        let fresh = reg.mint_grant(TaskId(5), HwResource::mmio(0x3F20_0000, 0x1000));
        assert_eq!(fresh, 1);
        assert_eq!(old, 1);
        assert_eq!(
            reg.grant(TaskId(5), fresh),
            Some(HwResource::mmio(0x3F20_0000, 0x1000))
        );
    }
}
