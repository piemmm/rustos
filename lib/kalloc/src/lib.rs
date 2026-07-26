//! Freeing kernel heap allocator for freestanding TAIRiX images.
//!
//! # Why a freeing allocator
//!
//! The kernel global heap is long-lived: the driver-store service, the
//! scheduler, and the syscall paths allocate and free for the life of the
//! system. A forward-only bump allocator (this crate's previous design)
//! never reclaimed, so any sustained allocation traffic — a file-size
//! proportional bundle read, a long-running service serving requests —
//! eventually exhausted the heap and the `#[alloc_error_handler]`
//! *panicked*, violating ("Deterministic OOM behaviour:
//! allocation failure is a `Result`, never a panic"; the `GlobalAlloc`
//! null return is that fallible signal).
//!
//! [`FreeListAllocator`] is the replacement: a coalescing first-fit
//! free-list allocator. It reclaims on [`GlobalAlloc::dealloc`] and merges
//! adjacent free blocks, so steady allocate/free traffic runs in bounded
//! memory.
//!
//! # A growable, shrinkable heap — not a fixed ceiling
//!
//! The heap is *not* capped at a hand-picked constant. It starts on a
//! small bootstrap region (a `.bss` arena that covers early boot, before a
//! physical frame allocator exists) and, once the boot path installs a
//! [`HeapSource`], **grows on demand** by drawing fresh regions from that
//! source and **shrinks** by handing whole drained regions back. This is
//! the growable-capacity discipline the charter requires of every resource
//! ceiling: a busy kernel is not wedged by a fixed 64 MiB slab, and an idle
//! one does not hold memory it has freed. The production source draws
//! physically contiguous frames from the frame allocator (reachable through
//! the kernel direct map); because that allocator is heap-independent by
//! construction, growth never re-enters this heap's own lock.
//!
//! # Design
//!
//! The free space is a singly linked list of **holes**, each hole storing
//! its own `{ size, next }` header at its first bytes and kept **sorted by
//! address** so [`GlobalAlloc::dealloc`] coalesces with its physical
//! neighbours in one pass. Allocation is first-fit with front/back
//! splitting; a split remnant smaller than a hole header is absorbed into
//! the allocation rather than leaked as an unrepresentable fragment. Each
//! grown region carries a region-header separator at its base, so a hole
//! never coalesces across a region boundary and a wholly-drained grown
//! region is always exactly one hole that can be returned intact. The list
//! head, the `used`/`capacity` counters, the region list, and the installed
//! source live behind an inline spin lock (an `AtomicBool`), so the crate
//! stays dependency-free (`core` only) and the `static`
//! `#[global_allocator]` is `const`-constructed; the bootstrap region is
//! planted lazily on the first allocation.
//!
//! # Interrupt-safe lock
//!
//! TAIRiX takes interrupts while in-kernel code runs, so an interrupt
//! service routine can fire on a CPU that is mid-allocation holding the
//! lock. Were the lock left plain, an ISR that allocated or freed would spin
//! forever on the lock its own interrupted mainline holds — a single-CPU
//! self-deadlock. The lock is therefore **interrupt-safe**: it masks the
//! current CPU's interrupts for the whole hold. The masking primitive is
//! architecture-specific, so the freestanding bin installs it once at boot
//! ([`FreeListAllocator::install_irq_control`]) before interrupts are ever
//! enabled; until then — and on the hosted test build and the interrupt-free
//! `wasm32` port — the lock does not mask, and that window is single-CPU with
//! interrupts already masked, so no ISR can reenter.
//!
//! # Deterministic OOM
//!
//! A forward-only bump allocator (this crate's original design) never
//! reclaimed, so sustained traffic exhausted the heap and the
//! `#[alloc_error_handler]` *panicked*. This allocator instead reclaims,
//! grows, and — when the source is finally exhausted — returns null from
//! [`GlobalAlloc::alloc`], the fallible signal the charter requires
//! ("allocation failure is a `Result`, never a panic").
//!
//! the charter forbids "an `unsafe` global allocator that performs raw
//! pointer arithmetic without bounds-checked wrappers": every hole address
//! is confined to a region the allocator owns (the bootstrap region, or a
//! chunk the source handed out) by construction — the initial hole of each
//! region spans exactly that region and splits only ever shrink it — and
//! every returned pointer lies within a hole that fit the request.

