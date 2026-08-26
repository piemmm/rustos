//! Freeing kernel heap allocator for freestanding TAIRiX images.
//!
//! # Why a freeing allocator
//!
//! The kernel global heap is long-lived: the driver-store service, the
//! scheduler, and the syscall paths allocate and free for the life of the
//! system. A forward-only bump allocator never reclaimed, so any sustained
//! allocation traffic eventually exhausted the heap and the
//! `#[alloc_error_handler]` *panicked*, where deterministic OOM demands a
//! fallible signal (the `GlobalAlloc` null return).
//!
//! # A growable, shrinkable heap — not a fixed ceiling
//!
//! The heap is *not* capped at a hand-picked constant. It starts on a
//! small bootstrap region (a `.bss` arena that covers early boot, before a
//! physical frame allocator exists) and, once the boot path installs a
//! [`HeapSource`], **grows on demand** by drawing fresh regions from that
//! source and **shrinks** by handing whole drained regions back. The
//! production source assembles each region out of several physical chunks
//! mapped into one virtually contiguous kernel window, so growth is bounded
//! by total free RAM rather than by the largest physically contiguous block
//! available; because the frame allocator and that window's bookkeeping are
//! both heap-independent by construction, growth never re-enters this heap's
//! own lock.
//!
//! # Design: segregated fit over boundary tags, O(1) throughout
//!
//! Every block carries an in-band header — its size, its flags, and the
//! address of its physical predecessor — so a free block finds both physical
//! neighbours by arithmetic and coalescing is a constant-time pointer fix-up.
//! Free blocks are threaded onto **segregated free lists** indexed by a
//! two-level size class (a power-of-two first level, a linear second level
//! subdividing each octave), and a bitmap per level makes "the smallest
//! non-empty class that can satisfy this request" a pair of bit-scans. So
//! `alloc`, `dealloc`, coalescing, and returning a drained region are each
//! O(1) with no list ever walked.
//!
//! That is the whole point of the shape. The predecessor design was a single
//! address-sorted list of holes: allocation walked it first-fit, freeing
//! walked it again to find the ordered insertion point, and every free
//! additionally walked the whole region list looking for a drained region.
//! Because a grown region carries a header separator that free space may not
//! coalesce across, the hole count could never fall below the region count,
//! and the region count grows with the heap — so once the heap grew past its
//! bootstrap arena, *every allocation and every free in the kernel* paid a
//! cost linear in how much the heap had ever grown. Measured on the desktop's
//! wallpaper gallery that was a 5× slowdown over 26 MB of file reads and
//! still climbing, on a path whose filesystem driver measures 370 MB/s. A
//! foundational allocator has to be O(1); this one is.
//!
//! Each grown region keeps its header separator, and the region list is
//! doubly linked, so a drained region is unlinked and returned in constant
//! time. A block that spans its whole region is recognised by its own flags
//! (first *and* last block of the region) rather than by searching.
//!
//! # Interrupt-safe lock
//!
//! TAIRiX takes interrupts while in-kernel code runs, so an interrupt
//! service routine can fire on a CPU that is mid-allocation holding the
//! lock. Were the lock left plain, an ISR that allocated or freed would spin
//! forever on the lock its own interrupted mainline holds — a single-CPU
//! self-deadlock. The lock is therefore **interrupt-safe**: it masks the
//! current CPU's interrupts for the whole hold. The masking primitive is
//! architecture-specific, so the freestanding bin installs it during boot
//! ([`FreeListAllocator::install_irq_control`]); until then the context is
//! single-CPU with interrupts already masked.

#![no_std]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Bytes of the bootstrap heap arena a freestanding bin reserves in `.bss`.
///
/// This is the *bootstrap* region only — the heap grows past it from the
/// installed [`HeapSource`], so it is a boot floor and never a ceiling.
/// It must cover every allocation made before the boot path can install
/// that source.
pub const HEAP_BYTES: usize = 64 * 1024 * 1024;

/// 4 KiB-aligned heap storage handed out by [`FreeListAllocator`].
///
/// Aligned to a page so a page-aligned request is never wasted satisfying
/// it out of a half-aligned tail.
#[repr(C, align(4096))]
pub struct Heap([u8; HEAP_BYTES]);

impl Heap {
    /// Zero-initialised heap. `const` so the binary's arena is constructed
    /// in `.bss`, never on the stack (clippy's `large_stack_arrays` is a
    /// false positive: no `Heap` value ever materialises as a local; every
    /// consumer assigns `Heap::ZERO` directly to a `static`).
    #[allow(clippy::large_stack_arrays)]
    pub const ZERO: Self = Self([0; HEAP_BYTES]);
}

/// Machine word.
const WORD: usize = size_of::<usize>();

