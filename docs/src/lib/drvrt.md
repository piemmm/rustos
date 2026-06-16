# `rustos-drvrt`

`lib/drvrt` is the **user-space driver runtime host**: the rt-backed
`DriverHost` a first-party driver process links so it can run in user space
(the `AGENTS.md` §4 microkernel goal) instead of in the kernel. It is the
user-space counterpart of the in-kernel keyboard service's `IdentityMmioMapper`
+ frame-allocator DMA host, built on the `mmio_map` / `dma_alloc` syscall
surface delivered in `plans/PI.md` P10 chunk 5d-0.

## Why it exists

An in-kernel driver's host maps device memory by reaching the kernel's frame
allocator and identity map directly. A user-space driver cannot — and must
not (§4: no ambient authority). Instead, when the device manager autoloads a
driver for a hardware-tree node (§18.3), the kernel mints the driver one
unforgeable, owner-checked **grant handle** per `HwResource` the node requested
(a register window, an outbound bus window, a DMA constraint) — and no more.
The driver maps a register window with `mmio_map(handle)` and carves a coherent
DMA buffer with `dma_alloc(handle, len, …)`, passing those handles. Every
capability check and every bound is enforced kernel-side, on the far side of
the trap (§5.4).

`rustos_drvrt::RtDriverHost` turns that grant table into the three traits a
bus driver's `register()` consumes:

- **`MmioMapper`** — `map_window(phys_base, len)` finds the grant whose window
  covers the request, maps that grant's whole window once with `mmio_map`
  (caching the base so a window is never mapped twice, §2.16), and returns a
  `RegisterWindow` at the in-window offset. For an outbound
  `HwResourceKind::BusWindow` grant it translates the BAR's PCIe-bus address to
  the mapped CPU window — the bridge's bus→CPU translation (§18.1), performed
  here rather than in the architecture-neutral PCI walk.
- **`VirtioHost`** — `alloc_dma_zeroed(size)` carves the device-shared DMA
  region with `dma_alloc` against the DMA grant and returns a `DmaSlab` whose
  `phys()` is the device-visible base the controller programs. A non-coherent
  interconnect's cache-maintenance shim (e.g. the BCM2711 PCIe master) is
  supplied by the architecture-aware driver process, never synthesised here, so
  the crate stays platform-neutral (§2.20). It deliberately provides no virtio
  queue-completion wait: the host serves a polling / `irq_wait`-driven driver.
- **`DriverHost`** — reports the load-time capability set
  (`has_capability`), `DriverKind::UserSpace`, and hands its own `MmioMapper` /
  `VirtioHost` back through `mmio_mapper()` / `virtio_host()`.

## Not a privileged path

The host adds no authority. It only resolves a driver's request to the grant
handle the kernel already minted and issues the syscall; a forged or another
task's handle resolves to nothing kernel-side and is refused. The up-front
capability check fails fast without a round trip — the kernel re-checks
regardless (§5.4).

## Fail-closed and allocation-free

`no_std` and allocation-free (the grant table is a fixed `MAX_GRANTS` array),
so a driver process works before the userland heap is available
(`plans/SPAWN.md` `SP5b`). A missing capability, an unmappable request, a
window no grant covers, an over-long grant table, or a kernel refusal returns
an error — never a fabricated pointer or a panic (§2.9). There is no userland
DMA-free syscall: a carved buffer lives for the driver process's lifetime and
the kernel reclaims it at exit (`LiveSpace::Drop`, §4), so the slab's drop is a
no-op — the same "alloc once, never free" contract the in-kernel frame DMA host
uses.

## Testing seam

The two syscalls live behind the `GrantSyscalls` trait, so the host's grant
resolution, bus→CPU translation, map-once caching, and every fail-closed path
are unit-tested on the host without a kernel (§7). Production driver processes
use `RtGrantSyscalls`, which forwards to `rustos_rt`'s `mmio_map` / `dma_alloc`
wrappers — the one syscall trap (§2.2).

## Stability

Tier: `experimental` (see the crate `README.md`).