#![no_std]
#![deny(missing_docs)]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::mem::{align_of, size_of};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Size of the kernel heap's **bootstrap** region.
///
/// This is the `.bss` arena the [`FreeListAllocator`] starts on, sized to
/// cover the boot pipeline and steady-state kernel service traffic without
/// having to grow at all in the common case. It is **not** a ceiling: once
/// the boot path installs a [`HeapSource`], the heap grows past this on
/// demand and shrinks back, so a heavy load is never wedged by it and an
/// idle system never holds it fully. Freed allocations are always
/// reclaimed, so allocate/free traffic does not march the heap toward
/// exhaustion the way the original bump allocator did.
pub const HEAP_BYTES: usize = 64 * 1024 * 1024;

/// 4 KiB-aligned heap storage handed out by [`FreeListAllocator`].
///
/// Aligned to a page so a page-aligned request is never wasted satisfying
/// it out of a half-aligned tail.
#[repr(C, align(4096))]
pub struct Heap([u8; HEAP_BYTES]);

impl Heap {
    /// Zero-initialised heap. `const` so the binary's `static mut` arena is
    /// constructed in `.bss`, never on the stack (clippy's
    /// `large_stack_arrays` is a false positive: no `Heap` value ever
    /// materialises as a local; every consumer assigns `Heap::ZERO`
    /// directly to a `static mut HEAP`).
    #[allow(clippy::large_stack_arrays)]
    pub const ZERO: Self = Self([0; HEAP_BYTES]);
}

/// A free-region header, stored at the first bytes of every hole.
#[repr(C)]
struct Hole {
    /// Total byte size of this hole, including this header.
    size: usize,
    /// Next hole in the address-sorted free list, or `None` at the tail.
    next: Option<NonNull<Hole>>,
}

/// Minimum representable block: a block can never be smaller than a hole
/// header (a freed block must be able to host its own `{size,next}`), and
/// is always a multiple of the header alignment.
const MIN_BLOCK: usize = size_of::<Hole>();
/// Alignment every block start and size is rounded to, so a hole header
/// written at a block start is always well-aligned.
const ALIGN: usize = align_of::<Hole>();

/// Round `value` up to the next multiple of `align` (a power of two), or
/// [`None`] on overflow.
const fn align_up(value: usize, align: usize) -> Option<usize> {
    let mask = align - 1;
    match value.checked_add(mask) {
        Some(v) => Some(v & !mask),
        None => None,
    }
}

/// The block byte length an allocation of `layout` occupies: at least a
/// hole header, the requested size, rounded up to the header alignment so
/// the next block start stays aligned. Identical in [`FreeListAllocator::alloc`]
/// and [`FreeListAllocator::dealloc`] so a freed block is reinserted with
/// the exact size it was carved with.
fn block_size(layout: Layout) -> Option<usize> {
    let want = if layout.size() > MIN_BLOCK {
        layout.size()
    } else {
        MIN_BLOCK
    };
    align_up(want, ALIGN)
}

/// A source of fresh memory the heap grows into, and returns to on shrink.
///
/// The fixed bootstrap region a [`FreeListAllocator`] is constructed over
/// (`.bss` in the kernel binaries) covers early boot, before a physical
/// frame allocator exists. Once one does, the boot path installs a
/// `HeapSource` ([`FreeListAllocator::install_source`]) so the kernel heap
/// grows and shrinks on demand instead of being capped at a hand-picked
/// constant — the growable-capacity rule the charter requires of every
/// resource ceiling. The production implementation draws physically
/// contiguous frames from the frame allocator and hands back their
/// direct-map addresses; a host test uses a simple arena-backed fake.
///
/// # Contract
///
/// * [`grow`](Self::grow) returns a writable, `ALIGN`-aligned chunk of at
///   least `min_len` bytes, owned exclusively by the heap until it is
///   handed back, or `None` on genuine exhaustion (deterministic OOM — the
///   heap then returns null from `alloc`, never a panic).
/// * [`shrink`](Self::shrink) is only ever called with the exact
///   `(base, len)` pair a prior `grow` returned, once the heap has drained
///   every byte of that chunk.
///
/// The source is consulted only while the allocator holds its own lock, so
/// an implementation must **not** call back into this same heap (that would
/// re-enter the non-reentrant lock and deadlock). The production frame
/// allocator satisfies this: it is heap-independent by construction.
pub trait HeapSource: Sync {
    /// Provide a fresh chunk of at least `min_len` writable, `ALIGN`-aligned
    /// bytes, or `None` when no more memory can be given (fail closed).
    fn grow(&self, min_len: usize) -> Option<(*mut u8, usize)>;