/// Alignment every block start, block size, and payload is rounded to.
///
/// Two words, matching the platform's natural malloc guarantee, so a payload
/// is correctly aligned for any ordinary scalar without the over-aligned
/// padding path below. It also leaves the low bits of a size free for
/// [`FLAG_MASK`].
const ALIGN: usize = 2 * WORD;

/// In-band per-block header bytes: `{ size_and_flags, prev_phys }`.
///
/// Equal to [`ALIGN`], so a payload at `block + HEADER` inherits the block
/// start's alignment.
const HEADER: usize = 2 * WORD;

/// Smallest representable block: a header plus room for the two free-list
/// links a *free* block parks in its own payload.
pub(crate) const MIN_BLOCK: usize = HEADER + 2 * WORD;

/// The block is free (on a segregated list).
const FLAG_FREE: usize = 0b001;
/// The block is the first in its region, so it has no physical predecessor.
const FLAG_REGION_START: usize = 0b010;
/// The block is the last in its region, so it has no physical successor.
const FLAG_LAST: usize = 0b100;
/// Low bits of `size_and_flags` that carry flags rather than size.
const FLAG_MASK: usize = ALIGN - 1;

// A size is `ALIGN`-aligned, so its low bits are always free for flags, and
// there must be at least three of them.
const _: () = assert!(FLAG_MASK >= 0b111);
const _: () = assert!(ALIGN.is_power_of_two() && HEADER == ALIGN);

/// Second-level bits: each octave is subdivided into `1 << SL_BITS` classes.
const SL_BITS: u32 = 4;
/// Second-level classes per first level.
const SL_COUNT: usize = 1 << SL_BITS;
/// Sizes below this are classed linearly in first level `0`, one class per
/// [`ALIGN`] step, because an octave that small cannot be subdivided.
const SMALL_LIMIT: usize = ALIGN * SL_COUNT;
/// `log2(SMALL_LIMIT)`, the first level's origin.
const SMALL_SHIFT: u32 = SMALL_LIMIT.trailing_zeros();
/// First levels: one per octave from [`SMALL_LIMIT`] to the widest `usize`,
/// plus the linear level `0`. Derived from the word width, never picked.
const FL_COUNT: usize = (usize::BITS - SMALL_SHIFT + 1) as usize;

// The linear level must have a class for every `ALIGN` step below the limit.
const _: () = assert!(SMALL_LIMIT / ALIGN == SL_COUNT);

/// Round `value` up to the next multiple of `align` (a power of two), or
/// [`None`] on overflow.
const fn align_up(value: usize, align: usize) -> Option<usize> {
    let mask = align - 1;
    match value.checked_add(mask) {
        Some(v) => Some(v & !mask),
        None => None,
    }
}

/// The block byte length an allocation of `layout` occupies: its header plus
/// the requested payload, floored at [`MIN_BLOCK`] and rounded to [`ALIGN`]
/// so the next block start stays aligned.
fn block_size(layout: Layout) -> Option<usize> {
    let want = align_up(layout.size().checked_add(HEADER)?, ALIGN)?;
    Some(if want > MIN_BLOCK { want } else { MIN_BLOCK })
}

/// The first-level and second-level class an *existing* block of `size`
/// belongs to.
fn class_of(size: usize) -> (usize, usize) {
    if size < SMALL_LIMIT {
        return (0, size / ALIGN);
    }
    let octave = usize::BITS - 1 - size.leading_zeros();
    let sl = (size >> (octave - SL_BITS)) & (SL_COUNT - 1);
    ((octave + 1 - SMALL_SHIFT) as usize, sl)
}

/// `size` raised to its class's upper bound, so every block in
/// `class_of(round_up_class(size))` is at least `size` bytes.
///
/// Both the allocation search and [`FreeListAllocator::grow`] round through
/// here: a region drawn at an unrounded size could land in a class *below*
/// the one a search for that size starts at, and so never be found.
fn round_up_class(size: usize) -> usize {
    if size < SMALL_LIMIT {
        // Linear classes hold exactly one size each, so no rounding is due.
        return size;
    }
    let octave = usize::BITS - 1 - size.leading_zeros();
    let round = (1usize << (octave - SL_BITS)) - 1;
    size.saturating_add(round) & !round
}

/// The class to *begin a search* for `size` at: every block it holds fits.
fn search_class(size: usize) -> (usize, usize) {
    class_of(round_up_class(size))
}

/// A block's in-band header. Present on free *and* live blocks, which is
/// what makes coalescing constant-time.
#[repr(C)]
struct Block {
    /// Total block bytes (header included) in the bits above [`FLAG_MASK`],
    /// flags below.
    size_and_flags: usize,
    /// Address of the physical predecessor, meaningless when
    /// [`FLAG_REGION_START`] is set.
    prev_phys: usize,
}

