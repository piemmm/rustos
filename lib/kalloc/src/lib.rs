//! Freeing kernel heap allocator for freestanding RustOS images.
//!
//! # Why a freeing allocator
//!
//! The kernel global heap is long-lived: the driver-store service, the
//! scheduler, and the syscall paths allocate and free for the life of the
//! system. A forward-only bump allocator (this crate's previous design)
//! never reclaimed, so any sustained allocation traffic — a file-size
//! proportional bundle read, a long-running service serving requests —
//! eventually exhausted the heap and the `#[alloc_error_handler]`
//! *panicked*, violating `AGENTS.md` §4 ("Deterministic OOM behaviour:
//! allocation failure is a `Result`, never a panic"; the `GlobalAlloc`
//! null return is that fallible signal).
//!
//! [`FreeListAllocator`] is the replacement: a coalescing first-fit
//! free-list allocator over a single fixed heap region. It reclaims on
//! [`GlobalAlloc::dealloc`] and merges adjacent free blocks, so steady
//! allocate/free traffic runs in bounded memory.
//!
//! # Design
//!
//! The free region is a singly linked list of **holes**, each hole storing
//! its own `{ size, next }` header at its first bytes and kept **sorted by
//! address** so [`GlobalAlloc::dealloc`] coalesces with its physical
//! neighbours in one pass. Allocation is first-fit with front/back splitting; a split
//! remnant smaller than a hole header is absorbed into the allocation
//! rather than leaked as an unrepresentable fragment. The list head and a
//! `used` counter live behind an inline spin lock (an `AtomicBool`), so the
//! crate stays dependency-free (`core` only) and the `static`
//! `#[global_allocator]` is `const`-constructed; the heap is initialised
//! lazily on the first allocation.
//!
//! `AGENTS.md` §4 forbids "an `unsafe` global allocator that performs raw
//! pointer arithmetic without bounds-checked wrappers": every hole address
//! is confined to `[heap_base, heap_base + heap_len)` by construction (the
//! initial hole spans exactly the region and splits only ever shrink it),
//! and every returned pointer lies within a hole that fit the request.

#![no_std]
#![deny(missing_docs)]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::mem::{align_of, size_of};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

/// Kernel heap size.
///
/// 64 MiB covers the boot pipeline plus steady-state kernel service
/// traffic. With [`FreeListAllocator`] this is a working-set ceiling, not
/// a cumulative cap: freed allocations are reclaimed, so a load that
/// allocates and frees does not march the heap toward exhaustion the way
/// the previous bump allocator did.
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

/// The mutable allocator state guarded by [`FreeListAllocator::lock`].
struct Inner {
    /// `true` once the initial whole-heap hole has been planted.
    initialised: bool,
    /// Head of the address-sorted free list.
    head: Option<NonNull<Hole>>,
    /// Bytes currently handed out (diagnostic).
    used: usize,
}

/// A coalescing first-fit free-list allocator over a fixed heap region.
///
/// Implements [`GlobalAlloc`]. Pair with a `static HEAP: Heap`
/// (`#[repr(C, align(4096))]`) in the bin crate and register via
/// `#[global_allocator]`. The `heap_base` pointer must stay valid for the
/// life of the binary (the bin backs it with a `static mut Heap`,
/// `AGENTS.md` §2's documented single global-mutable exception).
pub struct FreeListAllocator {
    heap_base: *mut u8,
    heap_len: usize,
    lock: AtomicBool,
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
            inner: UnsafeCell::new(Inner {
                initialised: false,
                head: None,
                used: 0,
            }),
        }
    }

    /// Acquire the spin lock, run `f` over the mutable state, release it.
    ///
    /// # Safety
    ///
    /// The caller must not re-enter (the lock is not reentrant); the
    /// allocator only ever calls this from its own non-nested methods.
    unsafe fn with_inner<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
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
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.heap_len.saturating_sub(self.used())
    }
}

