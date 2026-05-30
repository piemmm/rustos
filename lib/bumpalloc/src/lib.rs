//! Forward-only bump allocator for freestanding boot heaps.
//!
//! # Why a bump allocator
//!
//! The bump allocator is the smallest allocator that supports the
//! `Arc`/`BTreeMap`/`Vec` traffic the `kernel/core` init sequence
//! produces during boot (the boot memory map, the identity-table
//! verifier's audit record, the scheduler's per-priority run-queues).
//! It satisfies `AGENTS.md` §15.1 — *do not be lazy* — for the boot
//! milestone without inventing a production heap; the real per-process
//! heaps land in a later kernel/mem sub-stage.
//!
//! # Why a shared crate
//!
//! Every freestanding RustOS boot binary (the production
//! `rustos-kernel`, every `tests/integration/*` QEMU bin, and every
//! architecture port's boot harness) needs exactly this allocator. It
//! lives in `lib/` so the type is defined once and re-used everywhere
//! (`AGENTS.md` §2.2 — no duplication, §6 — shared code lives in
//! `lib/`). The `rustos-kernel` crate re-exports it from its
//! `bumpalloc` module so existing call sites keep their import paths.
//!
//! # Documented limits
//!
//! * **Never frees.** `dealloc` is a deliberate no-op. The bump
//!   allocator is the only allocator the boot path has, and it never
//!   has to reclaim — `kernel/core::kernel_main` halts the CPU when the
//!   sequence completes (the trailing `KernelArch::halt`). A real
//!   slab/per-process heap lands in the kernel/mem follow-up commit
//!   that activates the scheduler dispatch loop.
//! * **Hard upper bound.** The backing storage is a single
//!   `Heap<HEAP_BYTES>` static. Exhaustion returns a null pointer per
//!   the `GlobalAlloc` contract; the caller (a `try_reserve`-style
//!   path or a `panic = "abort"` traceback) reports the failure.
//! * **Thread-safe.** The cursor is an `AtomicUsize`; concurrent
//!   allocators on different CPUs make progress without lock
//!   contention. Allocation is a CAS loop with `AcqRel`/`Relaxed`.
//! * **No `#[global_allocator]` declaration here.** The static lives
//!   in each binary so a `cargo build --workspace` can never link two
//!   `#[global_allocator]`-bearing crates together; each bin declares
//!   its own.
//!
//! `AGENTS.md` §4 forbids "an `unsafe` global allocator that performs
//! raw pointer arithmetic without bounds-checked wrappers" — every
//! pointer produced by [`BumpAllocator::alloc`] is range-checked
//! against `HEAP_BYTES` *before* it is returned to the caller.

#![no_std]
#![deny(missing_docs)]

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

/// Bump-allocator heap size.
///
/// 64 MiB is sized for the boot pipeline. The dominant consumer is
/// `kernel/mem::FrameAllocator::new`, which allocates a bitmap of
/// `total_frames / 64` `u64`s where `total_frames` is bounded by
/// `BootMemoryMap::highest_address() / PAGE_SIZE`. On QEMU with OVMF,
/// the EFI memory map's highest address reaches into the 32-bit
/// MMIO window (LAPIC at `0xFEE0_0000`, IO-APIC at `0xFEC0_0000`,
/// firmware-reserved ACPI tables in the high 32-bit half), so
/// `total_frames` ≈ 1 Mi and the bitmap is ≈ 128 KiB. The headroom
/// above that pays for the `Vec<MemoryRegion>` clone, the per-order
/// `BTreeSet` free-lists in the buddy allocator, the per-CPU scheduler
/// bookkeeping for the BSP, the `Arc<Arch>`, and the audit-event Sink
/// formatting allocations emitted during the `mem` / `sec` / `sched` /
/// `ipc` phases. Production per-process heaps land in a later stage;
/// this value is the documented temporary ceiling.
pub const HEAP_BYTES: usize = 64 * 1024 * 1024;

/// 4 KiB-aligned heap storage handed out by [`BumpAllocator`].
///
/// Aligned to a page so we never satisfy a page-aligned request out of
/// a half-aligned tail.
#[repr(C, align(4096))]
pub struct Heap([u8; HEAP_BYTES]);

