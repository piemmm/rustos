//! Revoking a removed device's authority from every task that holds it
//! (`plans/OPEN-DEFECTS.md` D230).
//!
//! A node's grants are minted to the driver admitted for it and carried into
//! whatever it delegates, each naming the node as its origin. When the node
//! leaves the tree they are all revoked at once, so nothing new is reached
//! through them, and each holder's standing reach into the device is torn
//! down: its bindings of the node's interrupt lines (a parked wait wakes
//! `NotFound`) and its windows onto the node's registers, shot down on every
//! CPU, so its next access to one faults.
//!
//! Shared RAM is not the device. A region the node conferred stays mapped
//! wherever it is mapped, so no holder is killed, or handed fabricated
//! contents in place of a reply that already landed, for a device going
//! away; the region is retired instead, so it can never carry another
//! device's data. Only a region a node still in the tree also confers (a
//! transport a parent republished) outlives the removed session: a holder's
//! mappings of it are withdrawn and the holder killed, since its pointers
//! into them could otherwise alias whatever is mapped there next.
//!
//! Tree removal precedes revocation, and admission checks the tree after it
//! mints, so a driver admitted during a removal is revoked here or refused
//! there. A holder that maps or binds between the revocation and its
//! teardown re-checks its grant after the operation and undoes its own.

use alloc::boxed::Box;
use alloc::sync::Arc;

use tairix_abi::hwtree::{HwResource, HwResourceKind};
use tairix_abi::{Errno, IrqHandle, Signal};
use tairix_arch_api::CrossCpuTlbShootdown;
use tairix_kernel_irq::IrqTable;
use tairix_kernel_sec::ProcessId;
use tairix_sync::RwLock;

use crate::aspace::pages_spanning;
use crate::aspace::AddressSpaceRegistry;
use crate::devres::SharedMemFacility;
use crate::live_producer::live_errno;
use crate::procsignal::ProcessSignal;
use crate::procspace::ProcessSpace;

/// What one revocation withdrew.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Revoked {
    /// Grants revoked, across every holder.
    pub grants: usize,
    /// Tasks that held any.
    pub holders: usize,
    /// Holders killed because their access could not be withdrawn in place.
    pub killed: usize,
}

/// The kernel state a revocation reaches.
pub struct Revoker<'a> {
    /// Where the grants, the live spaces and the snapshots are.
    pub aspaces: &'a RwLock<AddressSpaceRegistry>,
    /// The interrupt bindings.
    pub irq: &'a IrqTable,
    /// What frees a shared region's frames at its last reference.
    pub shared: &'a dyn SharedMemFacility,
    /// [`None`] only where there is no TLB to shoot down.
    pub shootdown: Option<&'a (dyn CrossCpuTlbShootdown + Sync)>,
    /// What kills a holder whose access cannot be withdrawn in place.
    pub signal: &'a dyn ProcessSignal,
}

