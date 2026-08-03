# Memory subsystem (`kernel/mem`)

Architecture-neutral, host-testable physical and virtual memory
management. Delivered by Stage 2.2 of `PLAN.md`. The architecture
crates (`kernel/arch/*`) supply the only piece this crate
does not implement: the real page-table writer behind the Arch HAL
page-table surface
([`PageTable`](#3-virtual-memory--page-table-operations)).

## Layered design

```text
                ┌──────────────────────────────────────────────┐
                │   sensitive — zero-on-free for credentials   │
                │   (`alloc_sensitive` / `free_sensitive`)     │
                ├──────────────────────────────────────────────┤
                │   slab — fixed-size objects + guard pages    │
                ├──────────────────────────────────────────────┤
                │   vmm — `AddressSpace<P: PageTable>`         │
                ├──────────────────────────────────────────────┤
                │   frame — buddy + bitmap physical allocator  │
                └──────────────────────────────────────────────┘
                          │
                  `BootMemoryMap` from `kernel/arch/*`
```

Every layer above depends only on the layer immediately below it; the
trait that crosses the architecture boundary lives in
[vmm](#3-virtual-memory--page-table-operations).

## 1. Physical frame allocator (`frame`)

Hybrid **buddy + bitmap**:

- A single bitmap covers the whole physical address range described by
  the [`BootMemoryMap`]. `0 = free`, `1 = allocated, reserved, or
  non-existent`. The bitmap is the source of truth for ownership, so
  every double-free or stray-free is detected and reported as
  `AllocError::InvariantViolation`.
- A `BTreeSet<usize>` per buddy order tracks the starting frame
  indices of free blocks at that order. Splits push two half-blocks
  down one order; merges pop a buddy at the same order and push the
  parent up one order. Merging consults the bitmap so it never
  reaches across a reserved region.

The allocator never panics on OOM: `alloc` / `alloc_order` return
`AllocError::OutOfMemory`. The constructor refuses overlapping or
malformed boot maps.

**Bootloader handoff.** The arch crates synthesise a
[`BootMemoryMap`] from whatever protocol the platform uses (multiboot2,
PVH, UEFI, DTB, WASM) and hand it to `FrameAllocator::new`. Reserved
regions are merged into the bitmap as "used" so they can never be
handed out; usable regions are rounded *inward* to whole-frame
boundaries.

**The zero page is never enrolled**, even when firmware reports it
usable (the PC low-BIOS region starts at physical 0): under an identity
direct map its translation is the null pointer, which no
`NonNull`-based consumer ([`FrameTableSource`], the DMA pool, an MMIO
window) can represent. Because the buddy lists hand out the lowest free
index first, a frame 0 that a consumer draws, cannot use, and returns
would be re-issued to every later request — wedging allocation
permanently while `free_frames` still reports plenty. It stays marked
reserved, exactly like firmware-reserved RAM, and is excluded from
`usable_frames`.

## 1a. Early-boot RAM self-test (`ramtest`)

Before the frame allocator hands out a single frame, the kernel proves the
installed RAM actually stores what is written to it. `kernel/mem`'s
architecture-neutral [`ramtest`] engine walks every **usable** region of the
[`BootMemoryMap`] through the port's direct [`PhysMap`] (§3b) and, on the
first mismatch, returns a `RamFault` naming the physical address. The
kernel-core half (`tairix_kernel_core::memtest`) drives it during the
`Phase::Mem` boot step and shows the result on the boot console.

It runs *before* `FrameAllocator::new` on purpose: usable regions are, by
definition, RAM nothing is using yet (the kernel image, its stack, the boot
page tables and the device tree all sit in reserved regions), so the test may
write to them freely. It is a **boot sanity check, not an exhaustive march
test** — it must finish in a couple of seconds on many gigabytes — so it does
not touch every byte and does not scrub whole regions. The few cells it does
write are restored to zero, and the allocator's consumers zero their own
frames before use anyway (the page-table frame source hands back
zero-initialised frames, anonymous and DMA memory is zeroed on map, the user
stack is zeroed on spawn), so a clean frame is each consumer's own guarantee.

For that invariant to hold, everything the kernel keeps reading *after* the
hand-off must be reserved out of the usable window — the flattened device
tree above all. Firmware on both the aarch64 (`virt`/Pi) and riscv64 (`virt`)
boards lands the DTB blob in the RAM window the map would otherwise call
usable, so each boot path reserves its whole frames through the one shared
`reserve_blob_frames` routine (`tairix-kernel::mem_map`). Without it the
self-test would zero the live tree and the later root-storage bind and device
discovery would find it unreadable — the concrete failure this reservation
prevents.

Two complementary passes (after Michael Barr, *"Software-Based Memory
Testing"*, 2000) are applied to each progress-sized window:

- a whole-window **address-line** test that stamps offset 0 and every
  power-of-two offset with its own marker and reads them all back, catching
  an address bit that is stuck or shorted (a write that lands in the wrong
  cell) in `O(log n)` accesses — the textbook quick address-line walk,
  covering the address decode across the full span, and
- a sampling **device** test that proves **one word per 4 KiB** holds both a
  `1` and a `0` (writing `0xAA…` then its complement `0x55…` and reading each
  back), catching a stuck data bit, row, column, or bank.

The device test deliberately does **not** touch every cell — that is the
`O(all bytes)` write-back-and-verify cost this boot check exists to avoid.
The faults that occur in practice — a stuck cell, row, column, or bank — span
far more than 4 KiB and are hit by the sample many times over; the coverage
traded away is a lone single-cell fault that falls between two samples, which
is `memtest86`'s job, not the boot path's. Only the address pass's handful of
offsets and the device pass's periodic sample (one word per page) are ever
written, read, or flushed, so the whole test costs
`O(usable_bytes / sample_interval)` cache-line accesses rather than
`O(usable_bytes)` — the difference between a few seconds and many minutes on a
large machine, quicker still on real silicon than under QEMU/TCG.

**Defeating the cache.** Reads and writes go through the *cacheable* direct
map, so a naive write-then-read could be served from the CPU cache and never
reach the DRAM cell under test. Before reading a sampled cell back the engine
flushes just that cell's cache line via [`PhysMap::clean_invalidate`], which
writes the dirty line back to DRAM and drops the cached copy, so the read
observes what the DRAM actually holds — a per-word flush, never a
whole-window one that would pay to write back RAM the test never reads. On an
I/O-coherent host or the `SimPhysMap` test double the flush is a documented
no-op and the model is identical, which is what makes the engine fully
host-testable (healthy RAM, an injected stuck-low/stuck-high bit, and a
shorted address line are all exercised on the host).

**On the console.** The `TAIRiX <version> <RAM>MiB` identity line is drawn
here — the figure starts at zero and climbs to the installed total in
**yellow** while the test is still running (RAM being verified but not yet
proven), then, once every region has passed, settles on the installed figure
redrawn in **light green**. The engine reports progress every couple of MiB,
but the driver coalesces those into at most a few hundred in-place redraws
spread across installed RAM, so the counter animates just as smoothly on
256 MiB as on 64 GiB without thousands of framebuffer blits dominating the
test's run time. It is drawn on *every* boot console — the framebuffer screen and the
serial line alike — so a headless boot shows the same line a graphical one
does; when a port makes the framebuffer its sole console and keeps the UART
for the debug log alone (the aarch64 `VIDEO_ONLY_CONSOLES` case), the line
lands on that one console, i.e. the screen. Userland `init` adds only the
processor line beneath it and never repeats the version or RAM figure. A
detected fault redraws the number in
**red** as the MiB offset of the failing location, prints a diagnostic
naming the physical address and the mismatch, and **halts the boot**: TAIRiX
never runs on memory it could not trust (fail closed, fail loud). A port with
no direct physical map (the host / `wasm32` environment) cannot reach
physical RAM and skips the test rather than faking a pass.

## 2. Slab allocator with guard pages

`AGENTS.md` §4 mandates guard pages around kernel slabs. The slab's
backing buffer is laid out as

```
[ GUARD | data: object_size × slot_count | GUARD ]
```

with each guard region being exactly one architectural page.

- **On hardware:** the guards are unmapped virtual pages. An overrun
  faults loudly.
- **On the host (`cargo test`):** the guards are filled with `0xCC`
  and validated on every alloc/free and on demand via
  `Slab::check_guards`. A pattern mismatch surfaces as
  `SlabError::GuardViolation` — the same error channel as the
  hardware fault, so callers and tests are written once.

Each slab also zero-fills a slot on free, preventing leftover bytes
from leaking into the next caller. This is a cheap defence-in-depth
measure; for **credentials, keys, and capability tokens** the caller
must use the sensitive-region API below.

Zero-on-free is an *enforced* invariant, not an incidental one
(`AGENTS.md` §3.3, CWE-908/200): because `free` wipes every byte and a
fresh slab starts zeroed, a free slot is always all-zero. `Slab::alloc`
verifies this before reusing a slot and refuses one whose contents are
non-zero with `SlabError::DirtySlot`, so a skipped or corrupted wipe
fails closed rather than leaking the previous occupant's bytes to the
next caller.

## 3. Virtual memory & page-table operations

[`AddressSpace<P: PageTable>`] is the per-process virtual address
space. It owns a page-table object and serialises `map` / `unmap` /
`translate` through it.

The architecture boundary is the Arch HAL page-table surface, not a
`kernel/mem`-local trait: [`PageTable`] is merely the bound alias
`tairix_arch_api::mmu::AddressSpace + tairix_arch_api::tlb::TlbShootdown`,
so `kernel/mem` names only the HAL vocabulary (`AGENTS.md` §2.2,
`plans/WIRING.md` Stage W5b-2). The HAL surface the façade drives:

| Method | Description |
| --- | --- |
| `map_page(vaddr, paddr, flags)` | Install a 4 KiB translation. |
| `unmap(vaddr)` | Tear it down, return the physical page. |
| `translate(vaddr)` | Read-only walk. Returns `Option`, never errors. |
| `root_phys()` | Physical address of the root translation table. |
| `flush_page(vaddr)` | Per-CPU TLB invalidation (no-op on host). |
| `flush_range(vaddr, pages)` | Contiguous-range invalidation; zero pages is a no-op. |

`flush_page` and `flush_range` invalidate only the *calling* CPU unless a port's
architecture makes a broadcast operation cheaper and equally sound (aarch64
uses one inner-shareable all-translation invalidation for a range). The
`AddressSpace::map_contiguous` façade validates a complete virtual/physical
extent before editing it, rolls every leaf back on a backend refusal, and then
issues one range synchronization. Shared-memory windows use this operation per
physically contiguous buddy chunk, so a display frame containing hundreds of
4 KiB pages does not pay hundreds of serial TLB barrier sequences.

The system-wide
counterpart — invalidating a stale translation on every other online CPU
after a shared mapping is torn down — is a sibling Arch HAL slice,
`tairix_arch_api::xtlb::CrossCpuTlbShootdown`, implemented on each port's
`SchedulerArch` handle rather than on the page-table object (see
[the modularity page](./modularity.md) and `plans/WIRING.md` Stage W13).

The façade bridges its own `Page` / `Frame` / `MapFlags` currency to the
HAL's `u64` / `PageFlags` at the boundary. Each arch crate's `paging`
`AddressSpace` implements the HAL traits directly. To keep `kernel/mem`
fully host-testable the crate ships a `HostPageTable` test double behind
`#[cfg(test)]`: a `BTreeMap`-backed implementation of the same HAL
traits that, additionally, enforces W^X (rejects `WRITE | EXEC`
mappings) so the security default is exercised in tests.

`MapFlags` is a small `bitflags`-style set:
`READ | WRITE | EXEC | USER | NO_CACHE`. Architecture code translates
these into native page-table bits during Stage 3.

**Backing a port's page tables with the frame allocator
(`FrameTableSource`).** A port's `AddressSpace` is built from 4 KiB
page-table frames it draws through the Arch HAL `PageTableFrames` seam
(`tairix_arch_api::frames`). The boot/bootstrap source is the static
`PageTablePool` each port ships; the production source is
[`FrameTableSource`], which draws a physical frame from the
`FrameAllocator`, maps it to a CPU view through the direct
[`PhysMap`] (§3b), zeroes it, and hands the port a `TableFrame`
(physical address + `'static` entry view). A frame outside the direct
map is returned to the allocator and the request fails closed
(`AGENTS.md` §2.9), never synthesising a pointer. This keeps the §17.4
one-way edge intact — `kernel/arch/*` names only the HAL trait, never
`kernel/mem` — while a real per-process address space's tables come from
ordinary reclaimable RAM rather than a fixed `.bss` pool
(`plans/WIRING.md` Stage W5b-3). Host tests run the HAL
`frames::conformance` suite over `FrameTableSource` and assert each
table is drawn from the allocator, zeroed, and distinct.

The runtime `spawn` producers (aarch64 and x86_64) are the first
production consumers: each builds a spawned child's page tables over a
boot-cached `FrameTableSource` rather than a fixed `.bss` `PageTablePool`
reserve, so the number of processes that can be spawned scales with
discovered RAM and grows on demand instead of being a hard `const`
ceiling (`AGENTS.md` §24.1; see
[the resource-limits page](./resource-limits.md)). The source is shared
across CPUs, so its direct-map handle is `Sync`.

**Reclaiming a dead process's whole footprint (`plans/APPS.md` I2).**
The seam is symmetric: `PageTableFrames::free_table` is the teardown
half, and a task's exit returns everything it owned. The retained
per-task `LiveSpace` (the object the `mem_map` / `mmio_map` / `dma_alloc`
syscalls mutate) is owned by the task's kernel-thread control block and
dropped when the scheduler reaps the exited task; its `Drop` (1) drains
every live DMA carve (zero-on-free, frames back to the allocator),
(2) walks every remaining tracked mapping — a page inside the
device-window or shared-memory window is only *unmapped* (its frames
belong to a device or to the shared-region registry), while every other
page (image segments, user stack, startup block, anonymous heap) is
unmapped, its frame **zeroed** through the direct map so a dead
process's bytes are never recycled readable, and freed — and (3) hands
the page-table hierarchy itself back post-order (children before
parents, the root last) through the one shared
`tairix_arch_api::frames::reclaim_hierarchy` walk each port's
`reclaim_table_frames` drives, so every stage-1 table frame returns to
the `FrameTableSource` for reuse. Teardown is SMP-safe by an invariant
the dispatcher maintains: after every switch-back from a user task the
CPU re-parks its translation on the permanent boot root (published
set-once at boot; the port's `park_kernel_root`), so a user root is
active on a CPU only while its task runs there and a dead root can never
be freed under a live walk (the port's reclaim additionally re-parks
defensively if the calling CPU still holds the dying root, and retires
the frames unreclaimed rather than dismantling an active translation —
fail closed). Host tests pin the whole discipline: the `LiveSpace` drop
test and the `spawn_image` spawn/exit-cycle test assert
`free_frames` returns exactly to its pre-spawn value (registry-owned
shared frames excluded), and the aarch64/riscv64 paging tests assert the
walk returns every drawn table exactly once, root last, leaves never.

## 3a. User-memory copy (`uaccess`)

A syscall handler is handed a raw user pointer (`ptr`, `len`) and must
move bytes to or from that buffer without ever trusting it — the
`copy_from_user` / `copy_to_user` boundary (`AGENTS.md` §5.4,
`tests/SECURITY.md` §5). The [`uaccess`] module is the
architecture-neutral half of that boundary: [`copy_in`] reads from the
caller's address space into a kernel slice, [`copy_out`] writes a kernel
slice into it. Both take the address space as a `&dyn UserAddressSpace`
(the read-only `translate` view, §3b) and compose the two layers above —
that one `translate` operation and the `PhysMap` direct map — so the
copy path walks any task's page-table backend behind a single
non-generic call site, with one validated traversal and never a second
pointer-walk implementation (`AGENTS.md` §2.2).

The copy walks the user range **one page at a time** (user memory is
contiguous in the virtual address space but its frames need not be
contiguous in physical RAM): for each `[page_start, page_start +
PAGE_SIZE)` window the range touches it `translate`s the page to its
`(Frame, MapFlags)`, turns the in-page physical span into a CPU pointer
through the `PhysMap`, and moves only the bytes of the caller's buffer
that fall inside that page. The first page may begin at a non-zero
offset and the last may end before the page boundary.

Every page is checked, fail-closed, before a byte moves:

| Reason | `UaccessError` |
| --- | --- |
| Null base on a non-empty copy | `Null` |
| `ptr + len` overflows the address space | `LengthOverflow` |
| A page in range is unmapped | `NotMapped` |
| A page in range is not user-accessible (kernel-pointer confusion) | `NotUser` |
| `copy_in` page lacks `READ` | `NotReadable` |
| `copy_out` page lacks `WRITE` (read-only / executable — the §19.2 W^X guard) | `NotWritable` |
| Backing frame outside the direct map | `PhysUnmapped` |
| A hardware fault interrupted the byte move (window fix-up) | `Faulted` |

A page missing `USER` is rejected **before** a missing data permission,
so a kernel-pointer-confusion attempt is never downgraded to a mere "not
readable". A zero-length copy touches no memory and succeeds for any
base. The two entry points carry one encapsulated `unsafe` block each
(the in-page span move), with a `// SAFETY:` rationale and full
host-test coverage (`AGENTS.md` §2.10): cross-page, mid-page-offset,
round-trip, and every fail-closed branch are exercised with
`HostPageTable` + `SimPhysMap`.

**The byte move itself runs under the architecture port's hardware
fault window** (`tairix_arch_api::uaccess`, the `copy_from_user`
page-fault fix-up of `tests/SECURITY.md` §5). Each MMU port publishes a
fault-windowed span-copy routine into a set-once Arch HAL slot at its
trap-vector install chokepoint (riscv64 `trap::install_trap_vector`,
aarch64 `exceptions::init_vectors`, x86_64 the production boot beside
its dedicated `#PF`-entry install): the copy's loads/stores sit inside
an exported `[window_begin, window_end)` instruction range, and the
port's trap handler, on a **kernel-mode** data fault whose saved PC lies
inside that range, rewrites the frame's saved PC (`sepc` / the frame
ELR slot / the interrupt frame `RIP`) to the window's fix-up — which
returns "faulted" to the caller, surfaced as `UaccessError::Faulted`
and collapsed onto the same `BadAddress` every other copy fault maps
to. The validated walk makes an in-window fault unreachable under
correct operation; the window is the backstop for a violated proof (a
kernel defect, a corrupted table), turning it into a failed syscall
instead of a halted machine. With no routine installed (the host test
build, `wasm32` — no synchronous-fault source) the seam is a plain
copy. The copy shape is per-port: `rep movsb` on x86_64, a 16-byte
`ldp`/`stp` loop on aarch64, and an alignment-safe doubleword loop on
riscv64 (a misaligned `ld`/`sd` may trap on real silicon, so the window
only ever absorbs page faults). Three QEMU verticals
(`tests/integration/uaccess_fault_qemu_{riscv64,aarch64,x86_64}`) take
real in-window faults on both the read and write side through the one
shared `tairix_arch_api::uaccess::conformance` checks and PASS only
when each surfaces as an error with the CPU still running.

This module is the foundational primitive of the staged user-memory
work: the per-task address-space registry, the syscall wiring of
`ipc_send` / `ipc_recv` / `cap_delegate` / `random_get`, and the
per-architecture page-fault fix-up above all build on it (see
`PLAN.md`).

[`uaccess`]: ../../tairix_kernel_mem/uaccess/index.html
[`copy_in`]: ../../tairix_kernel_mem/uaccess/fn.copy_in.html
[`copy_out`]: ../../tairix_kernel_mem/uaccess/fn.copy_out.html

## 3b. Per-task address-space registry

`copy_in` / `copy_out` take a `&dyn UserAddressSpace` (so the call site
names no concrete page-table backend), but a syscall handler only knows
the caller's `TaskId`. The bridge is the **address-space
registry** (`kernel/core`, `aspace` module): a
`BTreeMap<TaskId, (address space, PhysMap)>` that the syscall path reads
to resolve the calling task to the pair the copy walk consumes. It is
composed into `KernelState` next to the capability and IPC registries,
wrapped in the same reader-preferring `RwLock`, so the hot path takes
only a shared lock (`AGENTS.md` §2.1).

`AddressSpace<P>` is generic over its page-table backend `P`, so the
kernel cannot key one map on a single concrete `AddressSpace<P>` —
different tasks may run on different architecture page tables. The
registry therefore stores each entry behind [`UserAddressSpace`], an
object-safe, **read-only** view that exposes only `translate` (a blanket
impl forwards it to `AddressSpace::translate`, so there is one
translation definition, not two — `AGENTS.md` §2.2). Exposing only
`translate` keeps the copy path from ever mutating a caller's mappings;
map / unmap stay behind `AddressSpace`'s own accounted API (`AGENTS.md`
§2.4). The physical map is erased the same way the rest of the kernel
already erases it (`&dyn PhysMap`), so the registry is one concrete,
non-generic type.

Lifecycle is fail-closed: an entry is registered when a task's image is
mapped and withdrawn when it exits. Registering an id that is already
present is **refused** (`AspaceError::AlreadyPresent`) rather than
silently replacing a live mapping, withdrawal is idempotent, and
resolving an unknown id yields `None`. The registry is a pure data
structure with no audit sink of its own — the spawner and the `exit`
handler that drive the lifecycle own the security-relevant logging,
exactly as the syscall dispatcher (not the IPC `PortRegistry`) audits
endpoint lookups.

The registry is reached from a syscall handler through
`KernelSyscallHandlers::with_caller_aspace(caller, f)` (increment C of
the staged copy path): it is threaded into the
`KernelDispatchHook` / `KernelSyscallHandlers` borrows next to `caps`
and `ipc`, takes a read guard, resolves `caller.task_id`, and runs `f`
with the borrowed `(&dyn UserAddressSpace, &dyn PhysMap)` pair — the
guard living exactly as long as the borrowed references — failing
closed to `None` when the caller has no registered space. Keeping the
accessor in `kernel/core` is deliberate: the decoupled dispatcher
(`kernel/syscall`) reaches user memory without ever depending on
`kernel/mem` (`AGENTS.md` §17.4). The handler-side copies that consume
it (`ipc_send` / `ipc_recv` / `cap_delegate` / `random_get` through
`copy_in` / `copy_out`) are increment D, now **fully landed** (D.1–D.4;
`random_get` draws from the `tairix_rng::OutputReserve` composed into
`KernelState` and copies it out, see `PLAN.md`). Because the
copy entry points already accept `&dyn UserAddressSpace`, the pair that
`with_caller_aspace` yields drives them directly, with no concrete
`AddressSpace<P>` re-erasure at the boundary.

[`UserAddressSpace`]: ../../tairix_kernel_mem/vmm/trait.UserAddressSpace.html

## 4. Sensitive-region API

`alloc_sensitive(len) -> Result<SensitiveBuffer, AllocError>` hands
back a fixed-size byte buffer that **zeroes itself on drop**, using
the audited [`zeroize`](https://crates.io/crates/zeroize) crate (no
hand-rolled crypto per `AGENTS.md` §6). `free_sensitive(buf)` is a
named drop equivalent provided for documentation symmetry.

`SensitiveBuffer` is fixed-size (`Box<[u8]>`, not `Vec<u8>`) to avoid
silent reallocations that would leak a secret into the old
allocation. Its `Debug` impl deliberately redacts the contents.

## 5. DMA buffers

User-space drivers (`drivers/storage/virtio_blk`, `drivers/network/virtio_net`,
future NVMe / GPU bus-master devices) need page-aligned, contiguous-by-physical
buffers that a device can address directly. The
[`DmaPool<P>`][DmaPool] ships that facility,
composing the layers above:

- **Physical contiguity** — frames are taken from the buddy allocator at a
  single buddy order; the buffer is therefore physically contiguous up to
  `MAX_ORDER` pages.
- **Per-process virtual window** — the pool owns a slice of one process's
  `AddressSpace<P>`. Each allocation maps `data_pages` consecutive pages
  with `READ | WRITE | USER`; no `EXEC`, no global sharing.
- **Guard pages** — every allocation is bracketed by one *unmapped* virtual
  page on each side, so an overrun faults on the MMU rather than reaching a
  neighbouring allocation.
- **CPU access via the direct map** — the bytes the driver reads/writes are
  the buffer's *physical frames*, reached through the kernel direct physical
  map (`PhysMap`): `bytes` / `bytes_mut` / `slot_base` translate the buffer's
  `phys` into a pointer. The CPU therefore sees exactly the frames the device
  DMAs to — there is no disconnected copy. Production wires a `DirectPhysMap`
  (the boot identity map over low physical memory); host tests wire a
  `SimPhysMap` standing in for physical RAM.
- **Zero-on-free** — every byte of the data region is wiped with
  [`zeroize`](https://crates.io/crates/zeroize) before the frames return to
  the buddy allocator. A later allocation that lands on the same slot sees
  zeros; a forensic read of free physical memory cannot recover the
  credentials, keys, or capability tokens the buffer once held
  (`AGENTS.md` §4).
- **Bounded failure** — `alloc` / `free` return `Result<_, DmaError>`. No
  panic on resource exhaustion, no `expect` on hot paths, no `unsafe` leaks
  across the crate boundary. Allocation requests larger than `MAX_ORDER`
  return `DmaError::SizeUnsupported`; exhaustion of either the virtual
  window or the frame allocator returns `DmaError::Alloc(OutOfMemory)`.

```text
[ guard | data_0 | data_1 | … | data_{n-1} | guard ]
   |       └────────── mapped (R/W/U) ──────────┘   |
   └──────────────── unmapped (fault) ──────────────┘
```

The data frames are reached by the CPU through the direct physical map
(`phys`), keyed on each buffer's `phys` address, so the driver's view
and the device's view are the same memory.

The pool itself is **capability-agnostic**: it does not consult the calling
task's capability set. The capability gate is the companion module
`kernel/sec::dma`, whose `alloc_dma` / `free_dma` entry points verify
[`CapabilityId::MEM_DMA`][CapabilityId::MEM_DMA]
before dispatching to the pool, and emit
[`AuditEvent::DmaAllocated`] / [`AuditEvent::DmaAllocDenied`] records on
every decision (IDs 1030 / 1031, see [Security audit catalogue](./security.md)).
A future syscall wrapper maps gate failures to `Errno` via
`DmaGateError::as_errno`:

| Gate error | `Errno` |
| --- | --- |
| `CapabilityMissing` | `PermissionDenied` |
| `Pool(ZeroSize)` | `BufferTooSmall` |
| `Pool(Alloc)` / `Pool(SizeUnsupported)` | `LengthOutOfRange` |
| Other internal pool failures | `OutOfRange` |

[`AuditEvent::DmaAllocated`]: ../../tairix_kernel_sec/enum.AuditEvent.html#variant.DmaAllocated
[`AuditEvent::DmaAllocDenied`]: ../../tairix_kernel_sec/enum.AuditEvent.html#variant.DmaAllocDenied
[DmaPool]: ../../tairix_kernel_mem/dma/struct.DmaPool.html
[DmaWindowMap]: ../../tairix_kernel_mem/dma/struct.DmaWindowMap.html
[CapabilityId::MEM_DMA]: ../../tairix_abi/capability/struct.CapabilityId.html#associatedconstant.MEM_DMA

The guarded carve itself lives in the borrowed-space
[`DmaWindowMap`][DmaWindowMap] (its virtual-window base, slot bitmap, and
live-allocation records); [`DmaPool`][DmaPool] is the thin owning adapter
over a space it owns outright, exactly as `MmioMap` wraps `MmioWindowMap`
(§5.2) — there is one carve definition (`AGENTS.md` §2.2). The retained
per-task live address space (`LiveSpace`, §7e, the `dma_alloc` syscall
path) drives the *same* `DmaWindowMap` against the
space it owns and lends, adding an `addr_limit` bound (the granted device
DMA constraint, §18.3): a contiguous block that would reach at or above the
limit is returned to the allocator and the carve refused
(`DmaError::AddrLimitExceeded`). `LiveSpace` reclaims (zeroes and frees)
every live DMA block when it is dropped on task exit, so a driver's exit
leaks no frames and leaves no secret-bearing buffer recoverable
(`AGENTS.md` §4).

### 5.1 Slab hand-off to user-space drivers

The user-space virtio driver crates carry an owned
`DmaSlab { phys, ptr: NonNull<u8>, len, pool_id, slot, /* erased
free shim */ }` rather than borrowing the pool on every accessor
(Stage 4.D Item 0a). The pool exposes a single companion accessor,
`slot_base`, which takes a `&DmaBuffer` and returns
`Result<NonNull<u8>, DmaError>`, handing out the base pointer of the
buffer's data slots. The disjointness witness is the pool's slot
bitmap (one slot ↔ one
allocation); the slab carries `(pool_id, slot, len)` so its drop
can invoke a type-erased free shim that returns the slot to the
pool. See [Virtio transport — DMA ownership model](../drivers/virtio.md#dma-ownership-model)
for the consumer-side view. The kernel-side wiring of a
`KernelVirtioHost` that builds slabs from `alloc_dma` is the
subject of Stage 4.D Item 0.

### 5.2 MMIO register-window mapper

Device drivers also need their *register block* mapped — a PCI memory
BAR or a virtio-MMIO transport slot. The guarded-mapping mechanism is
`kernel/mem::mmio::MmioWindowMap`: the per-task bookkeeping (a bounded
virtual window, a slot bitmap, and the per-region guard/data accounting)
that maps a device window into a **borrowed** `&mut AddressSpace<P>`.
Unlike [`DmaPool`][DmaPool] it allocates **no** frames — the physical
address is fixed by the hardware, so it maps the *device's own* frames
with caching disabled (`MapFlags::NO_CACHE`), never executable (W^X,
`AGENTS.md` §19.2), and the same unmapped guard-page bracketing the DMA
pool uses; a part-way page-table failure unwinds every page it added
(all-or-nothing, `AGENTS.md` §2.9). `MmioWindowMap::map_into(space,
phys_base, len)` returns an `MmioRegion`; `region_base(region, phys)`
resolves the region's device physical base through the direct physical
map (`PhysMap`) into a base pointer. It is the device-window analogue of
[`map_anonymous`](#7c-anonymous-user-memory-mem_map--mem_unmap):
an architecture-neutral mechanism over a borrowed live address space,
shared without duplication (`AGENTS.md` §2.2) by two consumers — the
owning adapter `MmioMap`, which bundles `MmioWindowMap` with an
`AddressSpace<P>` it owns so the kernel-side mapper (`KernelMmioMapper`,
in `kernel/virtio`) turns an `MmioRegion` into an
[ABI `RegisterWindow`](../drivers/bus.md#register-window-hand-off) for
the in-kernel driver host, and the `mmio_map` syscall facility (`plans/PI.md`
P10 chunk 5d-0), which maps a granted device window into the caller's
*own running* address space (the production wiring of that facility, over
a retained live address space, is staged with the arch-level live-space
retention).

The mapper is **capability-agnostic**; the gate is
`kernel/sec::mmio`, whose `map_mmio` / `unmap_mmio` verify
[`CapabilityId::MMIO_MAP`][CapabilityId::MMIO_MAP] and emit
`MmioMapped` / `MmioMapDenied` audit records (IDs 1040 / 1041, see
[Security audit catalogue](./security.md)). `MmioGateError::as_errno`
maps refusals to `Errno` exactly as the DMA gate does.

[CapabilityId::MMIO_MAP]: ../../tairix_abi/capability/struct.CapabilityId.html#associatedconstant.MMIO_MAP

## 6. Result-returning OOM contract

Every fallible operation in this crate returns
`Result<_, AllocError>`. No path panics on out-of-memory
(`AGENTS.md` §4). The error variants:

| Variant | Meaning |
| --- | --- |
| `OutOfMemory` | No free block of the requested size. |
| `SizeUnsupported` | Request exceeds the allocator's capacity / `MAX_ORDER`. |
| `ZeroSize` | Zero-byte / zero-slot requests are rejected. |
| `OutOfRange` | Frame / address outside the allocator's window. |
| `MetadataAllocFailed` | Allocator could not bootstrap itself. |
| `InvariantViolation` | Double-free, free of a reserved frame, malformed boot map. |

## 7. Unsafe & pointer arithmetic discipline

Per `AGENTS.md` §4, raw pointer arithmetic is confined to the
`ptr` module's bounds-checked helpers (`offset_within`,
`end_within`, `slice_within`). Every other module routes pointer
math through them. Every `unsafe` block carries a `// SAFETY:`
rationale per `AGENTS.md` §2.10, encapsulated behind a safe public
API; no `unsafe` leaks across crate boundaries.

## 7a. Platform memory-map sources

The `BootMemoryMap` is *fed* by the architecture port, not constructed
by `kernel/mem`. On x86_64 (Stage 3a (a)) the discovery surface lives
in `kernel/arch/x86_64`:

- `multiboot2` parses the BIOS-derived memory-map tag (Multiboot2
  type 6) and the EFI memory-map tag (type 17) a GRUB-EFI boot hands
  in; `pvh` parses the E820-style `hvm_start_info` memory-map table a
  QEMU PVH direct boot hands in. All three parsers are zero-copy and
  `no_alloc`.
- `bootmemory` bridges those typed entries into
  `MemoryRegionDescriptor`s with a `RegionKind` mirror that is locked
  to `tairix_kernel_mem::RegionKind` by a host-side dev-dep
  round-trip test (`AGENTS.md` §2.2 — no duplication).

The kernel binary (which links against `kernel/mem`) is responsible
for draining the descriptor stream into a `BootMemoryMap` via
`BootMemoryMap::push`. This split keeps `kernel/arch/x86_64` free of
`alloc` so it can be linked into the freestanding Stage-2 QEMU test
binaries that do not yet provide a `#[global_allocator]`.

## 7b. Encrypted swap (`swap`)

When a pager writes a page of anonymous, stack, or capability-bearing
memory out to a backing store, that page leaves RAM — and the
zero-on-free guarantees of [§4](#4-sensitive-region-api) and
[§5](#5-dma-buffers) would be void if the bytes could be read back off
an unencrypted swap device. The `swap` module closes that gap: every
page is sealed with `lib/crypto`'s ChaCha20-Poly1305 AEAD before it
reaches the device (`AGENTS.md` §4).

**Fail-closed by construction.** `AGENTS.md` §4 requires that the kernel
"refuses to activate a swap device that is not wrapped by the
encrypted-swap layer". TAIRiX enforces this in the type system rather
than with a runtime flag: a [`SwapBackend`] (the raw, slot-addressed
device) exposes only opaque [`SWAP_RECORD_LEN`]-byte records and makes
no cryptographic decision, and the **only** way to read or write a page
through it is [`EncryptedSwap`], whose sole constructor
[`EncryptedSwap::activate`] takes a [`SealKey`]. There is no plaintext
code path to fall back to, so plaintext swap is unrepresentable
(`AGENTS.md` §2.11).

**Shared sealing primitives (`seal`).** The ephemeral per-boot key
([`SealKey`]), the injected platform-RNG seam ([`EntropySource`]), and
the never-repeating nonce sequence ([`NonceSequence`]) have exactly one
definition, in the `seal` module, shared by this layer and the
compressed anonymous-memory tier ([§7n](#7n-the-encrypted-compressed-anonymous-memory-tier-ramzip)).
Each tier holds its **own** key and sequence — neither depends on the
other's key or metadata format — but the key-hygiene and
nonce-uniqueness logic is never duplicated. The key is drawn from the
platform RNG, zeroed on drop, and never persisted: a power cycle
destroys it, so paged-out secrets cannot be recovered at rest.

**Record layout & nonce discipline.** Each on-device record is
`nonce(12) ‖ tag(16) ‖ ciphertext(4096)`. ChaCha20-Poly1305 fails
catastrophically on `(key, nonce)` reuse, so each [`NonceSequence`]
draws a random 32-bit salt at construction and appends a 64-bit
monotonic counter; counter exhaustion fails closed
([`SwapError::NonceExhausted`]) rather than wrapping. The slot index is
bound as associated data, so a record relocated to a different slot
fails authentication. On any failure — bad slot, backend fault, or a
forged/tampered record — `load` zeroes the caller's buffer before
returning the error, so a caller can never observe stale or forged
plaintext (`AGENTS.md` §5.4).

The pager that calls `store` / `load`, and the capability gate on
*activating* a swap device, are Stage 8 work; this module is the
cryptographic layer they are required to route through.

[`SwapBackend`]: ../../tairix_kernel_mem/swap/trait.SwapBackend.html
[`EncryptedSwap`]: ../../tairix_kernel_mem/swap/struct.EncryptedSwap.html
[`EncryptedSwap::activate`]: ../../tairix_kernel_mem/swap/struct.EncryptedSwap.html#method.activate
[`SealKey`]: ../../tairix_kernel_mem/seal/struct.SealKey.html
[`EntropySource`]: ../../tairix_kernel_mem/seal/trait.EntropySource.html
[`NonceSequence`]: ../../tairix_kernel_mem/seal/struct.NonceSequence.html
[`SwapError::NonceExhausted`]: ../../tairix_kernel_mem/swap/enum.SwapError.html#variant.NonceExhausted
[`SWAP_RECORD_LEN`]: ../../tairix_kernel_mem/swap/constant.SWAP_RECORD_LEN.html

## 7c. Anonymous user memory (`mem_map` / `mem_unmap`)

A spawned process boots with exactly its spawn-time image: code/data/bss
plus a user stack and the startup-vector block, placed above the image's
mapped top with an unmapped guard page between each region
(`tairix_kernel_mem::derive_user_layout`, bound to the shared policy in
`spawn_layout::user_layout` — the placement scales with the image instead
of capping it at a fixed slot; `plans/SPAWN.md` SP2/SP3). The stack is a
*reserved span* (`USER_STACK_RESERVE_PAGES`, 8 MiB — derived from the one
default stack policy value, `tairix_kernel_core::DEFAULT_STACK_LIMIT_BYTES`,
which `LimitSet::DEFAULT` also carries as the `StackBytes` bound, so the
settable default and the structural span can never silently diverge)
whose top `USER_STACK_COMMIT_PAGES` (128 KiB) are eagerly mapped: a
process pays only the stack it actually touches, and the uncommitted
remainder below the committed bottom is growth room the demand-grown
stack path (`plans/SPAWN.md` SP11) backs on fault, bounded by the
settable `StackBytes` resource limit, while the guard page below the
span stays unmapped so a true overrun still faults deterministically.
Every admission path
(`SpawnCtx::admit_process` / `InitSpawnCtx::admit_init`) records the
producer-derived span (`spawn_layout::stack_span`, one derivation across
the ports) in the address-space registry beside the task's limits, and a
user fault inside the span below the committed base — read or write, and
offered *before* the write-fatal file rule (§7o) — backs **every page
from the committed base down to the faulting page** with fresh zeroed
`RW` pages through the installed `mem_map` producer, lowers the recorded
committed base per page (the live usage `sysinfo limits` reports for
`stack-bytes`), and re-freezes the registry snapshot once. Growth is
contiguous by construction: a large frame whose first touch lands several
pages below the committed base (the compiler owes no page-by-page probe
order) can never strand an unmapped hole above the low-water mark, so
"every span page at/above the committed base is resident" is an
invariant. Growth stops fail-closed at the effective `StackBytes` soft
bound (checked before any page is mapped), a fault below the span or on
frame exhaustion stays fatal, and the audited kill names the class
(`stack_limit` / `stack`). The QEMU verticals
(`tests/integration/stack_grow_qemu_aarch64` / `…_riscv64` / `…_x86_64`,
over the one `stack_grow_program` fixture) prove growth, the
`ulimit`-lowered bound kill, and the below-span guard kill end to end on
all three MMU ports — the x86_64 twin composes the shared production
board bring-up (`tairix_kernel::x86_64::boot::bring_up_bsp`) with the
production hook in the production `DISPATCH_SLOT` — and wasm32's
linear-memory model is an honest n/a. The
`mem_map` (`abi-v1` no. 14) / `mem_unmap` (no. 15) syscalls are the one
mechanism by which a process obtains and releases *additional* memory at
runtime — the foundation the `lib/rt` userland heap allocator (§7d) layers its
`malloc` / `free` over. The ABI shape is fixed in `tairix_abi::memory`
(`MapFlags`) and `tairix_abi::syscall`; the syscall-layer contract is the
`mem_map` / `mem_unmap` rows of [the syscall page](./syscalls.md).

**Anonymous memory is demand-paged, exactly like the user stack above.**
`mem_map` **reserves** address space only — it commits no frame and writes
no page-table entry — and records the reservation `(base, page_count)` in
the address-space registry (`AddressSpaceRegistry::record_anon_region`).
Each page then faults in lazily on first touch: a user data abort inside a
reserved region — read or write, offered *after* stack growth and *before*
the write-fatal file rule (§7o) — is resolved by
`resolve_anon_fault`, which backs the single covering page with one fresh
zeroed `RW|USER` frame through the `mem_map` producer's single-page commit
and re-freezes the registry snapshot. Per-fault kernel work is therefore
**one page**, so a task touching a large mapping is preemptible between
faults; this is what replaced the earlier eager commit, whose single
`mem_map` syscall zeroed and mapped the whole region in one
non-preemptible pass and, under a memory-stress workload (`stress --vm`),
monopolised the CPU for the entire loop and starved every interrupt (the
serial console stuttered and the machine appeared to lock up).

**Reservation is commit-accounted — TAIRiX does not overcommit anonymous
memory.** `mem_map` reserves *physical headroom* for every page of the
region up front, against a global no-overcommit budget
(`FrameAllocator::commit`, admitted only while `free_frames >=
reserve_frames + committed_frames + request`). The budget is the usable RAM
below the kernel reserve, so a reservation the machine cannot actually back
is refused **here**, deterministically, as `Errno::OutOfMemory` — a `Result`
the `lib/rt` heap turns into a null `alloc` and `Vec::try_reserve` turns into
`Err`, so a program that checks its allocations (like `stress --vm`) degrades
gracefully instead of being killed. A committed page's first touch is then
**guaranteed** a frame (`FrameAllocator::alloc_user_committed`, which converts
one reserved page to residency); an *eager* user draw
(`FrameAllocator::alloc_user`) may never dip below `reserve_frames +
committed_frames`, so it can never steal a committed page's reserved frame.
This replaces the earlier overcommit-and-fault-time-kill behaviour, whose
`stress --vm 4 --vm-bytes 76M` on a ~175 MiB machine surfaced out-of-memory
only as a fault-time task kill mislabelled a "wild" fault. Stack growth takes
the same commitment one page at a time (`resolve_stack_fault` commits before
it backs each growth page), and a reservation's still-unbacked pages are
returned to the budget on `mem_unmap` and on task teardown, so a task that
dies holding untouched reservations leaks no headroom. `mem_unmap` validates the caller-named `(base, page_count)` against
the recorded reservation (fail closed with `NotFound` otherwise), then
**sparsely** tears down the region — reclaiming and zeroing only the pages
that actually faulted in and skipping the untouched reservation pages — and
drops the record. The reservation's page-rounded size is what the
`AddressSpaceBytes` ceiling and the pinned-memory budget are charged at map
time, so those bounds are enforced up front and the fault path re-checks no
limit. The copy path stays fault-aware: `copy_in_user` offers a staging
miss to `resolve_anon_fault` (after the file and stack resolvers) exactly as
the hardware fault path does.

This is staged (`plans/SPAWN.md` SP5):

- **SP5a (landed).** The `abi-v1` surface (`MapFlags`, the two syscall
  numbers, the `Errno::OutOfMemory` variant), the C-callable stubs
  (`tairix_sys_mem_map` / `tairix_sys_mem_unmap`) and generated header
  (`include/tairix/tairix_memory.h`), the dispatcher arms, and an
  arch-neutral fail-closed seam in `kernel/core` (`MemMap`, defaulting to
  `NULL_MEM_MAP` → `Errno::NotImplemented`, installed through
  `KernelSyscallHandlers::with_mem_map`, mirroring the console and spawn
  seams). The handler rejects a zero `len` with `LengthOutOfRange` and a
  reserved flag bit with `OutOfRange` before reaching the producer.
- **SP5b-1 (landed).** The reusable, architecture-neutral `kernel/mem`
  producer (`map_anonymous` / `unmap_anonymous` in the `anon` module) that
  mutates a *live* user address space: it maps fresh frames into the
  caller's own [`AddressSpace<P: PageTable>`] as `RW|USER` (the single
  `ANON_FLAGS` set, never executable), zeroes each frame through the kernel
  direct map *before* the mapping is visible, and on unmap **sparsely**
  tears the region down — zeroing-on-free and releasing every frame that is
  resident and skipping the pages the demand-paging fault path never backed
  (the caller validates the reservation before it reaches here). A frame
  exhaustion part-way through a map unwinds every page it already added, so
  a failed map leaves the space unchanged (`AGENTS.md` §2.9). The
  per-page TLB invalidation rides the existing `AddressSpace::map` /
  `AddressSpace::unmap` flush (the §17.2 `TlbShootdown` slice); the
  cross-CPU shootdown is part of SP5b-2 when the producer is driven from a
  live multi-CPU regime. Host-proven over `HostPageTable` + `SimPhysMap`.
- **SP5b-2 (landed).** The aarch64 EL0 `-M virt` vertical
  (`tests/integration/mem_map_qemu_aarch64`) wires the SP5b-1 producer
  through the `kernel/core` `MemMap` seam: it builds one isolated EL0 space
  with `spawn_image`, **retains** it live behind a `MemMap` producer backed
  by `map_anonymous` / `unmap_anonymous`, and routes the program's
  `mem_map` / `mem_unmap` `svc`s through it. A pure-Rust EL0 fixture
  (`tests/integration/mem_map_program`) `mem_map`s a region (FIXED), writes
  and reads back a pattern (proving the pages are real `RW` memory),
  `mem_unmap`s it, then touches the released range — the data abort the
  fault handler reports as PASS. The `tairix_rt::mem_map` / `mem_unmap`
  wrappers are the program's interface. The **riscv64 sibling**
  (`tests/integration/mem_map_qemu_riscv64`) is now landed too: it reuses the
  same pure-Rust `mem_map_program` fixture and the same `kernel/mem` producer
  over an Sv39 U-mode space, drops into the program through `spawn_image` + a
  direct `EnterUser::enter_user` (no scheduler — a single task only
  direct-returns from its `ecall`s, so the cooperative-switch trap-save path is
  off the critical path), and reports the use-after-unmap page fault as PASS on
  `-M virt`. The x86_64 sibling and the production per-task live-space retention
  follow; wasm32's linear-memory model is an honest n/a.

The binding invariants the producer must honour (settled here as the SP5
design note, `AGENTS.md` §15.2):

- **W^X, `RW` only (`AGENTS.md` §19.2).** A region is always readable and
  writable and **never** executable; `mem_map` never produces an `RWX`
  mapping. An executable (JIT) mapping is a separate, later
  `CAP_JIT_MAP_EXEC`-gated `RW`→`RX` flip and is explicitly **not** part of
  SP5 — `mem_map` does not add an `mprotect`-equivalent.
- **Per-process, never global (`AGENTS.md` §4).** A region is mapped only
  into the **caller's own** hardware-isolated address space. There is no
  global user heap and no cross-process mapping; shared memory stays the
  capability-checked IPC object. Because it only ever grows the caller's
  own space, the pair is unprivileged (no capability, `AGENTS.md` §16.6).
- **Zero on map and on free (`AGENTS.md` §4 — secret hygiene).** Pages are
  zeroed before the mapping is visible — no stale kernel or other-process
  bytes — and the frames `mem_unmap` reclaims are zeroed on free, the same
  guarantee [§4](#4-sensitive-region-api) and [§5](#5-dma-buffers) give
  the rest of the crate.
- **Deterministic, no-overcommit OOM (`AGENTS.md` §4 / §2.9 / §26.3).** A
  reservation the machine cannot back surfaces as `Errno::OutOfMemory` at
  `mem_map`/commit time, never as a fault-time kill and never a panic — the
  user-facing projection of the
  [§6 result-returning OOM contract](#6-result-returning-oom-contract).
  Anonymous (and stack-growth) memory is commit-accounted against the usable
  RAM below the kernel reserve, so a successful reservation is a *promise* the
  pages can be touched; there is no per-process quota, but the system as a
  whole does not overcommit, so a first touch of committed memory is
  guaranteed a frame rather than gambling on availability.

The immutable-`FrozenAddressSpace` snapshot the post-spawn registry stores
(§3b) is read-only; the production `mem_map` / `mmio_map` producers instead
mutate a task's **retained live** address space, the single live-space
mutation path (§7e) rather than a second parallel address-space model
(`AGENTS.md` §2.2).

## 7d. Userland heap allocator (`tairix-rt`)

The `mem_map` / `mem_unmap` pair is a page-granularity primitive; ordinary
`alloc` types (`Box`, `Vec`, `String`) need a byte-granularity `malloc` /
`free`. `lib/rt` supplies it as a `#[global_allocator]` (`lib/rt/src/heap.rs`),
so a first-party Rust program (the shell, `init`) can use `alloc`. It is a
userland allocator — outside `kernel/mem` — but is documented here because it
is the consumer the `mem_map` ABI exists for (§7c).

- **Free-span allocator over a growable, fixed-base arena.** The heap owns one
  contiguous virtual arena that starts at a fixed base and grows upward, one or
  more whole pages at a time, by `mem_map`ping with `MapFlags::FIXED` at the
  arena's current top. Freed regions are tracked as a coalesced,
  address-sorted free list held *inside the allocator* (a fixed-capacity span
  table), not as intrusive links in user memory, so the bookkeeping never
  dereferences freed memory and every returned pointer is range-checked before
  it is handed out (`AGENTS.md` §4 — no `unsafe` allocator doing raw pointer
  arithmetic without a checked wrapper).
- **Real free, with shrink.** Allocation is first-fit honouring the requested
  alignment, returning alignment padding to the free list; free coalesces with
  neighbours, and when whole trailing pages become free at the arena top they
  are returned to the kernel with `mem_unmap` — both syscalls are genuinely
  exercised, no dead path (`AGENTS.md` §2.14).
- **Deterministic OOM (`AGENTS.md` §4 / §2.9).** A failed `mem_map` or an
  overflowed span table returns a null pointer per the `GlobalAlloc` contract,
  never a panic.
- **No re-zero on free (`AGENTS.md` §2.16).** The kernel already zeroes pages
  on map and on free (§7c), so memory entering or leaving the process is clean;
  a process reusing its own freed bytes within its own heap is not a security
  boundary, so the heap does not re-zero on the hot path.

The pure free-span bookkeeping is host-unit-tested over a fake pager; the
aarch64 `-M virt` vertical `tests/integration/heap_qemu_aarch64` proves it end
to end — a pure-Rust EL0 fixture (`tests/integration/heap_program`)
Box-allocates, grows a `Vec` across several pages, reallocates after freeing,
verifies every value, and exits 0, with the program's allocator-issued
`mem_map` / `mem_unmap` `svc`s routed through the live `MemMap` producer
(`plans/PI.md` P6e-3b prerequisite).

## 7e. Retained live address space (`live`) and the production producers

The post-spawn registry holds a read-only `FrozenAddressSpace` snapshot (§3b)
for the user-memory copy path, but `mem_map` / `mmio_map` must mutate the
*running* space — grow a process's heap, or map a driver's granted device
window into its own space. A live arch `AddressSpace<P>` cannot sit behind
the registry's `Send + Sync` shared lock (the production page-table backend
is `!Send`/`!Sync`), so the live space is retained **per task and reached
only by the CPU currently running it**, never a global lock over a live page
table (`plans/PI.md` 5d-0-ii (b′)).

- **`kernel/mem::live` — the object-safe boundary.** `LiveUserSpace` is a
  `Send` object-safe trait (`map_anonymous` / `unmap_anonymous` /
  `map_device_window`); the generic `LiveSpace<P, M>` implements it by
  composing the audited `map_anonymous` / `unmap_anonymous` (§7c) and the
  `MmioWindowMap` device-window allocator (§5.2) — there is exactly one
  mapping path for each (`AGENTS.md` §2.2). Erasing the space behind the
  trait keeps `kernel/core` free of any concrete page-table backend `P`
  (`AGENTS.md` §17.4). `LiveSpaceError` unions the two mechanisms' errors.
- **Per-task ownership + per-CPU publication.** `kernel/core::kthread` owns
  the boxed live space in the task's `ThreadControl` (so it — and its
  page-table frames — is reclaimed when the task exits). A new per-CPU
  `USER_LIVE_SPACE` table publishes a pointer to it immediately before the
  task is switched in and clears it the instant the task switches back —
  the exact lifecycle as the `USER_RESUME` reschedule handle — so the slot
  is populated only while that CPU runs the (now trapped) task. The
  `with_current_live_space(cpu, f)` accessor hands a producer an exclusive
  `&mut dyn LiveUserSpace` that cannot alias: the task is suspended in its
  own syscall trap for the whole call, and a task runs on at most one CPU
  (`AGENTS.md` §4 — the access is genuinely exclusive). The
  `spawn_user_kthread_with_stack_live` admission entry carries the space.
- **The production producers.** `kernel/core::live_producer` provides
  `LiveMemMap<A>` (`MemMap`) and `LiveMmioMap<A>` (`MmioMapFacility`); each
  holds a `&'static A` (mirroring `KernelProcessWait`), reads
  `arch.current_cpu()`, routes through `with_current_live_space`, folds
  `LiveSpaceError` onto a stable `Errno`, and **fails closed**
  (`NotImplemented`) when the running task has no retained space
  (`AGENTS.md` §2.9 / §5.4 — it never touches another task's memory).
  `mmio_map` is fully served (the guarded `MmioWindowMap` chooses the user
  virtual window); anonymous `mem_map` is fully served for both `FIXED`
  placement (the caller names `addr_hint`) and **non-`FIXED`** placement
  (the kernel chooses the base out of the per-task heap window via
  `LiveSpace::map_anonymous_placed`, §7f) — never a guessed base.

The retention is wired into the **aarch64** spawn path (`plans/PI.md`
5d-0-ii (b′)-2): the live space threads through the `admit_init` /
`admit_process` seam as `Option<Box<dyn LiveUserSpace + Send>>` (the x86_64 /
riscv64 ports pass `None` until their turn), the aarch64 `init_spawn` /
`spawn_producer` freeze a snapshot for the copy path **and** retain a
`LiveSpace` built from the same arch space, admitting through
`spawn_user_kthread_with_stack_live`, and `kernel_main` installs `LiveMemMap` /
`LiveMmioMap` for every port (a port that retains no live space simply fails
those syscalls closed). A device window a user-space driver maps through
`mmio_map` is given the EL0-accessible device leaf
(`kernel/arch/aarch64::el0_device_leaf_attrs`, `AP_RW_EL0`) so the driver can
read its own register without a permission fault (§5.2). The aarch64
`mmio_map_qemu_aarch64` `-M virt` vertical proves the chain end to end (a
spawned EL0 program maps a minted virtio-MMIO window grant, reads its
`MagicValue` register, **and** round-trips a non-`FIXED` `mem_map`: map →
write a sentinel → read it back → `mem_unmap`). The `dma_alloc` DMA half is
the remaining staged 5d-0-ii (c) follow-on.

## 7f. Non-`FIXED` `mem_map` placement allocator (`anon_window`)

A non-`FIXED` `mem_map` asks the kernel to choose the base. That placement
decision is `kernel/mem::AnonWindowMap`: a per-task user-virtual-address
allocator over one configured heap window, driven against a borrowed live
`AddressSpace<P>` by `LiveSpace::map_anonymous_placed` (`plans/PI.md`
5d-0-ii (c)).

- **Placement only.** It allocates and releases page-aligned virtual ranges;
  the actual mapping is the audited `map_anonymous` (§7c) — one mapping path
  (`AGENTS.md` §2.2). `LiveSpace::map_anonymous_placed` reserves a base, maps
  it, and releases the reservation on a mapping failure (so a failed call
  consumes no address space); `unmap_anonymous` validates the placement
  record and releases its range, failing closed on a wrong base/extent before
  any teardown (§5.4).
- **Bump cursor + free-list, §24.1-scalable.** A bump cursor serves fresh
  ranges and a free-list of released holes (first-fit, split on a partial
  match) serves reuse, so the allocator's own memory is bounded by the
  live-plus-freed region count, never the page count of the window. The
  window is *address space*, not a physical resource, and its size is
  **derived from discovered RAM, never a hard-wired constant** (`AGENTS.md`
  §24.1): each port places it as the topmost *fixed-anchor* user region
  (4 GiB above the image bias, `spawn_layout::ANON_WINDOW_OFFSET`, above the
  device, DMA, and shared-memory windows) and splits the address space above
  it through `user_windows::user_windows(total_frames, base, USER_VA_TOP)` —
  the heap window tracks physical RAM (the true upper bound on backable
  pages), clamped to half the addressable user VA above the base and floored
  at `ANON_WINDOW_MIN_PAGES` (16 MiB) for a tiny machine, while the
  demand-paged **file-mapping window** (§7o) takes every remaining page up
  to the per-port user-VA ceiling. A 1 GiB machine gets the same 1 GiB heap
  window the former fixed constant gave; a large server scales up instead of
  being capped at 1 GiB. The window costs no RAM until the frame allocator
  backs a mapping — and that backing fails closed as a deterministic OOM
  (§4), so a 20 GiB request on a 1 GiB machine is refused (at the virtual
  reservation if it exceeds the window, else at frame exhaustion), never
  over-committed.
- **Tested.** `AnonWindowMap` host-unit tests (bump/no-overlap, exhaustion,
  release+reuse, fail-closed release), `LiveSpace` placement tests (real
  `HostPageTable` map + zero-on-map + reuse + fail-closed wrong-extent
  unmap), the `LiveMemMap` routing test, the `user_windows` split tests
  (small/large RAM, half-split cap, degenerate spans), and the extended
  `mmio_map_qemu_aarch64` `-M virt` vertical's `mem_map` round-trip.

## 7g. Reclaimable-memory model (`tairix_reclaim`) and the filesystem cache

`lib/reclaim` (`tairix_reclaim`) is the one definition of how a
reclaimable cache — memory holding *derived* state that can always be
rebuilt from its canonical source — is classed, bounded, and accounted
(`plans/SMARTRAM.md`).

It is a shared crate rather than a kernel module because memory pressure
is a property of the machine, not of privilege level: the desktop
session holds megabytes of rasterised glyphs, cursors and icons, and it
must give them back in the same order, at the same bands, as the
kernel's own caches. `userland/*` may not depend on `kernel/*`, so the
one definition sits in `lib/` and both sides import it. Only what
genuinely needs the kernel's anonymous-memory tier stayed behind in
`kernel/mem::pressure`: the `ramzip` handoff gate, the escalation
ladder, and the frame allocator's `FreeMemorySource` binding.

- **Classes.** Each entry belongs to one `ReclaimClass` with a
  deterministic `reclaim_priority` following the `plans/SMARTRAM.md`
  section 7 pressure order (first reclaimed first): `DisposableUi`,
  `PredictivePrefetch`, `BackgroundValidation`, `SemanticAppCache`,
  `RuntimeCache`, `CleanFileData` (page chunks of clean file bytes,
  one bounded device read to rebuild), `TransformCache`, `FsMetadata`
  (stat/security/lookup/directory-entry records — small, hot, rebuilt
  by a tree walk, so they outlive file data under pressure), and
  `ReliabilityAssist`. The taxonomy is the complete SMARTRAM class
  set; consumers beyond the filesystem cache arrive with the stages
  that build them.
- **Classification and admission (fail closed).** Before a cache
  admits anything it declares a `CacheCandidate` — class, a
  `ReclaimOwner` to charge (a kernel subsystem, a filesystem volume by
  its stable per-boot mount handle, or a task; session/service owners
  arrive with their identities), a `RebuildCost`, a `Sensitivity`, an
  `InvalidationSource`, a `ReclaimRule`, and its worst-case per-entry
  bookkeeping bytes — and passes `CacheCandidate::classify`, a pure
  (deterministic) gate producing a `CachePolicy` or a typed
  `AdmissionRefusal`. An unknown class or owner, unruled-out sensitive
  material (credentials, keys, capability tokens — and an undeclared
  sensitivity is treated as the most sensitive), per-entry metadata
  over the fixed `MAX_ENTRY_METADATA` validation bound, a missing
  reclaim rule (non-reclaimable), or a missing invalidation source is
  refused, and the producer serves uncached: no unowned,
  unclassifiable, or uninvalidatable memory exists in the model.
- **Budgets with hysteresis.** A `CacheBudget` is derived from the
  backing resource's size (`CacheBudget::from_backing` — 1/16 of the
  kernel heap arena per cache; each boot volume carries two, the clean
  filesystem cache and the transform cache, so the boot volumes' four
  caches together stay at or under 1/4 of the heap and cache growth can
  never exhaust it). Growth runs to the *hard* limit; a forced shrink
  evicts down to the *low* watermark (3/4 of hard), never both on one
  threshold.
- **Fail-closed accounting.** `CacheAccounting` keeps per-class byte
  ledgers with checked arithmetic (typed `AccountingError` on
  overflow/underflow, never wrapping), split into the entry *payloads*
  and the per-entry bookkeeping *metadata* charged on top of them
  (`class_payload_bytes` / `class_metadata_bytes`; the budget and
  shrink targets bound their sum), plus saturating hit/miss/insertion/
  invalidation/eviction/refusal counters and the SMART9 event counters:
  pressure-forced shrink passes, whole-cache teardown drains, and
  detected internal failures (§7k).

The first consumer is the **clean, rebuildable filesystem cache**
(`kernel/core::fs::CachedFs`, `plans/SMARTRAM.md` section 6.1): a
wrapper around each mounted volume's filesystem driver, *below* the VFS
policy layer, applied at driver registration (`system_mount`). Key
properties:

- **Never bypasses authorisation.** Every permission check still runs
  in the secured VFS per operation; the cache only spares the driver a
  repeated structural read. A `security` record is cached but
  invalidated by `set_security`, so a tightened mode is seen by the
  very next check.
- **One volume, one writer.** Every mutation flows through the wrapper:
  the `fs_*` syscalls and the `CAP_USER_ADMIN` account-administration
  engine share the single registered driver behind one `SleepLock`
  (`LateFilesystem::register` returns the leaked lock precisely so a
  second, coherence-breaking window over the same device cannot exist).
- **Precise, fail-closed invalidation.** Writes/truncates drop the
  file's chunks and stat; create/remove/rename drop the affected
  directory's *entire* lookup set (driver name matching may fold case),
  its directory entries, and its stat; an unidentifiable mutation
  target purges the whole cache; a detected ledger imbalance poisons
  the cache (purge + admit nothing) while the driver keeps serving.
- **Bounded and zeroing.** Payload copies are fallibly allocated
  (`try_reserve`); oversized names are refused; reads above four chunks
  bypass the cache so bulk streams cannot evict the hot working set;
  and every cached buffer (file bytes, names) is zeroed on
  invalidation, eviction, purge, and teardown — the volumes are
  encrypted at rest, so cached bytes are decrypted user data that must
  not linger in reusable heap memory.

## 7h. VM pressure bands and reclaim ordering (`pressure`)

`tairix_reclaim::pressure` is the one definition of the system's
memory-pressure state and of the order reclaimable caches shrink in as
pressure rises (`plans/SMARTRAM.md` SMART2). The band vocabulary —
normal, mild, moderate, severe, critical — is shared with
`plans/SWAPSWAPSWAP.md`; there is no parallel model.

- **The gauge.** `MemoryPressure` samples a `FreeMemorySource` — in
  production the physical `FrameAllocator` (free frames are the
  authoritative reading; the boot path builds one gauge over the leaked
  allocator and every mounted volume's cache shares it) — and folds
  each reading into a banded state machine with **hysteresis**: a band
  is entered below one watermark and left above a strictly higher one
  (initial targets: mild 20%/25% free, moderate 10%/14%, severe
  6.25%/8%, critical 3.125%/5% — implementation constants in the
  `plans/SWAPSWAPSWAP.md` section 6 shape, backed by the §7l benchmark
  evidence, never ABI). Deepening applies immediately; relaxing steps
  one band at a time past each exit watermark. Sampling happens on the consumers'
  own operations — no background worker, no periodic tick. Every
  *stored* band change counts one entry into the new band
  (`MemoryPressure::band_entries`, one atomic per band; the starting
  band and hysteresis holds count nothing), so pressure-state
  transitions stay observable through the internal diagnostics (§7k).
- **Reserves, fail closed.** The thresholds carry a reserve floor
  (1/64 of the backing). A reading at or below it is critical
  regardless of history; a zero-size (unknown) backing reports critical
  for every reading and admits nothing. `growth_permitted` allows cache
  growth only at normal pressure and never lets it take the free
  reading into the reserve — cache expansion can never be the cause of
  reserve exhaustion.
- **Reclaim ordering.** `shrink_target(band, class, budget)` is the
  pure per-band ceiling each `ReclaimClass` must shrink to: at mild
  pressure the disposable/speculative classes drop and semantic,
  runtime, and clean-file classes shrink to the low watermark; at
  moderate, clean file and transform cache drain fully while metadata
  and recovery assist are capped at the low watermark; at severe and
  critical every class shrinks to zero. Targets are monotonically
  non-increasing with depth.
- **`ramzip` handoff and escalation.** `ramzip_handoff` fixes the
  `plans/SWAPSWAPSWAP.md` ordering: no compression at normal/mild; at
  moderate, compression of cold anonymous pages may start only once
  clean and transform cache are drained (reconstructable clean cache is
  cheaper than encrypted compressed anonymous storage); at severe
  `ramzip` owns cold-anonymous policy; at critical speculative work
  stops and `escalation` owns the next step. `escalation` is the
  deterministic answer when reclaim cannot help: reclaim caches while
  any remain, then hand off to `ramzip` (moderate/severe), then the VM
  pressure policy (critical). These are the seams the SWAP3 stage binds
  to when the `ramzip` store lands.
- **The consumers.** `CachedFs` (§7g) and the transform cache (§7i)
  sample the gauge at the head of every cache-touching operation: the
  band's forced-shrink targets are applied (data before metadata, every
  evicted buffer zeroed) before the cache is read, and admission is
  refused outside normal pressure — the volume is always still served
  straight from the driver.

## 7i. The ARXFS transform cache (SMART3)

The transformation cache (`plans/SMARTRAM.md` SMART3, section 6.2)
retains the expensive intermediate form ARXFS produces on every read of
a compressed cluster: the verified, decrypted, decompressed cluster
plaintext. Without it, each read that touches a compressed cluster pays
the full pipeline — a device read, an AEAD decrypt, and integrity checks
per stored block, then a whole-frame decompression — once per *call*;
with it, once per *cluster*.

- **A driver seam, a kernel implementation.** The ARXFS driver stays
  kernel-independent: it exposes the `ClusterCache` trait
  (`tairix_drv_fs_arxfs::ClusterCache`) and consults an injected
  implementation only in its serving read path. The production
  implementation is `tairix_kernel::transform_cache::TransformClusterCache`,
  installed by the boot path on both mounted volumes (`system_mount`
  for the read-only `/System` volume, the unlock path for the writable
  root) via `ARXFS::with_cluster_cache`. A volume without a cache
  behaves exactly as before, and the integrity passes (scrub, check,
  rescue) never consult it — they exist to verify the on-disk bytes.
- **Complementary to `CachedFs`, not duplicate.** `CachedFs` (§7g)
  retains page chunks of *served* plaintext for small reads; the
  transform cache sits below the driver's read path and covers what
  `CachedFs` cannot: the large sequential reads (bundle and
  driver-store loads) that bypass `CachedFs` by design, and `CachedFs`
  misses — both of which otherwise re-run the whole transform per call.
- **Classified, budgeted, pressure-governed.** The cache declares a
  `CacheCandidate` (class `TransformCache`, owned by the volume's
  stable per-boot mount handle, expensive to rebuild, decrypted user
  data, source-mutation invalidated, droppable) through the §7g
  admission gate — a refusal poisons it from birth and the driver keeps
  serving. Entries are LRU-evicted against a `CacheBudget`, admission
  obeys `growth_permitted`, and every operation first applies the
  band's `shrink_target`: the class is preserved at mild pressure and
  drained to zero from moderate on, before `ramzip` is handed anything
  (§7h).
- **Coherent by construction.** Entries are keyed by the cluster's
  first stored physical block and carry the run length. Every block
  free in the driver funnels through one choke point, which invalidates
  the covering entry *before* the block can be recycled; a transaction
  rollback (whose frees bypass that choke point) purges the whole cache;
  a defective entry that would stall the read loop fails the read closed
  (`DeviceFault`) instead. Reflink-shared clusters are only invalidated
  when their stored run is actually freed — a surviving referrer keeps
  the (still identical) plaintext.
- **Secret hygiene.** The plaintext is decrypted user data from an
  encrypted-at-rest volume: every buffer is volatilely wiped
  (`zeroize`) when its entry is invalidated, evicted, replaced, purged,
  or torn down, and the driver wipes its own transient frame and
  plaintext scratch on every path of the cluster read, clone, and
  decompose routines.

## 7j. The semantic application-launch cache (SMART4)

The semantic app/runtime cache (`plans/SMARTRAM.md` SMART4, section 6.3)
retains the result of the one shared application load gate
(`lib/appload`) for bundles on the immutable read-only system stores
(`/System/Apps`, `/System/Services`): the parsed signed `AppInfo`
manifest, the content-hash and syscall-interface-hash verdicts, the
dynamic-loader library policy decisions, and the validated `rxe`
entry-point image — one `LoadedApp` per bundle. Without it, every launch
of a system command re-reads and re-hashes the whole bundle tree and
re-verifies its Ed25519 signature; with it, once per boot.

- **The cache is `tairix_kernel_core::launch_cache::LaunchCache`,**
  held by the `AppStore` behind the `/System`-mount readiness latch. The
  boot path that publishes the mount installs the cache's budget and the
  system pressure gauge (`AppStore::install_reclaim`, called by
  `install_system_mount` just before it resolves the latch); until then
  — and on any classification refusal — every launch is served uncached
  through the full load gate (fail closed).
- **Only immutable bundles are cacheable.** A bundle on a writable
  volume (`/Apps`, a user's own store) can change between launches and
  is re-verified through the full gate every time
  (`AppStore::cacheable_bundle`). The read-only stores cannot change
  within a boot, so the boot *is* the entry's generation
  (`InvalidationSource::GenerationToken`): an app or system update is a
  new volume image and a new boot, and there is no stale-entry window to
  invalidate across.
- **A hit carries no caller authority.** The cached ceiling is the
  manifest request itself (the spawn path loads under the full-word
  intersection identity before inserting, making the result
  caller-independent); the per-caller capability intersection happens on
  every admit, and the spawn path re-authorises the *caller's* read of
  the entry point through the secured VFS before serving a hit — so a
  policy or grant change can never be replayed from the cache, and a
  hit and a miss produce identical load decisions.
- **Classified, budgeted, pressure-governed.** The cache declares a
  `CacheCandidate` (class `SemanticAppCache`, owned by the kernel
  app-store subsystem, expensive to rebuild, system data,
  generation-invalidated, droppable) through the §7g admission gate — a
  refusal poisons it from birth. Entries are LRU-evicted against a
  `CacheBudget` (the same kernel-heap-derived policy as §7g/§7i),
  admission obeys `growth_permitted`, and every operation first applies
  the band's `shrink_target`: shrunk to the low watermark at mild
  pressure and drained to zero from moderate on, before `ramzip` is
  handed anything (§7h). Reclaim can never make an app unlaunchable — a
  miss re-runs the gate over the intact on-disk bundle.
- **No secret content.** Entries are shared `Arc`s to signed, public
  system code (`Sensitivity::SystemData`) — never credentials, keys, or
  user plaintext — so eviction drops the cache's reference without
  wiping: a launched process legitimately holds the same image.

Two SMART4 families are deliberately **not** cached, and are recorded as
scope decisions in `plans/SMARTRAM.md`: command-resolution output
(`lib/cmdres` is a pure spelling function with no I/O — recomputing it
is cheaper than any cache, and the expensive verification behind the
winning candidate is exactly what this cache retains), and a separate
RXE relocation-preparation cache (the loader model has no separate
relocation stage; the validated image in the `LoadedApp` *is* the
cached RXE state).

## 7k. Reclaimable-cache observability (SMART9 + STRESSTEST ST1)

The reclaimable-cache subsystem is observable through its internal
counters, the existing structured logging, and — since
`plans/STRESSTEST.md` ST1 — the capability-gated System Information
queries `MEMORY_PRESSURE`, `RECLAIM_STATS`, `CACHE_LEDGERS`, and
`RAMZIP_STATS` (`CAP_SYSINFO_KERNEL`, audited): no `/proc`, no `/sys`,
and no text-scrape file. The kernel's half of the export is the one
arch-neutral memory-statistics registry
(`kernel/core::memstats::MEM_STATS`): the boot path publishes the single
system pressure gauge through it, every production cache registers its
`CacheLedger` at construction (observation-only, lock-free reads of
saturating diagnostics), and the process-global `ramzip` tier's stats
feed is installed there by the boot path
(`memstats::install_global_ramzip_stats`, reading
`ramzip::global_stats`, §7p) — reporting a truthful idle all-zero tier
until one is installed and populated.

The reclaim model has two halves (§7g), and so does the export. Only the
kernel's caches can be *measured* from outside the process holding them;
a desktop process's glyph atlases and decoded icon artwork are its own
heap, invisible to anything else. Left there the class totals would lie
— `disposable-ui`, the class reclaim starts with, would read zero on a
desktop holding megabytes of exactly that — so the userland half
**reports** what it holds, and the two sets are folded into one.

- **Counters.** Every cache instance's `CacheAccounting` (§7g) is the
  per-owner counter surface — each cache is charged to exactly one
  `ReclaimOwner`, so its ledger *is* that owner's contribution. The
  `hits` and `misses` counters are kept **per reclaim class** (each
  lookup is attributed to the class it served), so the `RECLAIM_STATS`
  export carries a genuine per-class hit ratio — the direct measure of a
  cache's effectiveness, which `sysmon`'s `caches` panel renders as its
  `hit%` column. Beside those and the split payload/metadata byte
  ledgers and the insertion/invalidation/eviction/refusal counters it
  counts: `pressure_shrinks` (forced-shrink passes that actually
  reclaimed), `teardowns` (whole-cache drains — a rollback purge, a
  poison drain), and `failures` (detected ledger/index defects). The
  pressure gauge counts entries into each band (`band_entries`, §7h).
  Counters saturate: they are diagnostics, never control flow.
- **Stable audit events.** Security-relevant failures emit one
  structured record through the boot audit sink using `kernel/mem`'s
  reserved `EventId` range (`2_000..3_000`, `reclaim_audit`):
  `RECLAIM_CACHE_REFUSED` (2000, Error) when a candidate fails the §7g
  classification gate at construction (the cache starts poisoned, its
  consumer serves uncached), and `RECLAIM_CACHE_POISONED` (2001, Error)
  when a live cache detects a ledger or index defect — a corruption-like
  event — drains itself, and disables admission. A cache reports its
  poisoning exactly once; normal operation emits nothing.
- **Closed field shape, no secrets.** Every record carries exactly
  `cache` (the fixed label `clean_fs` / `transform` / `launch`),
  `owner` (a kernel subsystem name or the owner kind `volume` /
  `task`), `owner_id` (the numeric mount handle or task id), and
  `cause` (a fixed label such as `ledger_imbalance`,
  `orphan_index_slot`, or an `AdmissionRefusal::cause`). No file name,
  cached plaintext, key, or capability token can enter a diagnostic
  record (`plans/SMARTRAM.md` section 9).
- **The wiring.** The boot paths that build the caches thread the
  `'static` audit sink through their constructors: `system_mount`'s
  `cached` helper and `install_system_mount` (the `/System` volume's
  clean and transform caches, the launch cache via
  `AppStore::install_reclaim`), and the unlock path's
  `register_writable_state` / `WritableStateSink` (the writable root's
  pair).
- **Per cache, not only per class.** A cache describes itself with one
  shared `tairix_reclaim::CacheLedger` — its label, its `ReclaimOwner`,
  its class, and a shared handle to the ledger above — and one shared
  conversion into the `CacheLedgerRecord` wire row, so a kernel row and a
  reported row cannot be spelled differently. `MemStats` exports its rows
  through the `CacheLedgers` introspection domain; `sysinfod` folds every
  row into the per-class `RECLAIM_STATS` totals with the single
  `fold_cache_ledgers` and serves the breakdown itself as
  `CACHE_LEDGERS`, so the class total is by construction the sum of its
  rows. `sysmon`'s `caches` panel renders both.
- **A process reports its own caches, event-driven.**
  `tairix_rt::cachereport` holds the process-wide set of caches and
  submits them through the ungated, self-scoped `CACHE_REPORT` operation
  — ungated for the same reason `SELF_PROCESS_LIST` is, since a process
  describes only itself, grants nothing, and reads nothing. It samples on
  each turn of the owning program's event loop, sends only when the
  sample differs from the last one sent, and offers a one-shot deadline
  for a change the rate limit suppressed. An idle process arms nothing:
  its last report is still true, because a process that is not running is
  not changing what it holds. There is no timer and no poll loop
  anywhere on the path.
- **A reported figure is contained as a claim.** The registry of reported
  rows lives in `sysinfod`, never the kernel, so it cannot reach
  `reclaim_class_stats` and therefore cannot steer the `ramzip` handoff
  (§7h) — structurally, not by convention. A submitted row must leave its
  origin unset and its pid zero; the service stamps both from the
  caller's kernel-attested `Origin`, keys each entry by the unforgeable
  process-instance id so a recycled pid inherits nothing, replaces rather
  than accumulates, expires an instance that is gone, derives its
  reporter capacity from the machine's RAM, and emits no audit record on
  that ungated path (which would otherwise hand every process a way to
  write the hash-chained journal). Every row carries its origin and
  `ReclaimClassRecord::self_reported_bytes` carries the claimed share of
  each class total, so a reader always sees what is attested and what is
  claimed.

## 7l. Cross-cache integration and benchmark evidence (SMART10)

The per-cache suites (§7g–§7j) each prove one cache against its own
gauge; the SMART10 integration suites prove the *system* behaviour the
plan binds, over **one** shared gauge:

- **`kernel/core/src/reclaim_integration_tests.rs`** drives the
  production `CachedFs` and `LaunchCache` from a single simulated
  free-memory source through the full band order: mild refuses growth
  while clean data survives, moderate drains the semantic and clean
  classes while hot metadata is preserved, severe forces every class
  to zero, and both caches keep serving correctly from their backings
  throughout. The `ramzip` handoff is computed over the caches'
  *combined* clean+transform residue (held while any remains, open
  once their own next operations drain it, never at critical — where
  `escalation` yields the VM policy), the reserve floor is shared and
  admits nothing, and a file mutated while the caches were drained is
  never served stale after recovery.
- **The thrash scenario** flaps the free reading inside the mild
  band's hysteresis window: the band holds (`band_entries` does not
  grow), neither cache oscillates between rebuild and reclaim (the
  insertion counters stay flat and the refused re-admissions are
  counted), and one genuine recovery above the exit watermark rebuilds
  once — churn is detected through the §7k counters and reduced to
  zero rebuilds by the hysteresis plus outside-normal admission
  refusal, with no new mechanism.
- **The layered stack** is proven in
  `kernel/tairix-kernel/src/transform_cache_tests.rs`: `CachedFs`
  wrapping a real ARXFS volume whose read path consults the installed
  `TransformClusterCache`, both on one gauge — a filesystem-cache hit
  never reaches the transform layer, and moderate pressure drains both
  layers on their own next operations while the volume still serves
  correct bytes through the full decrypt/decompress pipeline.
- **Benchmark evidence** (`plans/SMARTRAM.md` section 14) is the
  work-avoided form: the integration suite's bench test asserts
  deterministically that a warm pass performs zero driver reads and
  zero load-gate runs (the retention policy's entire benefit), and
  prints wall-clock estimates for the warm and cold passes —
  explicitly estimates for threshold tuning, never guarantees or
  assertions. The shared test fixtures live once:
  `kernel/core/src/test_pressure.rs` (the controllable gauge source)
  and the bundle-verification helpers in
  `kernel/core/src/test_bundle.rs`.

The gauge's QEMU-level behaviour rides the existing memory soak
(`tests/integration/memsoak_qemu_aarch64`), which exercises the frame
allocator the production gauge samples; a dedicated in-guest
pressure-band vertical is deliberately not built while the gauge's
consumers are all host-provable — the band arithmetic is pure and the
allocator reading is already soaked.

## 7m. The whole-disk block cache (SMART11)

`kernel/tairix-kernel/src/block_cache.rs` is the block-level LRU cache
under the entire mounted storage stack (`plans/SMARTRAM.md` SMART11):
the boot path wraps the one brought-up disk in a `BlockCache` **before**
the block-sharing layer (`shared_block::SharedBlock`), so every window
onto the disk — the `/System` driver-store window, the encrypted-root
unlock window, and the writable-root window — reads through one
coherent cache of recently used device blocks. It complements the
layers above rather than duplicating them: `CachedFs` (§7g) retains
served plaintext per volume and the transform cache (§7i) retains
decompressed cluster plaintext; the block cache retains the raw device
blocks underneath both, so their misses — and every consumer with no
higher cache (partition-table walks, driver-store scans, ARXFS
metadata block reads) — avoid a device round-trip that parks the
calling task across a completion interrupt.

- **Classification and budget.** Classified through the §7g gate as
  `CleanFileData` (clean, rebuildable, one bounded device read),
  owned by the `boot_block_device` kernel subsystem, treated as user
  data (the disk carries the encrypted user volume), droppable, and
  precisely invalidated by the device's single serialised writer. A
  refusal — or a block size the per-block entry model cannot bound —
  poisons the cache from birth: every operation passes straight
  through to the device (fail closed). Bounded by the same
  kernel-heap-derived `CacheBudget` as the volume caches.
- **Pressure.** Every operation first applies the band's forced-shrink
  target for the clean-file class (§7h): shrunk to the low watermark
  at mild pressure and drained to zero from moderate on, before any
  `ramzip` handoff; growth only at normal pressure and never into the
  reserve. Inserts over the hard limit evict least-recently-used
  blocks down to the low watermark (hysteresis).
- **Coherence.** The cache sits on the device side of the sharing
  lock, so it observes every operation any window issues, serialised:
  a successful write refreshes the cached copies of the written blocks
  in place (admitting nothing new), a failed write invalidates the
  range (the device state is unknown — fail closed), and a discard
  invalidates its range. Reads spanning more than
  `LARGE_READ_BYPASS_BLOCKS` stream through uncached so a bulk bundle
  or driver-store load cannot flush the hot working set.
- **Sequential readahead.** The filesystem serves file content one
  block per iteration, so a cold sequential load (a program image, a
  bundle, the users database) would otherwise cost one device
  round-trip — one submit/park/wake on virtio or EMMC2 — per block.
  The cache detects a sequential stream (a miss whose LBA continues
  exactly where the previous request ended) and reads a bounded
  window ahead in a single coalesced device request, retaining it so
  the following blocks are hits; the window ramps 8→16→32→64 blocks
  (doubling per sustained sequential miss, capped at
  `LARGE_READ_BYPASS_BLOCKS`) and is clamped to the device end. It is
  a pure hint: random access disarms the ramp (scattered reads never
  over-read), a coalesced read that faults falls back to the exact
  requested span (a speculative over-read never widens a caller's
  fault), and a scratch reservation refused under pressure falls back
  to the exact read. Prefetched blocks still pass the pressure/budget
  admission gate.
- **Secret hygiene.** `BufferClass::Sensitive` reads and writes (key
  slots, credentials) bypass the cache entirely *and* evict any cached
  copy of their range, so no credential-bearing block is ever
  retained; every released buffer is volatilely wiped (invalidation,
  eviction, pressure shrink, poisoning, teardown).
- **Observability.** The §7k ledger/counters and audit events apply
  unchanged: the cache label is `block`, a classification refusal
  emits `RECLAIM_CACHE_REFUSED` (2000), and a detected ledger/index
  defect emits `RECLAIM_CACHE_POISONED` (2001) exactly once.

The host suite (`kernel/tairix-kernel/src/block_cache_tests.rs`)
proves classification, hit/miss/insertion accounting (a hit is shown
never to reach the device by corrupting the backing store),
write-through coherence, failed-write and discard invalidation,
sensitive-class scrubbing, the large-read bypass, sequential
readahead (a streaming read collapsing to a handful of device
round-trips, byte-correctness, prefetch-served-from-cache,
no-speculation-on-random-access, bypass-resets-the-run, and
coalesced-fault fallback), LRU eviction with
hysteresis, per-band growth/shrink/drain enforcement with recovery,
zero-backing refusal, uncacheable-geometry poisoning with the device
still serving, the closed audit field shape, and wipe-in-place.

## 7n. The encrypted compressed anonymous-memory tier (`ramzip`)

`plans/SWAPSWAPSWAP.md` — a near-zero-idle-cost, encrypted, compressed,
RAM-resident tier for cold anonymous pages, sitting *before* any
optional block swap in the pressure order:

```text
active RAM → ramzip → optional encrypted block swap → VM policy
```

It is a compressed memory tier, not magic extra RAM and not persistent
swap; the §7b block-swap layer is independent and shares neither key
nor metadata format with it.

- **Eligibility fails closed** (`ramzip::eligibility`). Only cold,
  unpinned, CPU-only anonymous user pages qualify; kernel stacks,
  interrupt stacks, page tables, DMA buffers, device memory, driver
  rings, crypto-key storage, credential metadata, sensitive or
  latency-critical pages, and pages of *unknown* role are refused with
  a typed reason. A mapping-flag defence (`NO_CACHE`/`DMA_COHERENT`)
  backs the classifier in depth.
- **Process pinning is the "pinned" attribute's source**
  (`mem_pin`/`mem_unpin`, `plans/STRESSTEST.md` ST2). A process holding
  `CAP_MEM_PIN` may mark its entire anonymous memory — current and
  future — as pinned, and the per-task registry's `is_pinned` mark is
  the one pin decision a candidate's owner is judged by, so a pinned
  process's pages carry the refusing `pinned` attribute above. The
  exemption is bounded by the `pinned-memory-bytes` limit (see
  [resource limits](resource-limits.md)) — the derived per-boot default
  grants one eighth of discovered RAM per process — never inherited
  across spawn, cleared on exit, and observable as the `pinned_bytes`
  aggregate in `RAMZIP_STATS` / `stats:mem/pinned`.
- **Derived capacity, no eager reservation** (`RamzipCaps`). Minimum
  guarantee `max(1% RAM, 64 MiB)` (clamped to the hard cap), soft cap
  10%, hard cap 25% — all fractions of discovered RAM, enforced per
  band (soft at moderate pressure, hard at severe, zero elsewhere).
  Construction allocates nothing: idle cost is one empty struct.
- **Compress, then seal.** A page is compressed through `lib/compress`
  first and only then sealed with `lib/crypto` AEAD under the tier's
  own per-boot [`SealKey`]/[`NonceSequence`] (§7b's shared `seal`
  primitives). The associated data binds the entry's identity (space
  id, page number, mapping flags), so replay against any other page,
  space, or permission set fails authentication. A page that does not
  compress below the acceptance bound (`PAGE_SIZE` minus sealing
  overhead minus the fixed per-entry metadata bound) is refused, never
  stored raw. Every plaintext temporary is zeroed on all paths, and
  the freed frame is scrubbed before it returns to the allocator.
- **Pressure integration.** Compression runs only where the §7h
  `ramzip_handoff` gate opens (moderate band with clean+transform
  caches drained, or severe), and never pushes free memory to the
  *decompression floor* (the §7h reserve plus fixed fault-in headroom):
  the tier can always restore what it holds. Refusals are typed
  (`CompressRefusal`) and feed the deterministic `escalate_refusal`
  policy (reclaim caches first; at moderate-or-deeper with caches
  drained, the VM policy owns the next step).
- **Move-only restore.** `fault_in` authenticates, decrypts,
  decompresses into a fresh frame, remaps with the original flags, and
  deletes the blob. Authentication or decode failure returns **no
  plaintext**: the entry is discarded, the (zeroed) frame returned,
  the loss audit-logged (events 2002/2003), and the typed error
  escalates through the VM policy.
- **Accounting is checked and tamper-independent** (`RamzipLedger`).
  Global and per-task books (logical/compressed/stored/metadata bytes,
  entries) with all-or-nothing checked arithmetic; releases use the
  figures charged at compression time, never a length recomputed from
  the (corruptible) blob — a regression the fuzz harness found and
  pinned. One task's share is bounded to half the active band cap.
  A ledger imbalance poisons admission (restores continue).
- **Clustering and warm-up are strictly budgeted.** After a demand
  fault, up to 8 neighbouring entries (same space, ±8 pages, sealed
  within 32 events) may be restored — only at normal pressure with
  free memory above the warm-up start watermark (§7h `warmup_start` /
  `warmup_stop` hysteresis) and the decompression floor protected;
  cluster failure never fails the original fault. A `warm_step`
  restores up to 8 entries near recent demand faults, re-checks the
  gate before every page, stops instantly on any pressure transition,
  and reports `NothingToDo` when no fault-locality evidence exists —
  cold pages stay compressed by design. Both are driven from the live
  fault path (§7r), foreground-only, so there is no warm-up daemon.
- **Thrash detection** is event-clock based and deterministic: restores
  of recently sealed entries score the owning task; over the threshold
  the tier refuses that task's pages until the score decays (halving on
  a fixed event cadence). No wall clock, no retry loop.
- **Enablement.** The tier is switched on for running tasks on every
  MMU-bearing Tier-1 port. The restartable user page-fault path exists
  (trap → restore → resume) and the referenced-bit facility (§7o) is
  live — x86_64 reads the hardware Accessed bit, aarch64/riscv64 manage
  it in software — so cold-page identification, compress-out (§7q), and
  the move-only fault-in, fault clustering, and warm-up restores (§7r)
  all run end to end. wasm32 keeps the fail-closed `Unsupported` default
  (no per-page referenced bit), so the sweep stays inert there.

Audit events (continuing the §7k catalogue, `kernel/mem` range
`2000..3000`): `RAMZIP_AUTH_FAILURE` (2002, Error) when a sealed entry
fails authentication on restore, and `RAMZIP_ENTRY_CORRUPT` (2003,
Error) when it fails metadata validation or decompression after
authenticating. Both carry numeric handles only (`space`, `page`,
`task`) — never page contents, keys, or nonces.

## 7o. Demand-paged file mappings (`file_map` / `file_unmap`)

`file_map` (`abi-v1` no. 75) maps a byte range of an open, readable,
filesystem-backed descriptor into the caller's own address space as a
**demand-paged, read-only private mapping** — the `mmap(2)` shape. It is how
a program views a file far larger than RAM (a 20 TB file on a 1 GiB machine
costs only the pages actually touched, `AGENTS.md` §26.7):

- **Reserve, don't read.** The handler validates the shape (page-aligned
  offset, non-zero length, no overflow), resolves the descriptor
  owner-checked against the kernel-trusted caller id (open for reading,
  `OpenBacking::Path` only — a resource/pipe has no positional bytes), and
  checks the projected total against the caller's shared
  `AddressSpaceBytes` accounting (one budget with `mem_map`). It then
  *reserves address space only* out of the task's file-mapping window
  (`LiveSpace`'s file `AnonWindowMap`, sized by `user_windows` §7f) and
  records a `FileRegion` — base, page-rounded length, resolved path,
  page-aligned file offset, and the **mapping-time identity** (uid +
  effective capability snapshot, the open-descriptor authority model; the
  mapping survives a later `fs_close`).
- **Fault-driven backing.** A user-mode **read** data fault is offered to
  the resident `DispatchHook::resolve_user_fault` before the fatal path
  (each port's `fault::set_user_fault_resolver`, installed beside the
  dispatch callback: the aarch64 lower-EL data-abort branch, the riscv64
  U-mode load-page-fault branch, and the x86_64 resumable `#PF` entry —
  which saves the interrupted GPRs, offers a user/not-present/read/data
  fault to the resolver under the timer path's `swapgs` convention, and
  on success restores and `iretq`s into a retry). The resolver attributes
  the fault to the scheduler's current task, looks up the covering
  `FileRegion`, reads the single covering page through the secured VFS
  **under the mapping-time identity** (owner/mode/ACL re-applied on every
  fault), and maps it read-only, never executable (`filemap::FILE_FLAGS`;
  a short tail page is zero-filled past end-of-file). The registry
  snapshot is re-frozen so the copy path sees the new page, and the task
  retries the faulting instruction. A **write** fault is offered through
  the same seam with the port-attested `write` verdict (aarch64
  `ESR.WnR`, riscv64 store/AMO `scause`, x86_64 `#PF` `W/R`) but is never
  *resolved*: file mappings are read-only, so it can never be made valid
  — resolving one against a resident page would retry the store into an
  endless fault storm — and the one shared hook policy sends every write
  (like every unresolvable read) down the task-kill path, so a store to a
  read-only mapping or any wild user write costs the task, never the
  CPU. The demand-grown **stack** resolver (§7c) is offered *before* this
  write-fatal file rule, for reads and writes alike — a stack push is a
  write — so a legitimate growth fault inside the task's recorded stack
  span is backed, never killed. An instruction fetch, and any other
  synchronous EL0 exception the specific handlers do not resolve — an
  illegal/unallocated instruction, an alignment fault — is never offered
  to the demand-paging resolver (retrying it would re-take the exception
  forever); the port instead routes it to a resolution-free terminate path
  (`DispatchHook::terminate_user_fault`) that kills the offending task and
  keeps the CPU alive. Only a genuinely unrecoverable *kernel* (same-EL)
  exception, or one with no running task to attribute, halts the CPU — a
  user task's own bad instruction never parks the core.
- **Fail closed, kill the task not the machine.** An address outside every
  region, a page wholly at/past end-of-file (the `SIGBUS` analogue), a
  filesystem error, or frame exhaustion terminates the *faulting task*: the
  hook records exit code 139 (`128 + SIGSEGV`), reclaims exactly what
  `exit`/signal-kill reclaim, and the port suspends it with an `Exit`
  action. The kill is audited with the stable `TaskFaultKilled` event
  (kernel/core id 4034): task id plus a coarse `fault_class`
  (`stack_limit` / `stack` / `file_region` / `anon` / `wild`) and a
  non-leaking `fault_offset` locality bucket (`null_page` /
  `below_stack_guard` / `region` / `in_region` / `wild`), never the raw
  address — so a crashing program
  is visible on the system log, not only via its `wait` status. A miss
  *inside* memory the task legitimately reserved (the deterministic
  out-of-memory case — with commit accounting this should not arise for
  anonymous memory, since `mem_map` fails first, but the kernel still
  reports it honestly) is `fault_class=anon`/`file_region`/`stack` with
  `fault_offset=in_region`, **never** the misleading `wild`, which is
  reserved for an address outside every mapping. A fault
  with no attributable task falls back to the fatal halt. A page already
  resident (a concurrent resolution) is a benign race and simply resumes.
- **Release is sparse.** `file_unmap` (no. 76) releases only the exact whole
  `(base, len)` region the caller mapped (validated against the per-task
  region table before any teardown): resident pages are unmapped and their
  frames zeroed on free, never-touched holes cost nothing, the accounting
  is credited, and the snapshot re-frozen so freed pages leave it. Task
  exit reclaims resident pages through the live space's drop and the region
  records through the registry withdraw.
- **The kernel copy path resolves misses too.** A syscall buffer inside an
  untouched mapping works without pre-touching (`write(fd, mapped, n)`):
  the copy walk reports the missing page's base
  (`UaccessError::NotMapped { va }`), and the one fault-aware staging
  helper (`KernelSyscallHandlers::copy_in_user`, used by every handler
  that copies a user buffer in) offers exactly that page to the same
  resolvers — the file resolver, then the stack-growth resolver (§7c),
  over the same region/span tables and identities the hardware path
  uses — releasing
  the registry guard around each resolution and retrying under a budget
  of one resolution per touched page (an unresolvable miss stays the
  stable `BadAddress`). Copy-*out* is unchanged: file mappings are
  read-only, so a write into one fails closed regardless. aarch64,
  riscv64, and x86_64 all resolve faults today and pass their
  `user_windows` file window at spawn; wasm32 has no user-fault source
  and reserves nothing.
- **Tested.** `kernel/mem` `filemap` engine tests (fill/zero-tail/W^X/
  scrub-on-error, sparse release), `LiveSpace` file-region tests
  (reserve/fault/read-back/release, fail-closed coverage checks, drop
  reclaim), `kernel/core` handler + resolver tests (shape/fd/limit
  refusals, exact-match unmap, page read at the right file offset, EOF and
  foreign-task refusal, benign-race fold), the `copy_in_user`
  resolve-retry tests (miss offered + bounded, outside-region and
  resident-page behaviour), the fault-kill audit test, per-port fault
  classification tests (aarch64 `is_write_data_abort`, riscv64
  `is_load_page_fault` / `is_store_page_fault`, x86_64
  `is_user_data_fault` / `is_resolvable_user_fault` + resolver-slot
  round-trip), dispatch-core fault-forwarding tests (including the
  write-fault-is-terminated regression), and the `lib/rt` wrapper
  marshalling tests. The QEMU end-to-end verticals
  (`tests/integration/file_map_qemu_aarch64` / `…_riscv64`, over the
  shared four-role fixture program `tests/integration/file_map_program`)
  prove the whole path live on both boards through the production
  `KernelDispatchHook`: demand-fault of the first/interior/EOF-straddle
  pages with byte and zero-fill verification, the mapping surviving
  `fs_close`, an untouched mapped page handed to `fs_open` as its path
  buffer (the copy-path proof), sparse `file_unmap`, a wild read after
  unmap fault-killed with exit 139, and a store to the resident read-only
  mapping fault-killed with exit 139 — each observed by a parent through
  the production `spawn` + `wait`.

## 7o. Cold-page identification (referenced bit + `coldscan`)

Switching the §7n `ramzip` tier on for arbitrary *running* tasks needs a
way to tell a page the task still uses from one it has abandoned, so
compression relieves pressure instead of evicting a hot page straight
back into a fault. That is a page-replacement referenced-bit facility,
and its architecture-neutral core lands here (staged in
`.junie/swapswap-progress.md`).

- **The HAL primitive.** `tairix_arch_api::mmu::AddressSpace` gains
  `test_and_clear_accessed(vaddr)` — read *and clear* the leaf's
  per-page referenced (accessed) bit, returning whether it had been set,
  and invalidate the page's TLB entry so the next access re-sets it — plus
  an honest `access_tracking()` declaration (`AccessTracking::Supported`
  / `Unsupported(reason)` / `Pending(reason)`, the same honesty discipline
  as `BlockSplit`, memory tagging, and side-channel mitigation). The
  default is fail-closed: a port that does not maintain a referenced bit
  declares it non-`Supported` and `test_and_clear_accessed` returns
  `MapError::Unsupported`. The MMU conformance vertical
  (`mmu::conformance::run_all`) checks the declaration is honest — a
  non-supported port must carry a non-empty justification and must fail
  the primitive closed rather than fabricate an access verdict.
- **The scanner.** `kernel/mem::coldscan::ColdPageScanner` is the classic
  **second-chance (clock)** page-replacement scan built on that primitive.
  It keeps a rotating clock hand across passes and, for each candidate
  page in clock order, reads-and-clears the referenced bit: a page found
  *set* was touched since the last pass and is given a second chance (its
  bit is now cleared, it stays mapped); a page found *clear* went
  untouched across a full pass and is returned as a cold reclaim
  candidate, up to the caller's budget. On a port whose `access_tracking`
  is not `Supported` the scan returns `ColdScanError::Unsupported` and the
  tier reclaims nothing there — reclaim is safe by omission, never by
  guessing (the fail-closed rule; a false-cold classification would cause
  the very thrash the tier avoids). This is the same approximation Linux
  page reclaim uses, with no per-page timestamp and no hot-path
  allocation beyond the returned list.
- **Per-port state.** Three of the four Tier-1 ports are live and declare
  `Supported`; wasm32 stays fail-closed by construction.
  - **x86_64**: the hardware always sets the Accessed bit (PTE bit 5) and
    never clears it (Intel SDM Vol 3A §4.8), so `test_and_clear_accessed`
    walks to the 4 KiB leaf, reads and clears bit 5, and `INVLPG`s the
    page — no software fault path. `flags::ACCESSED` is the single
    definition of bit 5.
  - **aarch64**: the Access Flag (AF, descriptor bit 10) is
    software-managed — cortex-a57/a72 (the boards and the default QEMU
    CPU) lack ARMv8.1 HAFDBS, so an access to a valid leaf whose AF is
    clear raises an Access-Flag fault. `test_and_clear_accessed` clears AF
    (+ TLBI); the synchronous-exception path
    (`is_access_flag_fault` → `paging::set_accessed_flag_in_active`) sets
    AF back on the faulting leaf and retries. Both data and instruction
    aborts, from either EL, are handled; a non-AF fault falls through
    unchanged (fail closed).
  - **riscv64**: the Accessed bit (A, PTE bit 6) is likewise
    software-managed — RISC-V leaves A/D *update* implementation-defined
    (a Svade part faults, a Svadu part updates in the walk).
    `test_and_clear_accessed` clears A (+ `sfence.vma`); the trap path
    (load/store/instruction page fault → `paging::set_accessed_flag_in_active`
    with the `AccessKind`) sets A (and D for a store) back **only** on a
    valid leaf that permits the access — a genuine permission fault (same
    `scause`) is never masked — and retries. On a Svadu part the fault
    never fires and the branch is inert.
  - **wasm32** keeps the fail-closed default permanently: the browser
    sandbox exposes no per-page referenced bit.
  The `HostPageTable` double models the bit in software, so the scanner is
  fully host-tested on every target.
- **Tested.** `kernel/arch/api` mmu conformance (honest declaration,
  fail-open rejection), `kernel/mem::coldscan` host tests (untouched
  pages are cold up to budget, a referenced page gets a second chance and
  the cleared bit makes a still-idle page cold next pass, the clock hand
  rotates, and a backend without a referenced bit fails closed), the
  per-port paging host tests (the clock round-trip and the AF/A
  fault-fix-up + permission gating on aarch64 and riscv64), and a QEMU
  vertical per live port —
  `tests/integration/accessed_bit_qemu_{x86_64,aarch64,riscv64}`. Each
  drives the full clock transition on real (emulated) hardware — a fresh
  mapping's bit, a clear making the page read cold, a genuine access
  re-setting it (through the software fault path on aarch64/riscv64,
  proven because the aarch64 run uses cortex-a72 without HAFDBS and the
  riscv64 run pins `svade=true,svadu=false`), and a re-access after the
  next clear — plus the misaligned / unmapped fail-closed rejects and the
  `Supported` declaration.

## 7p. Live tier wiring (global pool, fault-in, boot install)

The §7n mechanism is wired into the running system as a single
**process-global** pool (`kernel/mem::ramzip::global`, an
`OnceCell<SpinLock<Ramzip>>`): one instance, keyed by `(space_id, page)`
with a per-task ledger and total-RAM-derived caps, matching how the tier
was designed (per-task fairness across one shared pool, not a tier per
process). The global free-memory decompression-floor check on every
`compress_out` upholds the reserve invariant, so the per-space band caps
are a fairness bound rather than the safety bound.

- **Ownership.** `kernel/mem::LiveSpace<P, M>` (the retained per-task live
  address space) carries a `space_id` (a monotonic id, PID-style) and its
  own `ColdPageScanner`, and exposes two object-safe `LiveUserSpace`
  operations that take the tier lock explicitly: `ramzip_fault_in`
  (move-only restore; `Fatal` on authentication/decode/OOM — fail closed,
  audited, no plaintext) and `ramzip_reclaim` (candidate set = resident
  *placed-anonymous* pages the heap-window allocator proves anonymous;
  `ColdPageScanner` decides cold, failing closed on a non-`Supported`
  backend; the tier's own gates admit). `LiveSpace::drop` purges the
  global tier of the dead space's entries (no leak of RAM or ledger
  charge).
- **Fault path.** `kernel/core::resolve_user_fault` offers
  `resolve_ramzip_fault` **before** the anonymous handler — a compressed
  page is reserved anonymous memory the anon handler would otherwise
  re-zero — and republishes the restored page to the registry snapshot as
  the anon path does. `Fatal` terminates only the faulting task; `NoEntry`
  (or no installed tier) falls through.
- **Boot install.** `init.rs::install_ramzip_tier` brings the tier online
  in the CSPRNG-reserve-seed success branch: caps from discovered RAM, the
  per-boot key drawn from the seeded reserve through a
  `RandomReserve → seal::EntropySource` adapter, fail closed (a failed draw
  leaves no tier, and the compressed path stays inert). The `RAMZIP_STATS`
  feed is registered by `memstats::install_global_ramzip_stats`.
- **Tested.** `kernel/mem::ramzip::global` stats projection tests and the
  `LiveSpace` live-wiring tests over `HostPageTable` (which is
  `Supported`): compress→fault round-trip restoring exact bytes,
  placed-anonymous-only candidate selection, Normal-pressure refusal,
  `NoEntry` fall-through, `want`-budget honouring, and a frame-neutral
  reclaim+fault cycle.

## 7q. Compress-out trigger (foreground direct reclaim)

The compress-out half is driven by **foreground direct reclaim**: TAIRiX
compresses cold pages out at demand-fault time, in the faulting task's own
context, exactly as a general-purpose kernel reclaims on the page-allocator
slow path. There is no separate reclaim daemon to schedule or wake — the
moment a task is about to commit another frame is precisely the moment to
free some, and making the *faulting* task reclaim its *own* cold pages
charges the cost to whoever is driving pressure (per-task fairness,
`plans/SWAPSWAPSWAP.md` sections 6, 10).

- **The trigger.** `kernel/core::resolve_user_fault` calls
  `KernelSyscallHandlers::ramzip_direct_reclaim` once, before it resolves
  the fault (so freed frames are available to back it). The step samples
  the shared pressure gauge (`memstats::MEM_STATS.current_pressure`) and
  asks `pressure::ramzip_reclaim_batch(band)` for a bounded page budget:
  **zero at normal/mild** (cheaper cache reclaim owns those bands) and
  **zero at critical** (escalation hands off to the VM policy, not more
  compression), **32 pages at moderate** and **128 at severe**. A zero
  budget returns after a single gauge read, so the fault path pays almost
  nothing off the compression bands.
- **The template.** The task's compression template is built from facts
  the kernel owns: a **pinned** task (`aspaces.is_pinned`) yields nothing
  (its pages must stay resident) and a **real-time** task
  (`sched.sched_class(..).is_realtime()`) is latency-critical and never
  compressed. Ordinary anonymous-heap sensitivity is covered by the tier's
  eligibility gate — kernel secrets never live in a task's placed-anonymous
  window.
- **Bounded, fail-closed, never a spin.** The sweep compresses at most the
  band's batch of the faulting task's cold placed-anonymous pages through
  `LiveSpace::ramzip_reclaim` (which now short-circuits *before* its O(n)
  candidate walk when the port exposes no referenced bit, so direct reclaim
  is near-free there too). The tier's own gates (handoff ordering against
  the clean+transform `reclaimable_residue`, caps, per-task share,
  decompression floor, eligibility) decide what is actually admitted; a
  refused page stays resident. Nothing loops or retries — one bounded pass
  per fault.
- **Snapshot republish.** A sweep that compressed any page freed its frame
  and dropped its PTE, so the registry snapshot is re-frozen once
  afterwards (several pages changed at once), the batched analogue of the
  per-page republish `resolve_ramzip_fault` does on restore, keeping the
  copy path from ever reading a freed frame.
- **Live on every MMU-bearing port (§7o).** x86_64, aarch64, and
  riscv64 all declare `AccessTracking::Supported`, so the cold-page
  scanner sees genuinely idle pages and direct reclaim compresses cold
  anonymous pages out end to end on hardware; wasm32 keeps the
  fail-closed `Unsupported` default, so the sweep stays inert there. The
  trigger, policy, template, residue gate, and snapshot republish are
  port-agnostic — a port needs no per-port change once its referenced
  bit is live. `HostPageTable` (also `Supported`) exercises the whole
  path in host tests.
- **Tested.** `pressure::ramzip_reclaim_batch` band mapping (zero off the
  compression bands, severe reclaims harder than moderate, non-zero only
  where the handoff opens), `memstats::current_pressure` (none until the
  gauge is created, then the one shared gauge) and
  `memstats::ramzip_reclaimable_residue` (only clean-file + transform bytes,
  payload and metadata), the `LiveSpace::ramzip_reclaim` access-tracking
  early-out, and the `KernelSyscallHandlers::ramzip_direct_reclaim`
  fail-closed no-op before the tier is installed. The end-to-end
  compress→fault behaviour the trigger drives is the §7p `LiveSpace`
  live-wiring suite.

## 7r. Fault clustering and opportunistic warm-up (live)

The §7n clustering and warm-up mechanisms are driven from the live
fault path, so a task resuming from the tier gets its working set back
without a fault per page — the read half's analogue of the §7q
compress-out trigger, and, like it, foreground-only: there is no
warm-up daemon to schedule, wake, or busy-poll. The cost is charged to
the resuming task, at the one moment it is provably using formerly
compressed memory.

- **The trigger.** After `resolve_ramzip_fault` restores the faulting
  page (§7p), and only then, it samples the shared pressure gauge once
  and — through the object-safe `LiveUserSpace::ramzip_cluster` and
  `ramzip_warm` seams — runs fault clustering around the faulted page
  and one bounded warm step over entries near recent faults. A task not
  resuming from the tier (`NoEntry`) never reaches this code, so it
  pays nothing.
- **Comfort-gated, reserve-safe.** Both restore only at normal pressure
  with free memory above the warm-up start watermark (`warmup_start` /
  `warmup_stop` hysteresis, §7h) and re-check the decompression floor
  before every page, so they never run under pressure and can never be
  the cause of renewed pressure — the §7n / `plans/SWAPSWAPSWAP.md` §11,
  §12 invariant. Clustering is bounded to ±8 pages sealed within 32
  events of the faulted entry; a warm step to 8 pages within ±64.
- **Best-effort, never fatal.** A cluster/warm restore failure never
  propagates: the original fault already succeeded. An authentication
  or decode failure is audit-logged and yields no plaintext, exactly as
  on the demand path.
- **Snapshot republish.** A warm restore remaps several pages at once,
  so the registry snapshot is re-frozen once when any page was brought
  back; the demand page's single-page delta covers the common case.
- **Tested.** The `LiveSpace` live-wiring suite over `HostPageTable`
  (`Supported`): a demand fault plus clustering restores exactly the
  contemporaneous neighbours when comfortable and nothing under
  pressure; a warm step restores near recent faults only with locality
  evidence and comfort, and stops immediately under pressure; both are
  fail-closed no-ops on an empty tier.

## 7s. Performance evidence (`ramzip`)

`plans/SWAPSWAPSWAP.md` §19 requires the tier to ship with performance
evidence, reported as estimates rather than guarantees. The evidence
lives beside the tier as host benchmark tests
(`kernel/mem::ramzip::tier::tests::bench_evidence_*`), following the
repository's established style (`kernel/core`'s `bench_evidence_*`): the
*deterministic assertions* prove the work the tier is meant to do, and
the *printed wall-clock figures* are estimates for threshold tuning. They
run over the same production shapes as every other tier test — a real
`FrameAllocator`, `SimPhysMap`, and `HostPageTable` — so the numbers
reflect the actual compress → seal → store → fault → restore pipeline,
not a stub.

- **Memory saved (deterministic).** Compressing 48 cold, compressible
  anonymous pages represents their full logical size (196 608 B) while the
  tier's accounted footprint (stored ciphertext + metadata) stays far
  below it; the test asserts *over half* is saved and observes ≈ 94 %
  (≈ 12.5 KiB stored for 192 KiB logical) on the developer host. This is
  the reason the tier exists, and it is checked, not merely timed.
- **Move-only, leak-free round trip (deterministic).** After faulting
  every page back in, no compressed entry is retained, the ledger balances
  to zero, and the frame allocator returns to its pre-compression free
  count — the write half never leaves a duplicate copy or a leaked frame.
- **Latency estimates.** Compress-out and fault-in are timed on a Pi-class
  2 MiB profile and a desktop-scaled 4 MiB profile (both from the one
  harness via `Env::with_total_frames`), and the fault-clustering restore,
  severe-band compression, and worst-case incompressible-page refusal are
  timed separately, so a future threshold change can be judged against
  real figures. On the developer host the 48-page compress-out and
  fault-in each run in the sub-millisecond range and a single
  incompressible-page refusal in tens of microseconds; these are
  estimates, and the band watermarks and caps (§7h, `ramzip::caps`) stay
  implementation constants, never ABI.
- **Worst-case and both pressure bands.** The incompressible workload is
  refused (`CompressRefusal::Incompressible`, never stored raw), and
  compression is exercised under both the moderate (ordinary relief) and
  severe (emergency growth) bands the handoff opens.

## 7t. The desktop UI cache and cooperative reclaim (SMART5)

The desktop rasterises vector assets — pointer cursors, notification
glyphs, icons — at the active scale and theme. Each rasterisation is
expensive and the result is pure derived state, so it is exactly the
`DisposableUi` class of §7g. The problem is that the desktop runs in
*userland*: it cannot see free frames, watermarks, or the reserve floor,
and it must not be able to. This section is how it obeys the same
reclaim policy anyway (`plans/SMARTRAM.md` SMART5, section 6.4).

- **One model, two vantage points.** `PressureGauge` has exactly two
  implementations. `MemoryPressure` *measures*: it samples the frame
  allocator and folds the reading into a band with hysteresis (§7h).
  `ReportedPressure` *receives*: it holds the band the kernel reported
  and answers from that. Both drive the same `shrink_target`, so a
  desktop cache and a kernel cache shrink identically. A
  `ReportedPressure` that has not been told anything answers
  `critical` — an unwired process admits nothing to its caches and
  renders everything on demand, rather than assuming the machine is
  comfortable.
- **One cache implementation.** `tairix_reclaim::ReclaimCache<K, V, E>`
  is the single bounded, generation-invalidated, pressure-governed LRU
  cache both sides use. It charges the §7g ledger, wipes non-public
  entries before release, evicts oldest-first through an O(log n)
  recency index, and poisons itself — draining and serving uncached
  for the rest of its life — if a charge or discharge ever fails to
  balance. When it cannot admit a value it still returns a usable one
  (`Served::Uncached`), so a caller never has to handle "caching was
  unavailable" and no path rasterises twice.
- **Notification, not polling.** `WaitSourceKind::MemoryPressure` (wire
  value 9; its `id` is always `0`, since the machine has one band) is an
  edge-triggered wait-set source. The gauge's band-change hook fires
  only on a *stored* change and only flags `waitq::PRESSURE_WAITQ`
  lock-free: it can be reached from inside the frame allocator or a
  demand fault, so taking a lock there could re-enter one the
  interrupted allocator already holds. The real unpark runs at the next
  dispatcher-context `drain_pending_wakes`, exactly like a device IRQ's
  wake, and `has_pending_deferred_wake` includes it so a fired tick on a
  lone-task CPU still reschedules to deliver it.

  Readiness compares the *published* band against the band the member
  last observed, and reporting the member advances it. A band that
  deepens and relaxes again before the waiter runs therefore correctly
  reports nothing to do — the waiter's view is already right. A member
  added while the machine is already tight baselines on the band in
  force, so it stays quiet until something actually moves. No capability
  is required and a non-zero `id` is refused.
- **A band-only read to drain the edge.**
  `SysinfoQueryId::MEMORY_PRESSURE_BAND` returns the published band and
  nothing else, taking no reading — an unprivileged caller must not be
  able to drive a free-memory sample on demand. It is ungated and
  unaudited: it is a coarser disclosure than the already-ungated
  `LOAD_AVERAGE` (which reports the live task census and the logged-in
  user count), it carries no per-task, per-user, or byte-level figure,
  and withholding it would not protect anything — it would simply make
  cooperative reclaim impossible and leave the process to be reclaimed
  *against*. The privileged, audited `MEMORY_PRESSURE` view (free and
  total bytes, every watermark, the per-band transition history) is
  unchanged.
- **The process gauge.** `tairix_rt::pressure::gauge()` is the one
  `ReportedPressure` per process, so every cache in a program shrinks
  together. The runtime deliberately does not fetch the band itself:
  reading it needs a System Information endpoint and transport the
  runtime has no business choosing for a program. The owning program
  parks on the wait source, reads the band, and calls
  `tairix_rt::pressure::report`.
- **The desktop's caches.** The window manager's cursor cache, the
  taskbar's notification-glyph cache, and the session's pinned-artwork
  cache are built from one policy,
  `tairix_reclaim::desktop::disposable_ui_cache`: class `DisposableUi`,
  owner `ReclaimOwner::DesktopSession { seat }`, sensitivity `UserData`
  (so every released entry is overwritten), invalidation by a
  `(scale, theme)` generation token, `Drop` on reclaim. The budget is
  derived from the discovered framebuffer byte size, so a 4K output gets
  a proportionately larger cache than a 640×480 one and no hand-picked
  ceiling exists. The policy lives in `lib/reclaim` because the window
  manager and the taskbar may not depend on each other or on the
  session, and that is the only crate all three already share. The window
  manager's rendered window furniture is a fourth cache of the same class
  and owner, differing only in its ceiling
  (`tairix_reclaim::desktop::window_chrome_cache`): one screenful of
  pixels rather than the small fraction a cursor or a glyph is allowed,
  because no more furniture than fills the screen can be visible at once.

  There is exactly one constructor and it demands the real backing size,
  the real gauge, and the real audit sink; each consumer takes its cache
  as a constructor argument and the session assembles it. A convenience
  constructor that defaulted the gauge would produce a cache that
  classifies and serves correctly while retaining nothing — software
  that looks like it works and silently rasterises every frame.
- **Window content is a release policy, not a cache.** A window's content
  surface is the desktop's largest single allocation, so it too is given
  back under pressure — over the *same* gauge and the *same*
  `shrink_target` ordering. It is deliberately not keyed and not
  recency-driven: evicting a visible window's pixels is a visual defect
  rather than a slowdown, so the ladder follows what the user can see —
  hidden and minimised windows at mild, visible-but-unfocused ones at
  critical, never the focused window. The buffer is wiped before it is
  dropped and the owning app is asked to present again through the window
  protocol's redraw handshake, which its client library answers for it.
  One memory model, two mechanisms for two different kinds of memory
  (see [Releasable window content](../desktop/wm.md#releasable-window-content)).
- **Teardown wipes.** Logout or seat revocation tears the caches down and
  wipes every window's content, overwriting every retained entry, so one
  session's rendered user data cannot outlive its seat in reusable heap.
- **Headless is unaffected.** The caches live in `userland/gui/*`; a
  headless image contains none of them, and the kernel carries no
  desktop-specific pressure policy.

`lib/raster`'s former `RasterCache` is deleted rather than kept
alongside: it was unbounded, scanned linearly, never shrank under
pressure, was invisible to the reclaim ledger, and never wiped rendered
user data.

## 8. Testing strategy

- **Unit tests** — alongside each module under `#[cfg(all(test, not(loom)))]`:
  buddy split/merge, bitmap correctness, slab guard-page detection,
  zero-on-free, OOM paths, the encrypted-swap round-trip /
  tamper-rejection / fail-closed cases, and the `uaccess` user-memory
  copy (cross-page, mid-page-offset, round-trip, and every fail-closed
  branch over `HostPageTable` + `SimPhysMap`).
- **Property tests** — `kernel/mem/tests/proptest_frame.rs` runs
  randomised alloc/free sequences and asserts the no-double-allocation
  and no-leak invariants, plus the reserved-frame untouchability
  invariant.
- **Fuzzing** — `kernel/mem/tests/fuzz_swap.rs` drives the encrypted-swap
  restore path with arbitrary device contents (`AGENTS.md` §19.6),
  asserting that tampering is always rejected and the output buffer is
  zeroed on failure. `kernel/mem/tests/fuzz_ramzip.rs` does the same
  for the §7n tier: random compress → tamper/truncate → fault cycles
  over the host doubles, asserting fail-closed restores, faithful
  untampered round-trips, and books that balance to zero after every
  cycle.
- **Loom tests** — `kernel/mem/tests/loom.rs` model-checks concurrent
  allocation, gated on `RUSTFLAGS="--cfg loom"` exactly like
  `lib/sync`.

[`BootMemoryMap`]: ../../tairix_kernel_mem/struct.BootMemoryMap.html
[`AddressSpace<P: PageTable>`]: ../../tairix_kernel_mem/struct.AddressSpace.html
[`PageTable`]: ../../tairix_kernel_mem/trait.PageTable.html