/// The two free-list links a free block parks in its own payload.
#[repr(C)]
struct FreeLinks {
    next: Option<NonNull<Block>>,
    prev: Option<NonNull<Block>>,
}

impl Block {
    fn size(&self) -> usize {
        self.size_and_flags & !FLAG_MASK
    }

    fn flags(&self) -> usize {
        self.size_and_flags & FLAG_MASK
    }

    fn has(&self, flag: usize) -> bool {
        self.size_and_flags & flag != 0
    }

    fn set_size(&mut self, size: usize) {
        self.size_and_flags = size | self.flags();
    }

    fn set(&mut self, flag: usize, on: bool) {
        if on {
            self.size_and_flags |= flag;
        } else {
            self.size_and_flags &= !flag;
        }
    }
}

/// Read the free-list links parked in `block`'s payload.
///
/// # Safety
///
/// `block` is a live free block of at least [`MIN_BLOCK`] bytes, so its
/// payload holds a [`FreeLinks`].
unsafe fn links(block: NonNull<Block>) -> *mut FreeLinks {
    // SAFETY: the block owns `HEADER + 2 * WORD` bytes at minimum, so the
    // payload start is in bounds and `ALIGN`-aligned for `FreeLinks`.
    unsafe { block.byte_add(HEADER).cast::<FreeLinks>().as_ptr() }
}

/// Header planted at the base of every *grown* region (never the fixed
/// bootstrap region), linking the regions the heap can hand back.
///
/// It does double duty: it keeps the region list allocation-free, and it
/// separates one region's blocks from another's so free space never
/// coalesces across a boundary and a drained region can be returned intact.
/// The list is **doubly** linked so that unlinking a drained region is O(1)
/// rather than a search.
#[repr(C)]
struct RegionHeader {
    /// Total byte length of the chunk, as passed to [`HeapSource::shrink`].
    total_len: usize,
    next: Option<NonNull<RegionHeader>>,
    prev: Option<NonNull<RegionHeader>>,
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

/// A source of fresh memory the heap grows into, and returns to on shrink.
///
/// The fixed bootstrap region a [`FreeListAllocator`] is constructed over
/// (`.bss` in the kernel binaries) covers early boot, before a physical
/// frame allocator exists. Once one does, the boot path installs a
/// `HeapSource` ([`FreeListAllocator::install_source`]) so the kernel heap
/// grows and shrinks on demand instead of being capped at a hand-picked
/// constant — the growable-capacity rule the charter requires of every
/// resource ceiling.
///
/// # Contract
///
/// * [`grow`](Self::grow) returns a chunk of at least `min_len` writable
///   bytes, aligned to two machine words, owned exclusively by the heap
///   until it is handed back, or `None` on genuine exhaustion
///   (deterministic OOM — the heap then returns null from `alloc`, never a
///   panic).
/// * [`shrink`](Self::shrink) is only ever called with the exact
///   `(base, len)` pair a prior `grow` returned, once the heap has drained
///   every byte of that chunk.
///
/// The source is consulted only while the allocator holds its own lock, so
/// an implementation must **not** call back into this same heap (that would
/// re-enter the non-reentrant lock and deadlock). The production source
/// satisfies this: the frame allocator, the page-table frame source, and
/// the window's own address-space bookkeeping are all heap-independent by
/// construction.
pub trait HeapSource: Sync {
    /// Provide a fresh chunk of at least `min_len` writable bytes, aligned to
    /// two machine words, or `None` when no more memory can be given (fail
    /// closed).
    fn grow(&self, min_len: usize) -> Option<(*mut u8, usize)>;

    /// Return a chunk previously produced by [`grow`](Self::grow), given its
    /// exact base and length.
    fn shrink(&self, base: *mut u8, len: usize);
}

/// The mutable allocator state guarded by [`FreeListAllocator::lock`].
struct Inner {
    /// `true` once the bootstrap region's initial block has been planted.
    initialised: bool,
    /// Bytes currently handed out (diagnostic).
    used: usize,
    /// Total usable bytes across the bootstrap region and every currently
    /// mapped grown region — the denominator `remaining` reports against.
    /// Grows and shrinks with the grown regions, never a fixed ceiling.
    capacity: usize,
    /// Head of the doubly-linked list of grown regions (those obtained from
    /// the [`HeapSource`] and returnable to it).
    regions: Option<NonNull<RegionHeader>>,
    /// The installed growth source, or `None` before the boot path installs
    /// one (the state in which the heap is capped at its bootstrap region).
    source: Option<&'static dyn HeapSource>,
    /// Set bit per first level holding at least one free block.
    fl_bitmap: usize,
    /// Set bit per second-level class holding at least one free block.
    sl_bitmap: [u32; FL_COUNT],
    /// Head of each segregated free list.
    heads: [[Option<NonNull<Block>>; SL_COUNT]; FL_COUNT],
    /// Test-only probe: how many *other* free-list or region-list nodes the
    /// operations since the last reset had to reach. It pins the O(1)
    /// property — a reintroduced list walk makes this grow with the heap.
    #[cfg(test)]
    steps: usize,
}

impl Inner {
    const fn new() -> Self {
        Self {
            initialised: false,
            used: 0,
            capacity: 0,
            regions: None,
            source: None,
            fl_bitmap: 0,
            sl_bitmap: [0; FL_COUNT],
            heads: [[None; SL_COUNT]; FL_COUNT],
            #[cfg(test)]
            steps: 0,
        }
    }

