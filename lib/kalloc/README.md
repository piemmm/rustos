# tairix-kalloc

Shared `no_std` freeing kernel heap allocator for freestanding TAIRiX
images.

Every freestanding boot binary (the production `tairix-kernel`, every
`tests/integration/*` QEMU bin, and every architecture port's boot
harness) registers a `FreeListAllocator` over a per-binary `Heap` static
as its `#[global_allocator]`. Defining the allocator once here satisfies
`AGENTS.md` §2.2 (no duplication) and §6 (shared code lives in `lib/`).

The allocator is a coalescing **segregated-fit** allocator over in-band
boundary tags: free blocks are threaded onto per-size-class lists selected
by a pair of bitmap scans, and every block records its physical predecessor
so coalescing reaches each neighbour directly. `alloc`, `dealloc`, and
returning a drained region are therefore **O(1)** — no list is ever walked.
Steady allocate/free traffic runs in bounded memory and exhaustion is a
`null` return, never a panic (`AGENTS.md` §4). See the crate-level rustdoc
for the design and bounds-checking invariants.

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

## Stability tier

`stable` — the public surface (`FreeListAllocator`, `Heap`, `HEAP_BYTES`,
`install_irq_control`) is the kernel-heap contract consumed across the
kernel and test trees. It is `no_std` and depends only on `core`.