impl Revoker<'_> {
    /// Revoke every grant whose origin is one of `nodes` (sorted ascending)
    /// and tear down what its holders still reach.
    ///
    /// The caller serialises revocations, so every revoked grant a walk finds
    /// is its own, and none is left behind when it returns.
    pub fn revoke(&self, nodes: &[u32]) -> Revoked {
        let grants = self.aspaces.write().revoke_node_grants(nodes);
        let mut revoked = Revoked {
            grants,
            ..Revoked::default()
        };
        if grants == 0 {
            return revoked;
        }
        let mut after = None;
        loop {
            let next = self.aspaces.read().next_revoked_holder(after);
            let Some(holder) = next else {
                return revoked;
            };
            after = Some(holder);
            revoked.holders += 1;
            let space = self.aspaces.read().live_space(holder);
            let withdrawn = self.tear_down(holder, space.as_deref());
            if withdrawn != Ok(true) && self.kill(holder, space.as_ref()) {
                revoked.killed += 1;
            }
            self.aspaces.write().retire_revoked(holder);
        }
    }

    /// Tear down `holder`'s standing access through its revoked grants,
    /// carrying on past a failure so as much as possible is withdrawn.
    /// `Ok(false)` when a mapping had to be withdrawn from under it.
    fn tear_down(&self, holder: ProcessId, space: Option<&ProcessSpace>) -> Result<bool, Errno> {
        let mut outcome = Ok(true);
        let (mut lines, mut windows) = (false, false);
        let mut after = None;
        loop {
            let next = self.aspaces.read().next_revoked_grant(holder, after);
            let Some((handle, resource)) = next else {
                break;
            };
            after = Some(handle);
            match resource.kind() {
                Some(HwResourceKind::Irq) => lines = true,
                Some(HwResourceKind::Shared) => {
                    let kept = self.withdraw_region(holder, space, resource.base());
                    outcome = outcome.and_then(|all| kept.map(|kept| all && kept));
                }
                Some(
                    HwResourceKind::Mmio | HwResourceKind::BusWindow | HwResourceKind::Framebuffer,
                ) => windows = true,
                _ => {}
            }
        }
        if lines {
            self.release_lines(holder);
        }
        if let Some(space) = space.filter(|_| windows) {
            outcome = self.sweep_windows(holder, space).and(outcome);
        }
        outcome
    }

    /// Settle `holder`'s revoked grant for shared region `region`, returning
    /// whether its mappings were left in place.
    fn withdraw_region(
        &self,
        holder: ProcessId,
        space: Option<&ProcessSpace>,
        region: u64,
    ) -> Result<bool, Errno> {
        let (live, covered) = {
            let aspaces = self.aspaces.read();
            let covered = aspaces.grant_covers(holder, &HwResource::shared(region));
            (aspaces.region_conferred_live(region), covered)
        };
        if !live {
            crate::sharedreg::retire(region);
            return Ok(true);
        }
        let Some(space) = space.filter(|_| !covered) else {
            return Ok(true);
        };
        let mut kept = true;
        while let Some(base) = crate::sharedreg::mapping_of(holder, region) {
            match self.unmap_mapping(holder, space, base, region) {
                Ok(()) => kept = false,
                Err(Errno::NotFound) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(kept)
    }

    /// Keep `process`'s binding `handle` of `line` only while a grant still
    /// names the line: one revoked while it was being bound may have been
    /// passed by the revocation's walk, so it is undone here.
    ///
    /// # Errors
    ///
    /// [`Errno::PermissionDenied`] once the binding is released.
    pub(crate) fn keep_binding(
        &self,
        process: ProcessId,
        line: u32,
        handle: IrqHandle,
    ) -> Result<IrqHandle, Errno> {
        if self.aspaces.read().holds_irq_line(process, line) {
            return Ok(handle);
        }
        self.irq.release_binding(handle, process);
        Err(Errno::PermissionDenied)
    }

    /// Release every binding of `holder` whose line no grant it still holds
    /// names, and wake its parked waits to find them gone. `irq_bind` binds
    /// only under a live grant, so these are exactly the revoked ones.
    fn release_lines(&self, holder: ProcessId) {
        let mut released = false;
        let mut after = None;
        while let Some(binding) = self.irq.next_binding_of(holder, after) {
            after = Some(binding.line);
            if !self.aspaces.read().holds_irq_line(holder, binding.line) {
                released |= self.irq.release_binding(binding.handle, holder);
            }
        }
        if released {
            crate::waitq::irq_wake();
        }
    }

    /// Withdraw `holder`'s mapping of shared region `region` at `base` from
    /// `space`: its entries and snapshot pages under the space's lock, then
    /// every CPU's TLB, and only then the reference that may free the frames.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] if `base` no longer maps `region`, or the unmap
    /// that failed, which leaves the region allocated.
    pub(crate) fn unmap_mapping(
        &self,
        holder: ProcessId,
        space: &ProcessSpace,
        base: u64,
        region: u64,
    ) -> Result<(), Errno> {
        let mut absorbed = true;
        let unmapped =
            crate::sharedreg::unmap_with(self.shared, holder, base, Some(region), |base, len| {
                space.with(|live| {
                    live.unmap_shared(base, len).map_err(live_errno)?;
                    absorbed = self.forget_snapshot(holder, base, pages_spanning(len as u64));
                    Ok(())
                })
            })?;
        if !absorbed {
            self.refreeze(holder, space);
        }
        self.shoot_down(base, pages_spanning(unmapped.len() as u64));
        drop(unmapped);
        Ok(())
    }

    /// Unmap every device window of `holder` that no grant it still holds
    /// authorises.
    ///
    /// # Errors
    ///
    /// The unmap that failed; the windows before it are gone.
    pub(crate) fn sweep_windows(
        &self,
        holder: ProcessId,
        space: &ProcessSpace,
    ) -> Result<bool, Errno> {
        let mut absorbed = true;
        let swept = space.with(|live| {
            live.retain_device_windows(
                &mut |phys, len| self.aspaces.read().maps_window(holder, phys, len),
                &mut |base, pages| {
                    absorbed &= self.forget_snapshot(holder, base, pages);
                    self.shoot_down(base, pages);
                },
            )
        });
        if !absorbed {
            self.refreeze(holder, space);
        }
        swept.map(|()| true).map_err(live_errno)
    }

    /// Kill `holder` if `space` is still its live space, returning whether it
    /// was signalled. A holder whose space is gone reaches nothing, and its id
    /// may already name another task.
    fn kill(&self, holder: ProcessId, space: Option<&Arc<ProcessSpace>>) -> bool {
        let current = self.aspaces.read().live_space(holder);
        let same = matches!((space, current), (Some(space), Some(current)) if Arc::ptr_eq(space, &current));
        same && self.signal.signal_task(holder, Signal::Kill).is_ok()
    }

    /// Drop the unmapped pages `[base, base + pages)` from `holder`'s
    /// snapshot, returning whether it took the removal as a delta.
    fn forget_snapshot(&self, holder: ProcessId, base: u64, pages: u64) -> bool {
        self.aspaces
            .write()
            .forget_region_pages(holder, base, pages)
    }

    fn shoot_down(&self, base: u64, pages: u64) {
        if let Some(shootdown) = self.shootdown {
            shootdown.shootdown_range(base, usize::try_from(pages).unwrap_or(usize::MAX));
        }
    }

    /// Rebuild `holder`'s snapshot from its own live space, never the
    /// caller's.
    fn refreeze(&self, holder: ProcessId, space: &ProcessSpace) {
        let frozen = space.with(|live| live.freeze());
        self.aspaces
            .write()
            .reregister_space(holder, Box::new(frozen));
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    use tairix_abi::hwtree::HwResource;
    use tairix_kernel_irq::WaitStep;
    use tairix_kernel_mem::{
        LiveSpaceError, LiveUserSpace, MmioError, Page, PhysAddr, PhysMap, SharedMemory,
        SimPhysMap, VirtAddr, PAGE_SIZE,
    };

    use crate::devres::SharedChunk;

    /// What the kernel did, in the order it did it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Event {
        Shootdown(u64, usize),
        Freed(u64),
    }

    #[derive(Default)]
    struct Log(Mutex<Vec<Event>>);

    impl Log {
        fn events(&self) -> Vec<Event> {
            self.0.lock().unwrap().clone()
        }
        fn push(&self, event: Event) {
            self.0.lock().unwrap().push(event);
        }
    }

    impl CrossCpuTlbShootdown for Log {
        fn shootdown_page(&self, vaddr: u64) {
            self.push(Event::Shootdown(vaddr, 1));
        }
        fn shootdown_range(&self, start_vaddr: u64, page_count: usize) {
            self.push(Event::Shootdown(start_vaddr, page_count));
        }
    }

    /// A shared-memory facility that maps every region into one space, as
    /// the production one maps into the caller's, and logs each free.
    struct IntoSpace<'a> {
        space: Arc<ProcessSpace>,
        log: &'a Log,
        next_phys: AtomicU64,
    }

    impl<'a> IntoSpace<'a> {
        fn new(space: &Arc<ProcessSpace>, log: &'a Log) -> Self {
            Self {
                space: Arc::clone(space),
                log,
                next_phys: AtomicU64::new(0x2000_0000),
            }
        }
    }

    impl SharedMemFacility for IntoSpace<'_> {
        fn alloc_region(&self, pages: u64) -> Result<Vec<SharedChunk>, Errno> {
            let phys_base = self
                .next_phys
                .fetch_add(pages * PAGE_SIZE as u64, Ordering::Relaxed);
            Ok(alloc::vec![SharedChunk {
                phys_base,
                order: 0,
                pages
            }])
        }
        fn map_region(&self, chunks: &[SharedChunk], memory: SharedMemory) -> Result<u64, Errno> {
            let chunks: Vec<(u64, u64)> = chunks.iter().map(|c| (c.phys_base, c.pages)).collect();
            self.space
                .with(|live| live.map_shared_chunks(&chunks, memory))
                .map_err(live_errno)
        }
        fn unmap_region(&self, base: u64, len: usize) -> Result<(), Errno> {
            self.space
                .with(|live| live.unmap_shared(base, len))
                .map_err(live_errno)
        }
        fn free_region(&self, chunks: &[SharedChunk], _memory: SharedMemory) {
            for chunk in chunks {
                self.log.push(Event::Freed(chunk.phys_base));
            }
        }
    }

    #[derive(Default)]
    struct Kills(Mutex<Vec<ProcessId>>);

    impl ProcessSignal for Kills {
        fn resolve_child(&self, _sender: ProcessId, _pid: i64) -> Result<ProcessId, Errno> {
            Err(Errno::NotFound)
        }
        fn signal_task(&self, target: ProcessId, signal: Signal) -> Result<(), Errno> {
            assert_eq!(signal, Signal::Kill);
            self.0.lock().unwrap().push(target);
            Ok(())
        }
    }

    struct Fixture {
        aspaces: RwLock<AddressSpaceRegistry>,
        irq: IrqTable,
        log: Log,
        kills: Kills,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                aspaces: RwLock::new(AddressSpaceRegistry::new()),
                irq: IrqTable::new(63),
                log: Log::default(),
                kills: Kills::default(),
            }
        }

        fn revoker<'a>(&'a self, shared: &'a dyn SharedMemFacility) -> Revoker<'a> {
            Revoker {
                aspaces: &self.aspaces,
                irq: &self.irq,
                shared,
                shootdown: Some(&self.log),
                signal: &self.kills,
            }
        }

        /// Give `task` a live space, recorded for revocation and with a
        /// registered snapshot frozen from it.
        fn with_space(
            &self,
            task: ProcessId,
            live: Box<dyn LiveUserSpace + Send>,
        ) -> Arc<ProcessSpace> {
            let space = Arc::new(ProcessSpace::for_test(live));
            self.register(task, &space);
            space
        }

        fn register(&self, task: ProcessId, space: &Arc<ProcessSpace>) {
            let physmap: Box<dyn PhysMap + Send + Sync> =
                Box::new(SimPhysMap::new(PhysAddr::new(0), PAGE_SIZE));
            let frozen = Box::new(space.with(|live| live.freeze()));
            let mut aspaces = self.aspaces.write();
            aspaces.register(task, frozen, physmap).expect("registers");
            aspaces.set_live_space(task, space);
        }

        fn refreeze(&self, task: ProcessId, space: &ProcessSpace) {
            let frozen = Box::new(space.with(|live| live.freeze()));
            self.aspaces.write().reregister_space(task, frozen);
        }

        fn snapshot_maps(&self, task: ProcessId, va: u64) -> bool {
            let page = Page::from_addr(VirtAddr::new(va & !(PAGE_SIZE as u64 - 1))).unwrap();
            let aspaces = self.aspaces.read();
            let (space, _) = aspaces.resolve(task).expect("registered");
            space.translate(page).is_some()
        }
    }

    fn translates(space: &ProcessSpace, va: u64) -> bool {
        let page = Page::from_addr(VirtAddr::new(va & !(PAGE_SIZE as u64 - 1))).unwrap();
        space.with(|live| live.translate_page(page)).is_some()
    }

    fn task(id: u64) -> ProcessId {
        ProcessId(id)
    }

    #[test]
    fn every_grant_a_removed_node_conferred_is_revoked_and_no_other() {
        let fx = Fixture::new();
        let shared = crate::devres::NULL_SHARED_MEM_FACILITY;
        let (driver, delegate, other) = (task(0x7_0001), task(0x7_0002), task(0x7_0003));
        let _delegate_space = fx.with_space(delegate, crate::procspace::host_test_space!());
        let kinds = [
            HwResource::mmio(0xFE00_0000, 0x1000),
            HwResource::irq(40, 1),
            HwResource::port(0x70, 2),
            HwResource::dma(0x3FFF_FFFF, 0x1000),
            HwResource::endpoint(0xCA11_0007),
            HwResource::shared(0x77),
        ];
        let unrelated = HwResource::mmio(0xFE10_0000, 0x1000);
        {
            let mut aspaces = fx.aspaces.write();
            for resource in kinds {
                aspaces.mint_node_grant(driver, resource, 7);
            }
            aspaces
                .delegate_grant(driver, delegate, HwResource::shared(0x77))
                .expect("delegates");
            aspaces.mint_node_grant(other, unrelated, 8);
        }

        let revoked = fx.revoker(&shared).revoke(&[7]);
        assert_eq!(
            revoked,
            Revoked {
                grants: 7,
                holders: 2,
                killed: 0
            }
        );
        let aspaces = fx.aspaces.read();
        for resource in kinds {
            assert!(!aspaces.grant_covers(driver, &resource), "{resource:?}");
        }
        assert!(!aspaces.grant_covers(delegate, &HwResource::shared(0x77)));
        assert!(
            aspaces.grant_covers(other, &unrelated),
            "another node's driver keeps its grant"
        );
        assert_eq!(
            aspaces.next_revoked_holder(None),
            None,
            "nothing is left flagged"
        );
        assert!(fx.kills.0.lock().unwrap().is_empty());
    }

    #[test]
    fn a_revocation_that_revokes_nothing_visits_no_holder() {
        let fx = Fixture::new();
        let shared = crate::devres::NULL_SHARED_MEM_FACILITY;
        fx.aspaces
            .write()
            .mint_node_grant(task(0x7_0010), HwResource::irq(3, 1), 8);
        assert_eq!(fx.revoker(&shared).revoke(&[7]), Revoked::default());
        assert!(fx.log.events().is_empty());
    }

    #[test]
    fn a_revoked_window_is_unmapped_dropped_from_the_snapshot_and_shot_down() {
        let fx = Fixture::new();
        let shared = crate::devres::NULL_SHARED_MEM_FACILITY;
        let driver = task(0x7_0020);
        let space = fx.with_space(driver, crate::procspace::host_test_space!());
        let (revoked_window, kept_window) = (
            HwResource::mmio(0xFE00_0040, 0x40),
            HwResource::mmio(0xFE20_0000, 0x1000),
        );
        {
            let mut aspaces = fx.aspaces.write();
            aspaces.mint_node_grant(driver, revoked_window, 7);
            aspaces.mint_node_grant(driver, kept_window, 8);
        }
        let revoked_va = space
            .with(|live| live.map_device_window(0xFE00_0040, 0x40))
            .expect("maps");
        let kept_va = space
            .with(|live| live.map_device_window(0xFE20_0000, 0x1000))
            .expect("maps");
        fx.refreeze(driver, &space);
        assert!(fx.snapshot_maps(driver, revoked_va));

        let revoked = fx.revoker(&shared).revoke(&[7]);
        assert_eq!((revoked.holders, revoked.killed), (1, 0));
        let page = revoked_va & !(PAGE_SIZE as u64 - 1);
        assert!(!translates(&space, revoked_va), "its next access faults");
        assert!(
            !fx.snapshot_maps(driver, revoked_va),
            "no kernel copy reaches it"
        );
        assert_eq!(fx.log.events(), [Event::Shootdown(page, 1)]);
        assert!(translates(&space, kept_va), "another node's window stays");
        assert!(fx.snapshot_maps(driver, kept_va));
    }

    /// An owner's region `holder` maps under a grant from node 7.
    struct Mapped<'a> {
        owner: ProcessId,
        holder: ProcessId,
        holder_space: Arc<ProcessSpace>,
        owner_fac: IntoSpace<'a>,
        holder_fac: IntoSpace<'a>,
        owner_va: u64,
        holder_va: u64,
        region: u64,
    }

    fn map_through_node_7(fx: &Fixture) -> Mapped<'_> {
        let owner = task(crate::test_boot::claim_task());
        let holder = task(crate::test_boot::claim_peer_task());
        let owner_space = fx.with_space(owner, crate::procspace::host_test_space!());
        let holder_space = fx.with_space(holder, crate::procspace::host_test_space!());
        let (owner_fac, holder_fac) = (
            IntoSpace::new(&owner_space, &fx.log),
            IntoSpace::new(&holder_space, &fx.log),
        );
        let (owner_va, region) = crate::sharedreg::create(&owner_fac, owner, 1).expect("created");
        let (holder_va, _) = crate::sharedreg::map(&holder_fac, holder, region).expect("mapped");
        fx.aspaces
            .write()
            .mint_node_grant(holder, HwResource::shared(region), 7);
        fx.refreeze(holder, &holder_space);
        Mapped {
            owner,
            holder,
            holder_space,
            owner_fac,
            holder_fac,
            owner_va,
            holder_va,
            region,
        }
    }

    #[test]
    fn a_region_whose_conferring_node_is_gone_stays_mapped_and_is_retired() {
        let fx = Fixture::new();
        let m = map_through_node_7(&fx);

        let revoked = fx.revoker(&m.holder_fac).revoke(&[7]);
        assert_eq!(
            revoked,
            Revoked {
                grants: 1,
                holders: 1,
                killed: 0
            }
        );
        assert!(
            translates(&m.holder_space, m.holder_va),
            "the holder keeps what it mapped"
        );
        assert!(fx.snapshot_maps(m.holder, m.holder_va));
        assert!(crate::sharedreg::is_retired(m.region));
        assert_eq!(
            crate::sharedreg::map(&m.holder_fac, m.holder, m.region).err(),
            Some(Errno::PermissionDenied),
            "nothing new reaches it"
        );
        assert!(fx.log.events().is_empty());
        assert!(fx.kills.0.lock().unwrap().is_empty());

        drop(
            crate::sharedreg::unmap(&m.holder_fac, m.holder, m.holder_va).expect("holder lets go"),
        );
        drop(crate::sharedreg::unmap(&m.owner_fac, m.owner, m.owner_va).expect("owner lets go"));
        assert_eq!(fx.log.events(), [Event::Freed(0x2000_0000)]);
    }

    #[test]
    fn a_region_a_live_node_still_confers_is_withdrawn_before_its_frames_are_freed() {
        let fx = Fixture::new();
        let m = map_through_node_7(&fx);
        let parent_driver = task(0x7_0030);
        fx.aspaces
            .write()
            .mint_node_grant(parent_driver, HwResource::shared(m.region), 8);
        drop(crate::sharedreg::unmap(&m.owner_fac, m.owner, m.owner_va).expect("owner lets go"));
        assert!(
            fx.log.events().is_empty(),
            "the holder's reference keeps it"
        );

        let revoked = fx.revoker(&m.holder_fac).revoke(&[7]);
        assert_eq!(
            revoked,
            Revoked {
                grants: 1,
                holders: 1,
                killed: 1
            }
        );
        assert_eq!(crate::sharedreg::mapping_of(m.holder, m.region), None);
        assert!(!translates(&m.holder_space, m.holder_va));
        assert!(!fx.snapshot_maps(m.holder, m.holder_va));
        assert_eq!(
            fx.log.events(),
            [Event::Shootdown(m.holder_va, 1), Event::Freed(0x2000_0000)],
            "every CPU stops translating the page before its frame is freed"
        );
        assert_eq!(
            *fx.kills.0.lock().unwrap(),
            [m.holder],
            "its pointers into the window could alias whatever is mapped there next"
        );
        assert!(
            !crate::sharedreg::is_retired(m.region),
            "the live node's session goes on"
        );
    }

    #[test]
    fn a_region_a_lasting_grant_still_covers_stays_mapped() {
        let fx = Fixture::new();
        let m = map_through_node_7(&fx);
        {
            let mut aspaces = fx.aspaces.write();
            aspaces.mint_node_grant(task(0x7_0031), HwResource::shared(m.region), 8);
            aspaces.revoke_node_grants(&[7]);
            aspaces.mint_grant(m.holder, HwResource::shared(m.region));
        }

        assert_eq!(
            fx.revoker(&m.holder_fac)
                .tear_down(m.holder, Some(&m.holder_space)),
            Ok(true)
        );
        assert_eq!(
            crate::sharedreg::mapping_of(m.holder, m.region),
            Some(m.holder_va)
        );
        assert!(translates(&m.holder_space, m.holder_va));
        assert!(fx.log.events().is_empty());

        drop(
            crate::sharedreg::unmap(&m.holder_fac, m.holder, m.holder_va).expect("holder lets go"),
        );
        drop(crate::sharedreg::unmap(&m.owner_fac, m.owner, m.owner_va).expect("owner lets go"));
    }

    #[test]
    fn a_holder_whose_space_is_gone_or_replaced_is_never_signalled() {
        let fx = Fixture::new();
        let shared = crate::devres::NULL_SHARED_MEM_FACILITY;
        let holder = task(0x7_0032);
        let torn_down = fx.with_space(holder, crate::procspace::host_test_space!());
        let revoker = fx.revoker(&shared);
        assert!(!revoker.kill(holder, None), "no space, nothing reachable");
        fx.aspaces.write().withdraw(holder);
        let _successor = fx.with_space(holder, crate::procspace::host_test_space!());
        assert!(
            !revoker.kill(holder, Some(&torn_down)),
            "its id now names another task"
        );
        assert!(fx.kills.0.lock().unwrap().is_empty());
    }

    #[test]
    fn a_revoked_line_is_released_and_a_parked_wait_finds_it_gone() {
        let fx = Fixture::new();
        let shared = crate::devres::NULL_SHARED_MEM_FACILITY;
        let driver = task(0x7_0040);
        {
            let mut aspaces = fx.aspaces.write();
            aspaces.mint_node_grant(driver, HwResource::irq(40, 1), 7);
            aspaces.mint_node_grant(driver, HwResource::irq(41, 1), 8);
        }
        let revoked_line = fx.irq.bind(40, driver).expect("binds").handle;
        let kept_line = fx.irq.bind(41, driver).expect("binds").handle;

        assert_eq!(fx.revoker(&shared).revoke(&[7]).holders, 1);
        assert_eq!(
            fx.irq.try_wait_step(revoked_line, driver, 0, 1_000),
            WaitStep::NotFound,
            "what a waiter woken by the release reads"
        );
        assert_eq!(
            fx.irq.try_wait_step(kept_line, driver, 0, 1_000),
            WaitStep::Continue,
            "another node's line stays bound"
        );
        assert!(fx.irq.bind(40, task(0x7_0041)).is_ok(), "the line is free");
    }

    #[test]
    fn a_binding_made_as_its_grant_is_revoked_is_undone() {
        let fx = Fixture::new();
        let shared = crate::devres::NULL_SHARED_MEM_FACILITY;
        let driver = task(0x7_0042);
        fx.aspaces
            .write()
            .mint_node_grant(driver, HwResource::irq(42, 1), 7);
        let kept = fx.irq.bind(42, driver).expect("binds").handle;
        assert_eq!(fx.revoker(&shared).keep_binding(driver, 42, kept), Ok(kept));

        fx.aspaces.write().revoke_node_grants(&[7]);
        assert_eq!(
            fx.revoker(&shared).keep_binding(driver, 42, kept),
            Err(Errno::PermissionDenied)
        );
        assert!(fx.irq.bind(42, task(0x7_0043)).is_ok(), "the line is free");
    }

    #[test]
    fn a_holder_whose_windows_cannot_be_torn_down_is_killed() {
        let fx = Fixture::new();
        let shared = crate::devres::NULL_SHARED_MEM_FACILITY;
        let driver = task(0x7_0050);
        // Page tables that no longer agree with the space's window record.
        let wedged = crate::test_live::FakeLive {
            next: Some(LiveSpaceError::Mmio(MmioError::InvalidRegion)),
            ..crate::test_live::FakeLive::default()
        };
        let _space = fx.with_space(driver, Box::new(wedged));
        fx.aspaces
            .write()
            .mint_node_grant(driver, HwResource::mmio(0xFE00_0000, 0x1000), 7);

        let revoked = fx.revoker(&shared).revoke(&[7]);
        assert_eq!(
            revoked,
            Revoked {
                grants: 1,
                holders: 1,
                killed: 1
            }
        );
        assert_eq!(*fx.kills.0.lock().unwrap(), [driver]);
        assert_eq!(
            fx.aspaces.read().next_revoked_holder(None),
            None,
            "its revoked grants are retired all the same"
        );
    }
}