    /// Return a chunk previously produced by [`grow`](Self::grow), given its
    /// exact base and length.
    fn shrink(&self, base: *mut u8, len: usize);
}

/// Header planted at the base of every *grown* region (never the fixed
/// bootstrap region), linking the regions the heap can hand back.
///
/// Placing it in-band at the chunk's first bytes does double duty: it keeps
/// the region list allocation-free, and it acts as a separator so a free
/// hole in one grown region is never physically adjacent to a hole in
/// another — [`insert_hole`] therefore never coalesces across a grown
/// region boundary, so a fully-drained grown region is always exactly one
/// hole and can be returned to the source intact.
#[repr(C)]
struct RegionHeader {
    /// Total byte length of the chunk (as passed to [`HeapSource::shrink`]).
    total_len: usize,
    /// Next grown region, or `None` at the tail.
    next: Option<NonNull<RegionHeader>>,
}

/// Bytes reserved at the base of a grown region for its [`RegionHeader`],
/// rounded up to [`ALIGN`] so the usable area that follows stays aligned.
const REGION_HDR: usize = {
    let raw = size_of::<RegionHeader>();
    match align_up(raw, ALIGN) {
        Some(v) => v,
        None => raw,
    }
};

/// The mutable allocator state guarded by [`FreeListAllocator::lock`].
struct Inner {
    /// `true` once the initial whole-heap hole has been planted.
    initialised: bool,
    /// Head of the address-sorted free list.
    head: Option<NonNull<Hole>>,
    /// Bytes currently handed out (diagnostic).
    used: usize,
    /// Total usable bytes across the bootstrap region and every currently
    /// mapped grown region — the denominator `remaining` reports against.
    /// Grows and shrinks with the grown regions, never a fixed ceiling.
    capacity: usize,
    /// Head of the intrusive list of grown regions (those obtained from the
    /// [`HeapSource`] and returnable to it); `None` when the heap has never
    /// grown past its bootstrap region.
    regions: Option<NonNull<RegionHeader>>,
    /// The installed growth source, or `None` before the boot path installs
    /// one (the state in which the heap is capped at its bootstrap region,
    /// exactly the old fixed-heap behaviour).
    source: Option<&'static dyn HeapSource>,
}

/// A coalescing first-fit free-list allocator over a fixed heap region.
///
/// Implements [`GlobalAlloc`]. Pair with a `static HEAP: Heap`
/// (`#[repr(C, align(4096))]`) in the bin crate and register via
/// `#[global_allocator]`. The `heap_base` pointer must stay valid for the
/// life of the binary (the bin backs it with a `static mut Heap`'s documented single global-mutable exception).
pub struct FreeListAllocator {
    heap_base: *mut u8,
    heap_len: usize,
    lock: AtomicBool,
    /// Installed per-CPU interrupt-mask hook (a `fn() -> usize` stored as a
    /// `usize`, `0` = none), read *outside* the lock to make the critical
    /// section interrupt-safe. See [`FreeListAllocator::install_irq_control`].
    irq_disable: AtomicUsize,
    /// Installed per-CPU interrupt-restore hook (a `fn(usize)` stored as a
    /// `usize`, `0` = none), paired with [`Self::irq_disable`].
    irq_restore: AtomicUsize,
    inner: UnsafeCell<Inner>,
}