impl Heap {
    /// Zero-initialised heap. `const` so the binary's `static mut`
    /// arena is constructed in `.bss`, *not* on the stack — clippy's
    /// `large_stack_arrays` lint is a false positive here because no
    /// `Heap` value ever materialises as a local variable; every
    /// consumer assigns `Heap::ZERO` directly to a `static mut HEAP`
    /// (the bump-allocator backing arena) where the bytes live in
    /// `.bss`. `AGENTS.md` §15.10 — every `#[allow]` is paired with a
    /// justifying comment.
    #[allow(clippy::large_stack_arrays)]
    pub const ZERO: Self = Self([0; HEAP_BYTES]);
}

/// Forward-only bump allocator.
///
/// Implements [`GlobalAlloc`]. Pair with a `static HEAP: Heap`
/// (`#[repr(C, align(4096))]`) in the bin crate, then register it via
/// `#[global_allocator]`.
///
/// The `heap_base: *mut u8` field carries the address of the bin
/// crate's per-binary heap so different bins can host independent
/// arenas without colliding through a single `static mut`. The pointer
/// must remain valid for the lifetime of the binary; in practice the
/// bin crates back it with a `static mut Heap` per
/// `AGENTS.md` §2 (*"No global mutable static beyond the per-CPU
/// bootstrap area"* — the boot heap is documented in that crate's
/// `README.md` as the single exception, justified by the lack of any
/// other allocator on the boot path).
pub struct BumpAllocator {
    /// Pointer to the first byte of the backing heap. Set once before
    /// the first allocation; never mutated.
    heap_base: *mut u8,
    /// Total bytes available at `heap_base`. Cap on `cursor`.
    heap_len: usize,
    /// Allocation cursor (in bytes). CAS-advanced on every successful
    /// allocation.
    cursor: AtomicUsize,
}

// SAFETY: `BumpAllocator` only mutates `cursor` (an `AtomicUsize`,
// inherently `Sync`) and reads `heap_base` / `heap_len` (immutable
// after construction). The underlying heap bytes are never aliased
// because each successful CAS hands out a disjoint slice.
unsafe impl Sync for BumpAllocator {}
unsafe impl Send for BumpAllocator {}

impl BumpAllocator {
    /// Construct a new allocator over an existing heap region.
    ///
    /// # Safety
    ///
    /// * `heap_base` must point at the first byte of a
    ///   `heap_len`-byte writable region that lives for the entire
    ///   lifetime of the binary.
    /// * The same region must not be exposed through any other
    ///   `GlobalAlloc` implementation (the bin crate's
    ///   `#[global_allocator]` is its only entry point).
    /// * `heap_base` must be page-aligned (the [`Heap`] type provides
    ///   that automatically) so the first satisfied
    ///   alignment-up-to-PAGE_SIZE allocation does not waste a frame.
    #[must_use]
    pub const unsafe fn new(heap_base: *mut u8, heap_len: usize) -> Self {
        Self {
            heap_base,
            heap_len,
            cursor: AtomicUsize::new(0),
        }
    }

    /// Bytes currently handed out. Diagnostic only.
    #[must_use]
    pub fn used(&self) -> usize {
        self.cursor.load(Ordering::Acquire)
    }