    /// Thread `block` onto the segregated list for its size.
    ///
    /// # Safety
    ///
    /// Called under the lock. `block` is off every list, marked free, and at
    /// least [`MIN_BLOCK`] bytes.
    unsafe fn push_free(&mut self, block: NonNull<Block>) {
        // SAFETY: `block` is a live header the lock makes exclusively ours.
        let size = unsafe { block.as_ref().size() };
        let (fl, sl) = class_of(size);
        let head = self.heads[fl][sl];
        // SAFETY: the payload of a `>= MIN_BLOCK` free block holds the links.
        unsafe {
            links(block).write(FreeLinks {
                next: head,
                prev: None,
            });
        }
        if let Some(old) = head {
            #[cfg(test)]
            {
                self.steps += 1;
            }
            // SAFETY: `old` is a live free block already on this list.
            unsafe { (*links(old)).prev = Some(block) };
        }
        self.heads[fl][sl] = Some(block);
        self.fl_bitmap |= 1 << fl;
        self.sl_bitmap[fl] |= 1 << sl;
    }

    /// Unthread `block` from the segregated list for its size.
    ///
    /// # Safety
    ///
    /// Called under the lock. `block` is currently on the list for its size.
    unsafe fn pop_free(&mut self, block: NonNull<Block>) {
        // SAFETY: `block` is a live free header.
        let size = unsafe { block.as_ref().size() };
        let (fl, sl) = class_of(size);
        // SAFETY: `block` is free, so its payload holds its links.
        let FreeLinks { next, prev } = unsafe { links(block).read() };
        match prev {
            // SAFETY: `prev` is a live free block on the same list.
            Some(p) => unsafe {
                #[cfg(test)]
                {
                    self.steps += 1;
                }
                (*links(p)).next = next;
            },
            None => self.heads[fl][sl] = next,
        }
        if let Some(n) = next {
            #[cfg(test)]
            {
                self.steps += 1;
            }
            // SAFETY: `next` is a live free block on the same list.
            unsafe { (*links(n)).prev = prev };
        }
        if self.heads[fl][sl].is_none() {
            self.sl_bitmap[fl] &= !(1 << sl);
            if self.sl_bitmap[fl] == 0 {
                self.fl_bitmap &= !(1 << fl);
            }
        }
    }

    /// The head of the smallest non-empty class whose blocks all satisfy
    /// `size`, or `None` when no free block is large enough. Two bit-scans,
    /// no list walked.
    fn find_free(&self, size: usize) -> Option<NonNull<Block>> {
        let (fl, sl) = search_class(size);
        if fl >= FL_COUNT {
            return None;
        }
        // Classes at or above `sl` within this first level.
        let mut level = fl;
        // Classes at or above `sl`; an out-of-range shift masks to nothing,
        // so an impossible class simply finds no block.
        let sl_shift = u32::try_from(sl).unwrap_or(u32::BITS);
        let mut classes = self.sl_bitmap[fl] & u32::MAX.checked_shl(sl_shift).unwrap_or(0);
        if classes == 0 {
            // Nothing here fits; take the next non-empty first level, whose
            // every class is wider than this one's.
            let fl_shift = u32::try_from(fl + 1).unwrap_or(u32::BITS);
            let higher = self.fl_bitmap & usize::MAX.checked_shl(fl_shift).unwrap_or(0);
            if higher == 0 {
                return None;
            }
            level = higher.trailing_zeros() as usize;
            classes = self.sl_bitmap[level];
        }
        self.heads[level][classes.trailing_zeros() as usize]
    }
}

/// A coalescing, segregated-fit kernel heap allocator with O(1) allocate,
/// free, and region reclaim.
///
/// Implements [`GlobalAlloc`]. Pair with a `static HEAP: Heap`
/// (`#[repr(C, align(4096))]`) in the bin crate and register via
/// `#[global_allocator]`. The `heap_base` pointer must stay valid for the
/// life of the binary.
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
// and the raw `heap_base` is only ever read, never aliased as a reference.
unsafe impl Sync for FreeListAllocator {}
// SAFETY: as for `Sync` — the type owns its arena and hands out disjoint
// blocks, so moving it between threads aliases nothing.
unsafe impl Send for FreeListAllocator {}

impl FreeListAllocator {
    /// Construct an allocator over an existing heap region.
    ///
    /// # Safety
    ///
    /// * `heap_base` must be non-null, aligned to two machine words, and
    ///   point at `heap_len` writable bytes owned exclusively by this
    ///   allocator for the whole life of the program.
    /// * The region must not overlap any other live allocation.
    #[must_use]
    pub const unsafe fn new(heap_base: *mut u8, heap_len: usize) -> Self {
        Self {
            heap_base,
            heap_len,
            lock: AtomicBool::new(false),
            irq_disable: AtomicUsize::new(0),
            irq_restore: AtomicUsize::new(0),
            inner: UnsafeCell::new(Inner::new()),
        }
    }

