//! Window-backed kthread kernel stacks with a hardware guard page.
//!
//! A kthread's kernel stack is a run of pages in the port's shared kernel
//! remap window, laid out low-to-high as `[guard | usable]`. The guard slot
//! is reserved but **never mapped**, so an overrun off the bottom of the
//! usable region takes a translation fault instead of corrupting the
//! lower-addressed neighbour.
//!
//! # Why the window rather than the identity map
//!
//! A guard page inside a coarse identity block can only be made to fault by
//! refining that block to 4 KiB leaves — a granule change on a root that is
//! already the active translation regime, which is a break-before-make
//! violation the architecture leaves undefined, and which has to be repeated
//! in every root a task might run under (`plans/OPEN-DEFECTS.md` D82). The
//! window's sub-hierarchy is installed by every root, so a slot left unmapped
//! here is absent in all of them at once, refines nothing, and needs no
//! maintenance.
//!
//! # Sizing
//!
//! The window is shared with the growable kernel heap. Both tiers are
//! frame-backed, so neither can consume more address space than the machine
//! has RAM; [`install_kernel_stacks`] therefore gives the stack tier as much
//! of the window as discovered RAM could ever back, capped at half the window
//! so the heap keeps a guaranteed share. Below that cap the tier cannot
//! exhaust address space before the frame allocator is genuinely out of
//! memory.

use alloc::boxed::Box;
use core::ptr::NonNull;

use tairix_arch_api::{KernelStackRegion, STACK_ALIGN};
use tairix_kernel_mem::{
    back_run, release_run, FrameAllocator, KernelVirtMap, PhysMap, SlotWindow, PAGE_SIZE,
};
use tairix_sync::{Once, SpinLock};

use crate::kthread::{KernelStack, KTHREAD_STACK_BYTES};

/// Mapped pages one stack occupies, above its guard slot.
const STACK_PAGES: usize = KTHREAD_STACK_BYTES / PAGE_SIZE;

/// Window slots one stack reserves: the unmapped guard plus the usable run.
const RESERVE_PAGES: usize = STACK_PAGES + 1;

/// The usable stack is a whole number of pages, so the guard slot below it
/// lands on a page boundary and no mapped page of one stack ever abuts the
/// mapped page of another. A page-aligned run over a page-aligned window
/// also makes the usable top `STACK_ALIGN`-aligned outright, so `prepare`
/// can never refuse one of these stacks as misaligned.
const _STACK_PAGE_ALIGNED: () = {
    assert!(KTHREAD_STACK_BYTES.is_multiple_of(PAGE_SIZE));
    assert!(STACK_PAGES > 0);
    assert!(PAGE_SIZE.is_multiple_of(STACK_ALIGN));
};

/// The installed stack tier. Set once from the boot path; absent on a port
/// with no remap window, where the software-canary fallback stands in.
static STACKS: Once<&'static StackTier> = Once::new();

/// The window-backed kthread kernel-stack tier.
struct StackTier {
    frames: &'static FrameAllocator,
    kvmap: &'static dyn KernelVirtMap,
    /// First window page index this tier may draw, so its slots and the
    /// heap's name disjoint address space.
    base_slot: usize,
    /// Which runs of the tier are handed out.
    ///
    /// A plain lock: the tier is reached only from thread admission and from
    /// the drop of an admitted task's control block, both of which run in
    /// task or dispatcher context. No interrupt handler creates or destroys a
    /// kthread stack, so no handler can reenter this. The remap map's own
    /// lock does mask, because the heap shares it and an ISR may allocate.
    slots: SpinLock<SlotWindow>,
}

impl StackTier {
    /// Reserve and back one stack, returning the window address of its guard
    /// slot and a pointer to the usable run above it.
    ///
    /// Nothing is left reserved or mapped when the frame pool or the port
    /// refuses part of the run.
    fn alloc(&self) -> Option<(u64, NonNull<u8>)> {
        let window = self.kvmap.window();
        let mut slots = self.slots.lock();
        let slot = slots.allocate(RESERVE_PAGES).ok()?;
        // The usable run starts one slot above the guard, and the window
        // derives its pointer from the root the port established, so no
        // address here is rebuilt into a pointer.
        let Some(base) = window.page_ptr(self.base_slot + slot + 1) else {
            let _ = slots.release(slot, RESERVE_PAGES);
            return None;
        };
        let usable = base.addr().get() as u64;
        let guard = usable - PAGE_SIZE as u64;
        if !back_run(self.kvmap, self.frames, usable, STACK_PAGES) {
            release_run(self.kvmap, self.frames, usable, STACK_PAGES);
            let _ = slots.release(slot, RESERVE_PAGES);
            return None;
        }
        Some((guard, base))
    }