    /// Bytes still available. Diagnostic only.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.heap_len.saturating_sub(self.used())
    }
}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let align = layout.align();
        let size = layout.size();
        // `Layout::align` is always a non-zero power of two per the
        // `Layout` contract, so `align - 1` is the canonical mask.
        let mask = align - 1;

        let mut cur = self.cursor.load(Ordering::Relaxed);
        loop {
            // Align the cursor up to the requested alignment.
            let Some(aligned_raw) = cur.checked_add(mask) else {
                return core::ptr::null_mut();
            };
            let aligned = aligned_raw & !mask;
            // Reserve `size` bytes starting at the aligned cursor.
            let Some(next) = aligned.checked_add(size) else {
                return core::ptr::null_mut();
            };
            // Range-check against the heap cap. AGENTS.md §4 — every
            // returned pointer is bounds-checked before being handed
            // out.
            if next > self.heap_len {
                return core::ptr::null_mut();
            }
            match self
                .cursor
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    // SAFETY: `aligned < self.heap_len` by the bound
                    // check above; `self.heap_base` is the first byte
                    // of a `self.heap_len`-byte region per the
                    // `BumpAllocator::new` safety contract.
                    return unsafe { self.heap_base.add(aligned) };
                }
                Err(observed) => cur = observed,
            }
        }
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Deliberate no-op. The bump allocator does not reclaim; see
        // the module-level limits documentation.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::alloc::Layout;

    // Page-aligned backing buffer for tests. `BumpAllocator::new`'s
    // safety contract requires a page-aligned base; an unaligned
    // stack-local `[u8; N]` would silently break the alignment checks
    // below.
    #[repr(C, align(4096))]
    struct Backing<const N: usize>([u8; N]);

    fn fixture<const N: usize>(storage: &mut Backing<N>) -> BumpAllocator {
        // SAFETY: `storage` outlives the allocator (borrowed for the
        // test's lifetime), is exclusively owned by the local
        // variable, and is page-aligned via the `Backing` newtype as
        // required by `BumpAllocator::new`.
        unsafe { BumpAllocator::new(storage.0.as_mut_ptr(), N) }
    }

    #[test]
    fn alloc_hands_out_aligned_disjoint_blocks() {
        let mut backing = Backing([0u8; 1024]);
        let alloc = fixture(&mut backing);

        let layout = Layout::from_size_align(16, 16).unwrap();
        // SAFETY: layout is non-zero and the allocator is fresh.
        let a = unsafe { alloc.alloc(layout) };
        let b = unsafe { alloc.alloc(layout) };
        assert!(!a.is_null());
        assert!(!b.is_null());
        assert_eq!((a as usize) % 16, 0);
        assert_eq!((b as usize) % 16, 0);
        assert!(b as usize >= a as usize + 16);
    }

    #[test]
    fn alloc_returns_null_when_heap_exhausted() {
        let mut backing = Backing([0u8; 64]);
        let alloc = fixture(&mut backing);

        let layout = Layout::from_size_align(32, 8).unwrap();
        // SAFETY: layout is non-zero.
        let a = unsafe { alloc.alloc(layout) };
        let b = unsafe { alloc.alloc(layout) };
        let c = unsafe { alloc.alloc(layout) }; // exhausts heap
        assert!(!a.is_null());
        assert!(!b.is_null());
        assert!(
            c.is_null(),
            "third 32-byte alloc must fail on a 64-byte heap"
        );
    }

    #[test]
    fn alloc_respects_layout_alignment_above_pointer_width() {
        let mut backing = Backing([0u8; 4096]);
        let alloc = fixture(&mut backing);

        // Burn one byte so the aligned-up cursor is genuinely advanced.
        let l1 = Layout::from_size_align(1, 1).unwrap();
        // SAFETY: layout is non-zero.
        let _ = unsafe { alloc.alloc(l1) };

        let big = Layout::from_size_align(64, 256).unwrap();
        // SAFETY: layout is non-zero.
        let p = unsafe { alloc.alloc(big) };
        assert!(!p.is_null());
        assert_eq!(
            (p as usize) % 256,
            0,
            "alloc must honour 256-byte alignment"
        );
    }

    #[test]
    fn used_and_remaining_report_cursor_position() {
        let mut backing = Backing([0u8; 128]);
        let alloc = fixture(&mut backing);
        assert_eq!(alloc.used(), 0);
        assert_eq!(alloc.remaining(), 128);

        let layout = Layout::from_size_align(16, 16).unwrap();
        // SAFETY: layout is non-zero.
        let _ = unsafe { alloc.alloc(layout) };
        assert_eq!(alloc.used(), 16);
        assert_eq!(alloc.remaining(), 112);
    }

    #[test]
    fn dealloc_is_a_noop() {
        let mut backing = Backing([0u8; 64]);
        let alloc = fixture(&mut backing);
        let layout = Layout::from_size_align(16, 16).unwrap();
        // SAFETY: layout is non-zero.
        let a = unsafe { alloc.alloc(layout) };
        assert!(!a.is_null());
        let used_before = alloc.used();
        // SAFETY: `a` was just produced by the same allocator with
        // `layout`.
        unsafe { alloc.dealloc(a, layout) };
        // Cursor is unchanged because dealloc is a no-op.
        assert_eq!(alloc.used(), used_before);
    }
}