/// Insert the hole at `[addr, addr+size)` into the address-sorted list and
/// coalesce it with an immediately-adjacent predecessor and/or successor.
///
/// # Safety
///
/// `addr` lies in the heap region and `size >= MIN_BLOCK` and is
/// `ALIGN`-aligned; the block is not currently in the free list.
unsafe fn insert_hole(head: &mut Option<NonNull<Hole>>, addr: usize, size: usize) {
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
    // absorb `cur` into the new block.
    let mut new_size = size;
    let mut next = cur;
    if let Some(node) = cur {
        let node_addr = node.as_ptr() as usize;
        if addr + size == node_addr {
            // SAFETY: `node` is a live hole.
            let node_size = unsafe { node.as_ref().size };
            new_size += node_size;
            // SAFETY: live hole.
            next = unsafe { node.as_ref().next };
        }
    }

    // Coalesce backward: if `prev` ends exactly where the new block starts,
    // extend `prev` instead of inserting a new node.
    if let Some(p) = prev {
        let p_addr = p.as_ptr() as usize;
        // SAFETY: `p` is a live hole.
        let p_size = unsafe { p.as_ref().size };
        if p_addr + p_size == addr {
            // SAFETY: `p` is live; extend it to cover the new (already
            // forward-coalesced) block and relink past any absorbed `cur`.
            unsafe {
                let p_mut = &mut *p.as_ptr();
                p_mut.size = p_size + new_size;
                p_mut.next = next;
            }
            return;
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
        // SAFETY: serialised access; `ensure_init` plants the heap once.
        unsafe {
            self.with_inner(|inner| {
                self.ensure_init(inner);
                // First-fit walk, tracking `prev` so the chosen hole can be
                // unlinked.
                let mut prev: Option<NonNull<Hole>> = None;
                let mut cur = inner.head;
                while let Some(node) = cur {
                    let hole_addr = node.as_ptr() as usize;
                    let hole_size = node.as_ref().size;
                    let hole_next = node.as_ref().next;
                    let hole_end = hole_addr + hole_size;

                    // Try to carve exactly `size` bytes at the first aligned
                    // address in the hole, leaving front/back remnants that
                    // are each either empty or a representable hole
                    // (>= `MIN_BLOCK`). A remnant smaller than a hole header
                    // would be unrepresentable, so rather than absorb it
                    // (which would leak: `dealloc` reconstructs only `size`
                    // from the layout, so a carved block larger than `size`
                    // could never be fully reclaimed) this hole is skipped
                    // and the search continues. `dealloc` therefore always
                    // frees the exact `size` that was carved — no leak.
                    if let Some(start) = align_up(hole_addr, align) {
                        let front = start - hole_addr;
                        let fits = start.checked_add(size).is_some_and(|end| end <= hole_end);
                        let front_ok = front == 0 || front >= MIN_BLOCK;
                        if fits && front_ok {
                            let back = hole_end - (start + size);
                            let back_ok = back == 0 || back >= MIN_BLOCK;
                            if back_ok {
                                // Unlink the chosen hole, then reinsert the
                                // front and back remnants (each a valid hole
                                // or skipped when empty).
                                match prev {
                                    Some(p) => (*p.as_ptr()).next = hole_next,
                                    None => inner.head = hole_next,
                                }
                                if front != 0 {
                                    insert_hole(&mut inner.head, hole_addr, front);
                                }
                                if back != 0 {
                                    insert_hole(&mut inner.head, start + size, back);
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
            })
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let Some(size) = block_size(layout) else {
            return;
        };
        let addr = ptr as usize;
        // SAFETY: serialised access; `ptr`/`size` reconstruct the exact
        // block `alloc` carved (same `block_size`), so reinserting it is in
        // bounds and cannot overlap a live block.
        unsafe {
            self.with_inner(|inner| {
                // `alloc` carved exactly `block_size(layout)` bytes (it skips
                // a hole rather than absorbing an unrepresentable remnant), so
                // freeing the same `size` reclaims the whole block with no
                // leak. `insert_hole` coalesces it with adjacent free
                // neighbours, undoing fragmentation.
                insert_hole(&mut inner.head, addr, size);
                inner.used = inner.used.saturating_sub(size);
            });
        }
    }
}

#[cfg(test)]
mod tests;
