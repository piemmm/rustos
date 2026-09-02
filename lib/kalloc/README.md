# tairix-kalloc

Shared `no_std` freeing kernel heap allocator for freestanding TAIRiX
images.

Every freestanding boot binary (the production `tairix-kernel`, every
`tests/integration/*` QEMU bin, and every architecture port's boot
harness) registers a `FreeListAllocator` over a per-binary `Heap` static
as its `#[global_allocator]`. Defining the allocator once here satisfies
`AGENTS.md` §2.2 (no duplication) and §6 (shared code lives in `lib/`).

The allocator has **two tiers behind one `GlobalAlloc`**, both under the
same lock.

A request up to the page granule is served by the **slab** tier:
per-size-class pages, the free list threaded through the free objects
themselves, and each page's own bookkeeping in its first object slot. There
is no per-object header and no rounding to a block boundary, so a page-sized
allocation — the kernel's dominant traffic, the filesystem cache's chunk
being exactly `PAGE_SIZE` — occupies exactly one frame. Classes are powers
of two, so an object placed at a multiple of its class inside a page-aligned
page is aligned to that class and alignment costs nothing. Which tier serves
a request is a pure function of its `Layout`, so `alloc` and `dealloc`
always agree with no side table; a routing decision that consulted installed
state would free a pre-install object down the wrong tier.

The sub-granule classes stop at a *derived* ceiling rather than running to
the granule: the descriptor costs a page one slot, so past a point a slab
page charges more per object than a byte-tier block of the same width would
(at half the granule it would charge double), and those widths go to the
byte tier instead. The granule class is exempt — it carries no descriptor at
all, so it both undercuts the byte tier and fits one frame exactly.

Anything larger, or in that middle band, is served by the **byte-granular**
tier: a coalescing
segregated-fit allocator over in-band boundary tags, where free blocks are
threaded onto per-size-class lists selected by a pair of bitmap scans and
every block records its physical predecessor so coalescing reaches each
neighbour directly. `alloc`, `dealloc`, and returning a drained region are
therefore **O(1)** — no list is ever walked.

Steady allocate/free traffic runs in bounded memory and exhaustion is a
`null` return, never a panic (`AGENTS.md` §4). Before it refuses, the heap
reclaims the one page each slab class keeps back as hysteresis. See the
crate-level rustdoc for the design and bounds-checking invariants.

The shape is load-bearing, not incidental. The predecessor walked one
address-sorted hole list on both `alloc` and `dealloc`, plus the whole
region list on *every* `dealloc`; because a grown region carries a header
separator that free space may not coalesce across, the hole count could
never drop below the region count. Once the heap grew past its bootstrap
arena, every allocation in the kernel paid a cost linear in how much the
heap had ever grown — measured as a 5x slowdown over 26 MB of file reads,
still climbing, on a read path whose filesystem driver does 370 MB/s.
`per_operation_node_reach_does_not_grow_with_the_heap` pins the fix.

Its lock is **interrupt-safe**: TAIRiX takes interrupts while in-kernel
code runs, so an interrupt handler can fire on a CPU already mid-allocation
holding the lock; without masking, an ISR that allocated would spin forever
on the lock its own interrupted mainline holds — a single-CPU self-deadlock
(`AGENTS.md` §23.2). The masking primitive is architecture-specific, so the
freestanding bin installs it once at boot via `install_irq_control`, before
interrupts are ever enabled; the hosted test build and the interrupt-free
`wasm32` port install nothing and the lock does not mask (that window is
single-CPU with interrupts already masked, so no ISR can reenter).

The lock is the shared `tairix_sync::IrqSafeSpinLock` over an
`InterruptControl` bound to those hooks, not a second spinlock written here.
That also puts the heap lock on the lockup watchdog's lock-site record
(`plans/WATCHDOG.md`): it is the one lock every subsystem descends into, so a
core wedged inside it must be named as such rather than reported against
whichever outer lock it happened to be holding.

The hooks mask the *calling* CPU, so they belong to the machine rather than
to any one heap: they are crate-global, and one install covers every core
and every `FreeListAllocator` the binary holds. Binding them per instance
would leave a heap the install site was never told about — every
freestanding test bin declares its own `#[global_allocator]` and publishes
it to no registry — silently spinning on a plain lock.

## Stability tier

`stable` — the public surface (`FreeListAllocator`, `Heap`, `HEAP_BYTES`,
`HeapSource`, `install_irq_control`) is the kernel-heap contract consumed
across the kernel and test trees. It is `no_std`, allocates nothing outside
the arena it is given, and depends only on `core` and the ABI crate's page
granule (`tairix_abi::PAGE_SIZE`), which itself has no dependencies.