    /// Install the growth source the heap draws fresh memory from once one
    /// exists (the boot path calls this after building the frame
    /// allocator).
    ///
    /// Idempotent-by-policy: the boot path installs exactly one source for
    /// the life of the binary. Before a source is installed the heap is
    /// confined to its bootstrap region and returns null once that is
    /// exhausted.
    pub fn install_source(&self, source: &'static dyn HeapSource) {
        self.with_inner(|inner| inner.source = Some(source));
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
    /// lock and reenter the allocator, self-deadlocking on it.
    ///
    /// Private, and never called from inside `f`: the lock is not reentrant,
    /// which is an invariant of this module rather than a contract on any
    /// caller outside it, so this is safe to call.
    fn with_inner<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
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

    /// Plant the bootstrap region's single free block if not yet done.
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
        inner.capacity = self.heap_len;
        let span = self.heap_len & !FLAG_MASK;
        if span >= MIN_BLOCK {
            // SAFETY: `heap_base` is `ALIGN`-aligned and owns `heap_len`
            // bytes, and `span >= MIN_BLOCK`, so the block fits.
            unsafe { Self::plant_region_block(inner, self.heap_base, span) };
        }
    }

    /// Write a single free block covering `[base, base + span)` and thread it
    /// onto the free lists. The block is both the first and last of its
    /// region, so nothing coalesces across the region's ends.
    ///
    /// # Safety
    ///
    /// Called under the lock. `base` is `ALIGN`-aligned and owns `span`
    /// writable bytes, `span` is `ALIGN`-aligned and at least [`MIN_BLOCK`],
    /// and no live block overlaps the range.
    unsafe fn plant_region_block(inner: &mut Inner, base: *mut u8, span: usize) {
        // `base` is `ALIGN`-aligned by contract, which is `align_of::<Block>()`
        // or more; the lint cannot see that invariant.
        #[allow(clippy::cast_ptr_alignment)]
        let block = base.cast::<Block>();
        // SAFETY: `base` owns `span >= MIN_BLOCK > HEADER` aligned bytes.
        unsafe {
            block.write(Block {
                size_and_flags: span | FLAG_FREE | FLAG_REGION_START | FLAG_LAST,
                prev_phys: 0,
            });
        }
        // SAFETY: just written, so non-null and live.
        unsafe { inner.push_free(NonNull::new_unchecked(block)) };
    }

    /// Take and clear the test-only node-reach probe.
    #[cfg(test)]
    pub(crate) fn take_steps(&self) -> usize {
        self.with_inner(|inner| core::mem::take(&mut inner.steps))
    }

    /// Bytes currently handed out (diagnostic).
    #[must_use]
    pub fn used(&self) -> usize {
        self.with_inner(|inner| inner.used)
    }

    /// Bytes not currently handed out (diagnostic; includes free-list
    /// fragmentation, so a single allocation of this size may still fail).
    ///
    /// Measured against the *current* capacity (bootstrap region plus every
    /// grown region), which rises and falls as the heap grows and shrinks,
    /// so this is never bounded by a fixed heap size.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.with_inner(|inner| inner.capacity.saturating_sub(inner.used))
    }

    /// The physical successor of `block`, or `None` when it ends its region.
    ///
    /// # Safety
    ///
    /// Called under the lock; `block` is a live header.
    unsafe fn next_phys(block: NonNull<Block>) -> Option<NonNull<Block>> {
        // SAFETY: `block` is live.
        let b = unsafe { block.as_ref() };
        if b.has(FLAG_LAST) {
            return None;
        }
        // SAFETY: not the last block, so `block + size` is another header
        // inside the same region.
        Some(unsafe { block.byte_add(b.size()) })
    }