// SAFETY: every access to `inner` is serialised by the `lock` spin gate
// (`alloc`/`dealloc`/`used`/`remaining` take it before touching the cell),
// so no two threads ever hold a reference to `*inner` concurrently. The
// heap bytes are only ever reachable through the serialised free list.
unsafe impl Sync for FreeListAllocator {}
unsafe impl Send for FreeListAllocator {}

impl FreeListAllocator {
    /// Construct an allocator over an existing heap region.
    ///
    /// # Safety
    ///
    /// * `heap_base` points at the first byte of a `heap_len`-byte writable
    ///   region that lives for the whole lifetime of the binary.
    /// * That region is exposed through no other [`GlobalAlloc`] (the bin's
    ///   `#[global_allocator]` is its only entry point).
    /// * `heap_base` is at least `ALIGN`-aligned (the [`Heap`] type's page
    ///   alignment satisfies this).
    #[must_use]
    pub const unsafe fn new(heap_base: *mut u8, heap_len: usize) -> Self {
        Self {
            heap_base,
            heap_len,
            lock: AtomicBool::new(false),
            irq_disable: AtomicUsize::new(0),
            irq_restore: AtomicUsize::new(0),
            inner: UnsafeCell::new(Inner {
                initialised: false,
                head: None,
                used: 0,
                capacity: 0,
                regions: None,
                source: None,
            }),
        }
    }

    /// Install the growth source the heap draws fresh memory from once one
    /// exists (the boot path calls this after building the frame
    /// allocator).
    ///
    /// Idempotent-by-policy: the boot path installs exactly one source for
    /// the life of the binary. Installing replaces any previous source; a
    /// second install is not expected and simply retargets future growth.
    /// Before a source is installed the heap is confined to its bootstrap
    /// region and returns null once that is exhausted.
    pub fn install_source(&self, source: &'static dyn HeapSource) {
        // SAFETY: serialised mutation of the state, like every other field.
        unsafe {
            self.with_inner(|inner| inner.source = Some(source));
        }
    }

    /// Install the per-CPU interrupt mask/restore hooks that make the
    /// allocator's lock **interrupt-safe**, foreclosing a single-CPU
    /// self-deadlock.
    ///
    /// TAIRiX takes interrupts while in-kernel code runs, so an interrupt
    /// service routine can fire on a CPU that is mid-allocation holding this
    /// lock; if that ISR (or anything it calls) allocates or frees, it would
    /// spin forever on the lock its own interrupted mainline holds. To
    /// foreclose it, the allocator's lock masks interrupts on the current CPU
    /// for the duration of every hold: `disable` masks them and returns an
    /// opaque token of the prior state, `restore` puts that state back.
    ///
    /// The primitives are architecture-specific (`msr daifset` on AArch64,
    /// `cli`/`pushf` on x86_64, `csrrci sstatus` on RISC-V), so the
    /// freestanding bin installs them once during boot, **before** interrupts
    /// are first enabled and before any secondary CPU is started. Until then —
    /// and on the hosted test build and the interrupt-free `wasm32` port — no
    /// hooks are installed and the lock does not mask; that window is
    /// single-CPU with interrupts already masked, so no ISR can reenter.
    ///
    /// Set once per boot; a later install simply retargets the hooks.
    pub fn install_irq_control(&self, disable: fn() -> usize, restore: fn(usize)) {
        // Publish `restore` first, then `disable` with Release, so any reader
        // that observes a non-zero `disable` (Acquire in `with_inner`) also
        // observes the matching `restore`.
        self.irq_restore.store(restore as usize, Ordering::Relaxed);
        self.irq_disable.store(disable as usize, Ordering::Release);
    }