    /// Zero, unmap, and release the stack whose guard slot is at `guard`
    /// and whose usable run starts at `base`.
    fn free(&self, guard: u64, base: NonNull<u8>) {
        let Some(index) = self.kvmap.window().page_index(guard) else {
            // Not an address this tier ever handed out: fail closed rather
            // than tear down a run belonging to something else.
            return;
        };
        let Some(slot) = index.checked_sub(self.base_slot) else {
            return;
        };
        let usable = guard + PAGE_SIZE as u64;
        let mut slots = self.slots.lock();
        // Release first: it accepts only an exact live run, so a stale guard
        // address is refused before anything is scrubbed or unmapped. The
        // lock is held across the teardown, so the released address space
        // cannot be re-handed out while its pages are still mapped.
        if slots.release(slot, RESERVE_PAGES).is_err() {
            return;
        }
        // A kernel stack can hold spilled capability tokens, so the frames
        // are scrubbed while they are still reachable, before they rejoin
        // the pool for any use.
        //
        // SAFETY: the run is this tier's own live mapping — `release`
        // accepted it, so no other owner holds it — and every page of it was
        // installed by `back_run` above and is writable kernel memory until
        // the teardown below removes it.
        unsafe {
            let bytes = core::slice::from_raw_parts_mut(base.as_ptr(), KTHREAD_STACK_BYTES);
            tairix_pagezero::zero(bytes);
        }
        release_run(self.kvmap, self.frames, usable, STACK_PAGES);
    }
}

/// A kthread kernel stack backed by a run of window pages.
///
/// Owns its run for as long as it lives; [`Drop`] scrubs and returns it, so
/// the tier's footprint tracks the live thread set rather than the spawn
/// history.
struct WindowStack {
    tier: &'static StackTier,
    /// Window address of the unmapped guard slot: the run's low edge, one
    /// page below the usable stack.
    guard: u64,
    /// The usable run's low edge, as the pointer the tier formed once when
    /// it handed the run out.
    base: NonNull<u8>,
}

// SAFETY: the run is owned exclusively by this value — the slots leave the
// tier's free list when they are handed out and rejoin it only from the
// `Drop` below — so moving it to another CPU moves the sole owner. The
// pages are mapped in the window's shared sub-hierarchy, which every
// translation root installs, so the pointer is valid under whichever root
// the task runs.
unsafe impl Send for WindowStack {}

impl Drop for WindowStack {
    fn drop(&mut self) {
        self.tier.free(self.guard, self.base);
    }
}

// SAFETY: `region` names the run's usable extent. Every page of it was
// installed by `back_run` in the window's shared sub-hierarchy, which every
// translation root maps, so it stays readable and writable under whichever
// root the task runs — and it is exclusive to this value, because the slots
// leave the tier's free list when they are handed out and rejoin it only
// from the `Drop` above. The guard slot below `usable` is deliberately never
// mapped, so an overrun faults.
unsafe impl KernelStack for WindowStack {
    fn region(&self) -> KernelStackRegion {
        // SAFETY: `base` addresses the `KTHREAD_STACK_BYTES` this run has
        // mapped, held until the `Drop` above returns them.
        unsafe { KernelStackRegion::new(self.base, KTHREAD_STACK_BYTES) }
    }
    // `check_guard` keeps the default: the guard slot is unmapped in every
    // root, so an overrun faults in hardware and there is no canary to read.
}

