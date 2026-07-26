# tairix-kalloc

Shared `no_std` freeing kernel heap allocator for freestanding TAIRiX
images.

Every freestanding boot binary (the production `tairix-kernel`, every
`tests/integration/*` QEMU bin, and every architecture port's boot
harness) registers a `FreeListAllocator` over a per-binary `Heap` static
as its `#[global_allocator]`. Defining the allocator once here satisfies
`AGENTS.md` §2.2 (no duplication) and §6 (shared code lives in `lib/`).

The allocator is a coalescing first-fit free list: it reclaims on
`GlobalAlloc::dealloc` and merges adjacent free blocks, so steady
allocate/free traffic runs in bounded memory and exhaustion is a
`null` return, never a panic (`AGENTS.md` §4). See the crate-level
rustdoc for the design and bounds-checking invariants.

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