    /// Acquire the spin lock, run `f` over the mutable state, release it.
    ///
    /// Interrupt-safe: interrupts on the current CPU are masked *before* the
    /// lock is taken and restored *after* it is released, whenever an
    /// interrupt-control hook is installed ([`Self::install_irq_control`]) —
    /// so an interrupt service routine can never fire on a CPU holding this
    /// lock and reenter the allocator, self-deadlocking on it. Before a hook
    /// is installed (early boot), on the hosted test build, and on the
    /// interrupt-free `wasm32` port, no masking is done: those contexts are
    /// single-CPU with interrupts already masked, so no ISR can reenter.
    ///
    /// # Safety
    ///
    /// The caller must not re-enter (the lock is not reentrant); the
    /// allocator only ever calls this from its own non-nested methods.
    unsafe fn with_inner<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        // Mask interrupts on this CPU for the whole hold, *before* taking the
        // lock (fail-safe: no hook installed means the context is already
        // non-reentrant, so masking is unnecessary).
        let disable = self.irq_disable.load(Ordering::Acquire);
        let irq_token = if disable == 0 {
            None
        } else {
            // SAFETY: `irq_disable` only ever holds a `fn() -> usize` pointer
            // round-tripped through `install_irq_control`.
            let disable = unsafe { core::mem::transmute::<usize, fn() -> usize>(disable) };
            Some(disable())
        };
        while self
            .lock
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            core::hint::spin_loop();
        }
        // SAFETY: the lock is held, so this is the only live reference to
        // `*inner`. `with_inner` is never re-entered.
        let inner = unsafe { &mut *self.inner.get() };
        let out = f(inner);
        self.lock.store(false, Ordering::Release);
        // Restore the prior interrupt state only *after* the lock is released,
        // so the whole critical section ran with interrupts masked.
        if let Some(token) = irq_token {
            let restore = self.irq_restore.load(Ordering::Relaxed);
            // SAFETY: a non-zero `irq_token` was produced by the installed
            // `disable`, whose paired `restore` (published before `disable`
            // with Release/Acquire) is visible here; `token` is this CPU's
            // saved state, restored exactly once.
            let restore = unsafe { core::mem::transmute::<usize, fn(usize)>(restore) };
            restore(token);
        }
        out
    }

    /// Plant the initial whole-heap hole if not yet done.
    ///
    /// # Safety
    ///
    /// Called under the lock. `heap_base`/`heap_len` satisfy [`Self::new`]'s
    /// contract, so the header write lands in the owned region.
    unsafe fn ensure_init(&self, inner: &mut Inner) {
        if inner.initialised {
            return;
        }
        inner.initialised = true;
        // The bootstrap region is the whole of `[heap_base, heap_base +
        // heap_len)`; its usable bytes are the heap's initial capacity.
        inner.capacity = self.heap_len;
        // A heap too small to host one hole header has no free space.
        if self.heap_len >= MIN_BLOCK {
            // `heap_base` is `ALIGN`-aligned (= `align_of::<Hole>()`) by
            // [`Self::new`]'s contract, so this cast cannot under-align the
            // `Hole` header; the lint cannot see that invariant.
            #[allow(clippy::cast_ptr_alignment)]
            let base = self.heap_base.cast::<Hole>();
            // SAFETY: `heap_base` is `ALIGN`-aligned and owns `heap_len`
            // bytes; `heap_len >= MIN_BLOCK` so the header fits.
            unsafe {
                base.write(Hole {
                    size: self.heap_len,
                    next: None,
                });
            }
            inner.head = NonNull::new(base);
        }
    }

    /// Bytes currently handed out (diagnostic).
    #[must_use]
    pub fn used(&self) -> usize {
        // SAFETY: serialised read of the mutable state.
        unsafe { self.with_inner(|inner| inner.used) }
    }

    /// Bytes not currently handed out (diagnostic; includes free-list
    /// fragmentation, so a single allocation of this size may still fail).
    ///
    /// Measured against the *current* capacity (bootstrap region plus every
    /// grown region), which rises and falls as the heap grows and shrinks,
    /// so this is never bounded by a fixed heap size.
    #[must_use]
    pub fn remaining(&self) -> usize {
        // SAFETY: serialised read of the mutable state.
        unsafe { self.with_inner(|inner| inner.capacity.saturating_sub(inner.used)) }
    }

    /// First-fit carve of `size` bytes at an `align`-aligned address out of
    /// the current free list, or null when no hole fits. Splits front/back
    /// remnants back onto the list; charges `used`. Does **not** grow.
    ///
    /// `boundary` is the bootstrap-region base, forwarded to
    /// [`insert_hole`] so remnant reinsertion never coalesces across it.
    ///
    /// # Safety
    ///
    /// Called under the lock with the free list well-formed.
    unsafe fn carve(inner: &mut Inner, size: usize, align: usize, boundary: usize) -> *mut u8 {
        let mut prev: Option<NonNull<Hole>> = None;
        let mut cur = inner.head;
        while let Some(node) = cur {
            // SAFETY: `node` is a live hole in the serialised list.
            let (hole_size, hole_next) = unsafe { (node.as_ref().size, node.as_ref().next) };
            let hole_addr = node.as_ptr() as usize;
            let hole_end = hole_addr + hole_size;

            // Carve exactly `size` bytes at an aligned address inside the
            // hole, leaving front/back remnants that are each either empty
            // or a representable hole (>= `MIN_BLOCK`), so every freed byte
            // stays on the list and `dealloc` frees back exactly the `size`
            // carved at the returned pointer — no leak.
            //
            // A hole is only ever `ALIGN`-aligned, so an over-aligned
            // request (`align > ALIGN`) can land the aligned start a
            // sub-`MIN_BLOCK` distance above the hole base, leaving a front
            // remnant too small to be its own free block. Were that hole
            // simply skipped, a single large but insufficiently-aligned hole
            // could starve every over-aligned request even with most of the
            // heap free. Instead the start is advanced one alignment stride
            // so the front remnant grows into a representable hole.
            if let Some(mut start) = align_up(hole_addr, align) {
                let mut front = start - hole_addr;
                if front != 0 && front < MIN_BLOCK {
                    if let Some(s) = start.checked_add(align) {
                        start = s;
                        front += align;
                    } else {
                        prev = cur;
                        cur = hole_next;
                        continue;
                    }
                }
                let fits = start.checked_add(size).is_some_and(|end| end <= hole_end);
                let front_ok = front == 0 || front >= MIN_BLOCK;
                if fits && front_ok {
                    let back = hole_end - (start + size);
                    let back_ok = back == 0 || back >= MIN_BLOCK;
                    if back_ok {
                        // SAFETY: unlink the chosen hole, then reinsert the
                        // front and back remnants (each a valid hole or
                        // skipped when empty).
                        unsafe {
                            match prev {
                                Some(p) => (*p.as_ptr()).next = hole_next,
                                None => inner.head = hole_next,
                            }
                            if front != 0 {
                                insert_hole(&mut inner.head, hole_addr, front, boundary);
                            }
                            if back != 0 {
                                insert_hole(&mut inner.head, start + size, back, boundary);
                            }
                        }
                        inner.used += size;
                        return start as *mut u8;
                    }
                }
            }
            prev = cur;
            cur = hole_next;
        }
        core::ptr::null_mut()
    }

    /// If the just-freed, coalesced hole `[faddr, faddr + fsize)` is exactly
    /// the whole usable area of a grown region, unlink that hole and the
    /// region and return the chunk to the [`HeapSource`] — the heap shrinks.
    ///
    /// The bootstrap region is never in the region list, so it is never
    /// returned; grown regions carry a header separator, so a fully-drained
    /// grown region is always exactly one hole (never coalesced into a
    /// neighbour), which is what makes this exact match reliable.
    ///
    /// # Safety
    ///
    /// Called under the lock, immediately after [`insert_hole`] reported
    /// `(faddr, fsize)` as the resulting free block.
    unsafe fn try_shrink(inner: &mut Inner, faddr: usize, fsize: usize) {
        let Some(source) = inner.source else {
            return;
        };
        let mut prev: Option<NonNull<RegionHeader>> = None;
        let mut cur = inner.regions;
        while let Some(region) = cur {
            let base = region.as_ptr() as usize;
            // SAFETY: `region` heads a live grown region.
            let (total_len, next) = unsafe { (region.as_ref().total_len, region.as_ref().next) };
            if faddr == base + REGION_HDR && fsize == total_len - REGION_HDR {
                // The whole grown region is free. Remove its single hole and
                // the region header, then hand the chunk back.
                // SAFETY: the free list and region list are well-formed.
                unsafe {
                    remove_hole(&mut inner.head, faddr);
                    match prev {
                        Some(p) => (*p.as_ptr()).next = next,
                        None => inner.regions = next,
                    }
                }
                inner.capacity -= total_len - REGION_HDR;
                source.shrink(base as *mut u8, total_len);
                return;
            }
            prev = cur;
            cur = next;
        }
    }
}