/// Wire the window-backed kthread stack tier over the upper part of the
/// port's kernel remap window, and return the page count the heap keeps.
///
/// The split is a policy over discovered geometry: every stack page is
/// frame-backed, so the tier can never usefully hold more pages than the
/// machine has usable RAM, and it is capped at half the window so the heap
/// keeps a guaranteed share on a machine whose RAM rivals the window. Below
/// that cap the tier's address space cannot bind before RAM does, so its
/// fail-closed path is a genuine out-of-memory rather than an invented
/// ceiling.
///
/// Called once from the boot path, alongside the heap source, after the frame
/// allocator, the direct physical map, and the window all exist. A window too
/// small to hold one stack, or bookkeeping that cannot be sized, leaves no
/// tier installed and every kthread on the software-canary fallback — fail
/// closed, never a panic.
pub fn install_kernel_stacks(
    frames: &'static FrameAllocator,
    physmap: &'static (dyn PhysMap + Sync),
    kvmap: &'static dyn KernelVirtMap,
) -> usize {
    let window_pages = kvmap.window().pages();
    let stack_pages = stack_window_pages(window_pages, frames.usable_frames());
    if stack_pages < RESERVE_PAGES {
        return window_pages;
    }
    let heap_pages = window_pages - stack_pages;
    let Ok(slots) = SlotWindow::new(stack_pages, frames, physmap) else {
        return window_pages;
    };
    let tier: &'static StackTier = Box::leak(Box::new(StackTier {
        frames,
        kvmap,
        base_slot: heap_pages,
        slots: SpinLock::new(slots),
    }));
    let _ = STACKS.call_once_infallible(|| tier);
    heap_pages
}

/// Pages of a `window_pages` window the stack tier takes on a machine with
/// `usable_frames` frames of RAM. See [`install_kernel_stacks`].
fn stack_window_pages(window_pages: usize, usable_frames: usize) -> usize {
    core::cmp::min(usable_frames, window_pages / 2)
}

/// Allocate a kthread kernel stack with a hardware guard page, falling back
/// to the software-canary [`crate::kthread::BoxStack`] where no tier is
/// installed or the window and frame pool cannot supply a run.
///
/// Never hands back an unguarded stack: the fallback carries a poison canary
/// the dispatcher checks on every switch-back. `None` when neither tier can
/// supply one, so the caller refuses the spawn rather than the allocator
/// aborting the kernel.
#[must_use]
pub fn alloc_kernel_stack() -> Option<Box<dyn KernelStack + Send>> {
    if let Ok(Some(tier)) = STACKS.get() {
        if let Some((guard, base)) = tier.alloc() {
            return Some(Box::new(WindowStack { tier, guard, base }));
        }
    }
    Some(Box::new(crate::kthread::BoxStack::new()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stack_tier_never_takes_more_than_half_the_window() {
        // A machine whose RAM rivals the window must still leave the heap a
        // guaranteed share rather than being crowded out by address space
        // stacks could never back.
        assert_eq!(stack_window_pages(1024, usize::MAX), 512);
        assert_eq!(stack_window_pages(1024, 4096), 512);
    }

    #[test]
    fn the_stack_tier_is_sized_from_ram_below_the_cap() {
        // Every stack page is frame-backed, so RAM is the honest bound and
        // the tier cannot run out of address space before it runs out of
        // memory.
        assert_eq!(stack_window_pages(1024, 100), 100);
        assert_eq!(stack_window_pages(1024, 0), 0);
    }

    #[test]
    fn the_guard_slot_is_the_page_below_the_usable_base() {
        // The verticals — and any overrun diagnosis — locate a stack's guard
        // from its public geometry alone, so `top - usable_bytes` must land
        // exactly on the usable base with the guard one page below it. The
        // A page-aligned run needs no `STACK_ALIGN` round-down to shift it,
        // which is what the const-assert above pins.
        let guard = 0x8000_0000u64;
        let top = guard + PAGE_SIZE as u64 + KTHREAD_STACK_BYTES as u64;
        assert!(top.is_multiple_of(STACK_ALIGN as u64));
        let usable_base = top - KTHREAD_STACK_BYTES as u64;
        assert_eq!(usable_base, guard + PAGE_SIZE as u64);
        assert_eq!(usable_base - PAGE_SIZE as u64, guard);
    }

    #[test]
    fn an_odd_window_leaves_the_remainder_to_the_heap() {
        // The heap takes what the stack tier does not, so the two shares
        // always sum to the window and neither overlaps the other.
        let window = 1025;
        let stacks = stack_window_pages(window, usize::MAX);
        assert_eq!(stacks, 512);
        assert_eq!(window - stacks, 513);
    }
}