    /// Carve `size` bytes out of the free block `block`, returning the
    /// payload pointer. Any tail above `size` is returned to the free lists
    /// as its own block; a tail too small to represent is absorbed.
    ///
    /// # Safety
    ///
    /// Called under the lock with `block` already removed from its free list,
    /// `block.size() >= size`, and `size` `ALIGN`-aligned and at least
    /// [`MIN_BLOCK`].
    unsafe fn occupy(inner: &mut Inner, block: NonNull<Block>, size: usize) -> *mut u8 {
        // SAFETY: `block` is a live, off-list header.
        let total = unsafe { block.as_ref().size() };
        let tail = total - size;
        if tail >= MIN_BLOCK {
            // SAFETY: `block` is live and owns `total` bytes.
            let was_last = unsafe { block.as_ref().has(FLAG_LAST) };
            // SAFETY: `size` is inside the block, so this is a header slot.
            let rest = unsafe { block.byte_add(size) };
            // SAFETY: `rest` is `ALIGN`-aligned (both `block` and `size` are)
            // and owns `tail >= MIN_BLOCK` bytes inside the same region.
            unsafe {
                rest.write(Block {
                    size_and_flags: tail | FLAG_FREE | if was_last { FLAG_LAST } else { 0 },
                    prev_phys: block.as_ptr() as usize,
                });
            }
            // SAFETY: `block` is live; it is no longer the region's last.
            unsafe {
                let b = &mut *block.as_ptr();
                b.set_size(size);
                b.set(FLAG_LAST, false);
            }
            // SAFETY: `rest` was just written, so it is a live header.
            if let Some(after) = unsafe { Self::next_phys(rest) } {
                // SAFETY: `after` is a live header in this region.
                unsafe { (*after.as_ptr()).prev_phys = rest.as_ptr() as usize };
            }
            // SAFETY: `rest` is off every list and marked free.
            unsafe { inner.push_free(rest) };
            inner.used += size;
        } else {
            inner.used += total;
        }
        // SAFETY: `block` is live and now sized to what it keeps.
        unsafe { (*block.as_ptr()).set(FLAG_FREE, false) };
        // SAFETY: the block owns at least `HEADER` bytes before its payload.
        unsafe { block.as_ptr().cast::<u8>().add(HEADER) }
    }

    /// Draw a fresh region from the installed source, big enough to host a
    /// block of `size`, and thread its free block onto the lists.
    ///
    /// # Safety
    ///
    /// Called under the lock.
    unsafe fn grow(inner: &mut Inner, size: usize) -> Option<()> {
        let source = inner.source?;
        // Draw the class-rounded size, not the raw one, so the planted block
        // is findable by a search for `size`; plus the region's own header.
        let rounded = round_up_class(size);
        let want = rounded.checked_add(REGION_HDR)?;
        let (base, len) = source.grow(want)?;
        if base.is_null() || len < want {
            return None;
        }
        let span = (len - REGION_HDR) & !FLAG_MASK;
        if span < MIN_BLOCK || span < rounded {
            source.shrink(base, len);
            return None;
        }
        // `base` is `ALIGN`-aligned by the source contract.
        #[allow(clippy::cast_ptr_alignment)]
        let header = base.cast::<RegionHeader>();
        // SAFETY: the source handed us `len >= REGION_HDR` exclusively-owned
        // aligned bytes at `base`.
        unsafe {
            header.write(RegionHeader {
                total_len: len,
                next: inner.regions,
                prev: None,
            });
        }
        // SAFETY: just written, so non-null and live.
        let header = unsafe { NonNull::new_unchecked(header) };
        if let Some(old) = inner.regions {
            // SAFETY: `old` is a live region header on the list.
            unsafe { (*old.as_ptr()).prev = Some(header) };
        }
        inner.regions = Some(header);
        inner.capacity = inner.capacity.saturating_add(span);
        // SAFETY: the usable area follows the region header, is `ALIGN`-aligned
        // and owns `span >= MIN_BLOCK` bytes no live block overlaps.
        unsafe { Self::plant_region_block(inner, base.add(REGION_HDR), span) };
        Some(())
    }