/// Insert the hole at `[addr, addr+size)` into the address-sorted list and
/// coalesce it with an immediately-adjacent predecessor and/or successor,
/// returning the `(addr, size)` of the resulting (possibly merged) hole.
///
/// Coalescing is never allowed across `boundary` (the bootstrap-region
/// base): a grown region can sit physically just below the bootstrap
/// region, and merging their holes would strand the grown chunk (it could
/// no longer be recognised as wholly free and returned to the source). A
/// merge point equal to `boundary` is therefore refused, keeping every
/// returnable region's free space distinct. Merges between two grown
/// regions cannot arise at all — each carries a header separator — so the
/// single `boundary` check is sufficient. Passing `usize::MAX` disables
/// the check (no boundary in play).
///
/// # Safety
///
/// `addr` lies in a heap region and `size >= MIN_BLOCK` and is
/// `ALIGN`-aligned; the block is not currently in the free list.
unsafe fn insert_hole(
    head: &mut Option<NonNull<Hole>>,
    addr: usize,
    size: usize,
    boundary: usize,
) -> (usize, usize) {
    // Find the insertion point: `prev` is the last hole with address < addr.
    let mut prev: Option<NonNull<Hole>> = None;
    let mut cur = *head;
    while let Some(node) = cur {
        if (node.as_ptr() as usize) >= addr {
            break;
        }
        prev = cur;
        // SAFETY: `node` is a live hole in the list.
        cur = unsafe { node.as_ref().next };
    }

    // Coalesce forward: if the new block ends exactly where `cur` starts,
    // absorb `cur` into the new block — unless that shared point is the
    // forbidden region boundary.
    let mut new_size = size;
    let mut next = cur;
    if let Some(node) = cur {
        let node_addr = node.as_ptr() as usize;
        if addr + size == node_addr && node_addr != boundary {
            // SAFETY: `node` is a live hole.
            let node_size = unsafe { node.as_ref().size };
            new_size += node_size;
            // SAFETY: live hole.
            next = unsafe { node.as_ref().next };
        }
    }

    // Coalesce backward: if `prev` ends exactly where the new block starts,
    // extend `prev` instead of inserting a new node — again unless that
    // shared point is the forbidden region boundary.
    if let Some(p) = prev {
        let p_addr = p.as_ptr() as usize;
        // SAFETY: `p` is a live hole.
        let p_size = unsafe { p.as_ref().size };
        if p_addr + p_size == addr && addr != boundary {
            // SAFETY: `p` is live; extend it to cover the new (already
            // forward-coalesced) block and relink past any absorbed `cur`.
            unsafe {
                let p_mut = &mut *p.as_ptr();
                p_mut.size = p_size + new_size;
                p_mut.next = next;
            }
            return (p_addr, p_size + new_size);
        }
    }

    // No backward merge: write a fresh header at `addr` and link it in.
    let node = addr as *mut Hole;
    // SAFETY: `addr` is an aligned, owned, off-list block of `>= MIN_BLOCK`
    // bytes, so the header write is in-bounds.
    unsafe {
        node.write(Hole {
            size: new_size,
            next,
        });
    }
    let node = NonNull::new(node);
    match prev {
        // SAFETY: `p` is a live hole; relink it to the new node.
        Some(p) => unsafe {
            (*p.as_ptr()).next = node;
        },
        None => *head = node,
    }
    (addr, new_size)
}

