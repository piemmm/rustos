# rustos-bumpalloc

Shared `no_std` forward-only bump allocator for freestanding RustOS boot
heaps.

Every freestanding boot binary (the production `rustos-kernel`, every
`tests/integration/*` QEMU bin, and every architecture port's boot
harness) registers a `BumpAllocator` over a per-binary `Heap` static as
its `#[global_allocator]`. Defining the allocator once here satisfies
`AGENTS.md` §2.2 (no duplication) and §6 (shared code lives in `lib/`).

See the crate-level rustdoc for the documented limits (never frees,
hard upper bound, thread-safe, bounds-checked).

## Stability tier

`stable` — the public surface (`BumpAllocator`, `Heap`, `HEAP_BYTES`) is
the boot-heap contract consumed across the kernel and test trees. It is
`no_std` and depends only on `core`.