    /// If the freed, coalesced `block` now spans the whole usable area of a
    /// *grown* region, unlink it and hand the chunk back to the source — the
    /// heap shrinks.
    ///
    /// O(1): a block that spans its region is exactly one that is both the
    /// region's first and last, so no search is needed, and the region list
    /// is doubly linked so unlinking is a pair of pointer writes. The
    /// bootstrap region carries no [`RegionHeader`] and is never returned,
    /// which is what `heap_base` distinguishes.
    ///
    /// # Safety
    ///
    /// Called under the lock, with `block` free and on its list.
    unsafe fn try_shrink(inner: &mut Inner, heap_base: *mut u8, block: NonNull<Block>) {
        let Some(source) = inner.source else {
            return;
        };
        // SAFETY: `block` is a live free header.
        let b = unsafe { block.as_ref() };
        if !(b.has(FLAG_REGION_START) && b.has(FLAG_LAST)) {
            return;
        }
        let addr = block.as_ptr() as usize;
        if addr == heap_base as usize {
            // The bootstrap arena: not the source's, never handed back.
            return;
        }
        let base = addr - REGION_HDR;
        // SAFETY: a grown region's usable area starts exactly `REGION_HDR`
        // after its header, so this is that live header.
        let header = unsafe { NonNull::new_unchecked(base as *mut RegionHeader) };
        // SAFETY: `header` is live.
        let RegionHeader {
            total_len,
            next,
            prev,
        } = unsafe { header.as_ptr().read() };
        #[cfg(test)]
        {
            inner.steps += 1;
        }
        // SAFETY: `block` is on its free list.
        unsafe { inner.pop_free(block) };
        match prev {
            // SAFETY: `p` is a live region header on the list.
            Some(p) => unsafe {
                #[cfg(test)]
                {
                    inner.steps += 1;
                }
                (*p.as_ptr()).next = next;
            },
            None => inner.regions = next,
        }
        if let Some(n) = next {
            #[cfg(test)]
            {
                inner.steps += 1;
            }
            // SAFETY: `n` is a live region header on the list.
            unsafe { (*n.as_ptr()).prev = prev };
        }
        inner.capacity = inner.capacity.saturating_sub(b.size());
        source.shrink(base as *mut u8, total_len);
    }
}

unsafe impl GlobalAlloc for FreeListAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let Some(size) = block_size(layout) else {
            return core::ptr::null_mut();
        };
        // A payload lands `HEADER` into an `ALIGN`-aligned block, so anything
        // up to `ALIGN` is already satisfied. A wider alignment may need the
        // block start pushed forward, and the skipped front must itself be a
        // representable free block — so ask for the worst case.
        let over = layout.align() > ALIGN;
        let Some(need) = (if over {
            size.checked_add(layout.align())
                .and_then(|s| s.checked_add(MIN_BLOCK))
        } else {
            Some(size)
        }) else {
            return core::ptr::null_mut();
        };
        self.with_inner(|inner| {
            // SAFETY: under the lock, per `ensure_init`'s contract.
            unsafe { self.ensure_init(inner) };
            let mut found = inner.find_free(need);
            if found.is_none() {
                // SAFETY: under the lock.
                unsafe { Self::grow(inner, need) };
                found = inner.find_free(need);
            }
            let Some(block) = found else {
                return core::ptr::null_mut();
            };
            // SAFETY: `block` is on the free list for a class that fits.
            unsafe { inner.pop_free(block) };
            if !over {
                // SAFETY: off-list free block of at least `size` bytes.
                return unsafe { Self::occupy(inner, block, size) };
            }
            // Push the payload up to the requested alignment, keeping the
            // skipped front as its own free block. `need` reserved the worst
            // case, so a stride bump always leaves a whole block.
            let start = block.as_ptr() as usize;
            let Some(mut payload) = align_up(start + HEADER, layout.align()) else {
                // SAFETY: still a valid free block; put it back.
                unsafe { inner.push_free(block) };
                return core::ptr::null_mut();
            };
            while payload - HEADER - start != 0 && payload - HEADER - start < MIN_BLOCK {
                payload += layout.align();
            }
            let front = payload - HEADER - start;
            if front == 0 {
                // SAFETY: off-list free block of at least `size` bytes.
                return unsafe { Self::occupy(inner, block, size) };
            }
            // SAFETY: `block` owns `front + size` or more; split the front off
            // as its own free block and occupy the remainder.
            let split = unsafe { Self::split_front(inner, block, front) };
            // SAFETY: `split` is the off-list tail, at least `size` bytes.
            unsafe { Self::occupy(inner, split, size) }
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
        if ptr.is_null() {
            return;
        }
        // SAFETY: `ptr` came from `alloc`, so it is non-null and its header
        // sits `HEADER` bytes below it, `ALIGN`-aligned.
        let block = unsafe { NonNull::new_unchecked(ptr).byte_sub(HEADER).cast::<Block>() };
        self.with_inner(|inner| {
            // SAFETY: `block` is the live header of a handed-out block, and
            // the lock makes it exclusively ours.
            unsafe {
                inner.used = inner.used.saturating_sub(block.as_ref().size());
                let merged = Self::free_and_coalesce(inner, block);
                Self::try_shrink(inner, self.heap_base, merged);
            }
        });
    }
}