/// Unlink the free hole that starts exactly at `addr` from the list.
///
/// Used by [`FreeListAllocator::try_shrink`] to lift a fully-drained grown
/// region's single hole out before its backing chunk is returned to the
/// [`HeapSource`]. The hole is known to exist at `addr` (the caller matched
/// it against a live region), so a missing match is a no-op fail-safe.
///
/// # Safety
///
/// Called under the lock with a well-formed free list.
unsafe fn remove_hole(head: &mut Option<NonNull<Hole>>, addr: usize) {
    let mut prev: Option<NonNull<Hole>> = None;
    let mut cur = *head;
    while let Some(node) = cur {
        // SAFETY: `node` is a live hole.
        let next = unsafe { node.as_ref().next };
        if node.as_ptr() as usize == addr {
            match prev {
                // SAFETY: `p` is a live hole; relink past `node`.
                Some(p) => unsafe {
                    (*p.as_ptr()).next = next;
                },
                None => *head = next,
            }
            return;
        }
        prev = cur;
        cur = next;
    }
}

unsafe impl GlobalAlloc for FreeListAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(size) = block_size(layout) else {
            return core::ptr::null_mut();
        };
        let align = if layout.align() > ALIGN {
            layout.align()
        } else {
            ALIGN
        };
        let boundary = self.heap_base as usize;
        // SAFETY: serialised access; `ensure_init` plants the heap once.
        unsafe {
            self.with_inner(|inner| {
                self.ensure_init(inner);
                // Try the current free list first.
                let p = Self::carve(inner, size, align, boundary);
                if !p.is_null() {
                    return p;
                }
                // No hole fits: ask the installed source for a fresh region,
                // register it, and retry once. The request reserves the
                // region header plus the block plus alignment slack, so a
                // source that honours `min_len` guarantees the retry fits —
                // no unbounded grow loop. With no source, or on genuine
                // exhaustion, fail closed with null (deterministic OOM,
                // never a panic).
                let Some(source) = inner.source else {
                    return core::ptr::null_mut();
                };
                let Some(min_len) = REGION_HDR
                    .checked_add(size)
                    .and_then(|v| v.checked_add(align))
                    .and_then(|v| v.checked_add(align))
                else {
                    return core::ptr::null_mut();
                };
                let Some((base, len)) = source.grow(min_len) else {
                    return core::ptr::null_mut();
                };
                // A chunk too small to host the header plus one representable
                // block is unusable; hand it straight back rather than
                // corrupting the region list.
                if len < REGION_HDR + MIN_BLOCK || base.is_null() {
                    source.shrink(base, len);
                    return core::ptr::null_mut();
                }
                // Plant the region header at the chunk base and link it in.
                // SAFETY: `base` owns `len >= REGION_HDR + MIN_BLOCK`
                // `ALIGN`-aligned bytes, so the header write is in bounds and
                // well-aligned.
                #[allow(clippy::cast_ptr_alignment)]
                let header = base.cast::<RegionHeader>();
                header.write(RegionHeader {
                    total_len: len,
                    next: inner.regions,
                });
                inner.regions = NonNull::new(header);
                let usable = len - REGION_HDR;
                inner.capacity += usable;
                // Add the usable area as a fresh hole (never merges across
                // the bootstrap boundary), then retry the carve. The usable
                // area is owned, off-list, and aligned.
                insert_hole(
                    &mut inner.head,
                    base as usize + REGION_HDR,
                    usable,
                    boundary,
                );
                Self::carve(inner, size, align, boundary)
            })
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let Some(size) = block_size(layout) else {
            return;
        };
        let addr = ptr as usize;
        let boundary = self.heap_base as usize;
        // SAFETY: serialised access; `ptr`/`size` reconstruct the exact
        // block `alloc` carved (same `block_size`), so reinserting it is in
        // bounds and cannot overlap a live block.
        unsafe {
            self.with_inner(|inner| {
                // `alloc` carved exactly `block_size(layout)` bytes (it skips
                // a hole rather than absorbing an unrepresentable remnant), so
                // freeing the same `size` reclaims the whole block with no
                // leak. `insert_hole` coalesces it with adjacent free
                // neighbours, undoing fragmentation, and reports the merged
                // hole so a wholly-drained grown region can be returned.
                let (faddr, fsize) = insert_hole(&mut inner.head, addr, size, boundary);
                inner.used = inner.used.saturating_sub(size);
                Self::try_shrink(inner, faddr, fsize);
            });
        }
    }
}

#[cfg(test)]
mod tests;