impl FreeListAllocator {
    /// Split `front` bytes off the head of the off-list free block `block`,
    /// returning the off-list tail. The head is threaded back onto the free
    /// lists.
    ///
    /// # Safety
    ///
    /// Called under the lock with `block` off every free list, `front`
    /// `ALIGN`-aligned and at least [`MIN_BLOCK`], and
    /// `block.size() >= front + MIN_BLOCK`.
    unsafe fn split_front(
        inner: &mut Inner,
        block: NonNull<Block>,
        front: usize,
    ) -> NonNull<Block> {
        // SAFETY: `block` is live.
        let (total, flags) = unsafe { (block.as_ref().size(), block.as_ref().flags()) };
        let tail_addr = block.as_ptr() as usize + front;
        // SAFETY: `tail_addr` is `ALIGN`-aligned inside the same region and
        // owns `total - front >= MIN_BLOCK` bytes.
        unsafe {
            (tail_addr as *mut Block).write(Block {
                size_and_flags: (total - front) | (flags & FLAG_LAST),
                prev_phys: block.as_ptr() as usize,
            });
        }
        // SAFETY: `block` keeps the front; it is no longer the region's last.
        unsafe {
            let b = &mut *block.as_ptr();
            b.set_size(front);
            b.set(FLAG_LAST, false);
            b.set(FLAG_FREE, true);
        }
        // SAFETY: just written, so non-null and live.
        let tail = unsafe { NonNull::new_unchecked(tail_addr as *mut Block) };
        if let Some(after) = unsafe { Self::next_phys(tail) } {
            // SAFETY: `after` is a live header in this region.
            unsafe { (*after.as_ptr()).prev_phys = tail_addr };
        }
        // SAFETY: `block` is off every list and marked free.
        unsafe { inner.push_free(block) };
        tail
    }

    /// Mark `block` free and merge it with whichever physical neighbours are
    /// also free, returning the surviving block (now on its free list).
    ///
    /// Constant time: the successor is `block + size` and the predecessor is
    /// recorded in the header, so neither is searched for. Region ends carry
    /// [`FLAG_REGION_START`] / [`FLAG_LAST`], so a merge can never cross into
    /// a neighbouring region and a drained region stays exactly one block.
    ///
    /// # Safety
    ///
    /// Called under the lock with `block` live and on no free list.
    unsafe fn free_and_coalesce(inner: &mut Inner, block: NonNull<Block>) -> NonNull<Block> {
        let mut block = block;
        // Forward: absorb a free successor.
        // SAFETY: `block` is live.
        if let Some(next) = unsafe { Self::next_phys(block) } {
            // SAFETY: `next` is a live header in this region.
            if unsafe { next.as_ref().has(FLAG_FREE) } {
                #[cfg(test)]
                {
                    inner.steps += 1;
                }
                // SAFETY: `next` is free, so it is on its list.
                unsafe { inner.pop_free(next) };
                // SAFETY: both are live headers in the same region.
                unsafe {
                    let n = next.as_ref();
                    let grown = block.as_ref().size() + n.size();
                    let last = n.has(FLAG_LAST);
                    let b = &mut *block.as_ptr();
                    b.set_size(grown);
                    b.set(FLAG_LAST, last);
                }
            }
        }
        // Backward: extend a free predecessor over this block instead.
        // SAFETY: `block` is live.
        let prev = unsafe {
            let b = block.as_ref();
            if b.has(FLAG_REGION_START) {
                None
            } else {
                NonNull::new(b.prev_phys as *mut Block)
            }
        };
        if let Some(prev) = prev {
            // SAFETY: `prev` is a live header in this region.
            if unsafe { prev.as_ref().has(FLAG_FREE) } {
                #[cfg(test)]
                {
                    inner.steps += 1;
                }
                // SAFETY: `prev` is free, so it is on its list.
                unsafe { inner.pop_free(prev) };
                // SAFETY: both are live headers in the same region.
                unsafe {
                    let b = block.as_ref();
                    let grown = prev.as_ref().size() + b.size();
                    let last = b.has(FLAG_LAST);
                    let p = &mut *prev.as_ptr();
                    p.set_size(grown);
                    p.set(FLAG_LAST, last);
                }
                block = prev;
            }
        }
        // SAFETY: `block` is live and now covers the merged span.
        unsafe { (*block.as_ptr()).set(FLAG_FREE, true) };
        // Whatever follows the merged block now points back at it.
        // SAFETY: `block` is live.
        if let Some(after) = unsafe { Self::next_phys(block) } {
            // SAFETY: `after` is a live header in this region.
            unsafe { (*after.as_ptr()).prev_phys = block.as_ptr() as usize };
        }
        // SAFETY: `block` is off every list (both merge sources were popped)
        // and marked free.
        unsafe { inner.push_free(block) };
        block
    }
}

#[cfg(test)]
mod tests;
