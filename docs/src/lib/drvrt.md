# `rustos-drvrt`

`lib/drvrt` is the **user-space driver runtime host**: the rt-backed
`DriverHost` a first-party driver process links so it can run in user space
(the `AGENTS.md` §4 microkernel goal) instead of in the kernel. It is the
user-space counterpart of the in-kernel keyboard service's `IdentityMmioMapper`
+ frame-allocator DMA host, built on the `resource_grants` / `mmio_map` /
`dma_alloc` syscall surface delivered in `plans/PI.md` P10 chunks 5d-0 / 5d-2.

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

The driver process learns *which* handles it holds by calling `resource_grants`
at start-up: the kernel serialises the task's minted grant set (handle +
`HwResource` per record) and the host decodes it. `RtDriverHost::from_grants_query`
is the production constructor that issues that syscall into a fixed
`MAX_GRANTS` buffer and builds the grant table from the delivery — the path a
`devmgr`-autoloaded driver uses. (`RtDriverHost::new` takes a caller-supplied
grant slice instead, for tests and verticals.)

`RtDriverHost::resources()` exposes the granted `HwResource`s read-only, so a
driver derives its concrete bring-up inputs — its register BAR window and DMA
aperture bound — from the same grant set the host maps over, without a second
`resource_grants` syscall (§2.16). The USB keyboard driver process
(`drivers/input/usb_kbd`) uses it with
`rustos_hid::derive_keyboard_resources` to fill the
`bar_base`/`bar_len`/`dma_aperture_top` its bring-up needs (`plans/PI.md` P10
chunk 5d-2-ii).

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
  `VirtioHost` back through `mmio_mapper()` / `virtio_host()`. Its `emit_node`
  forwards to the `hw_emit_node` syscall, so a user-space **bus** driver
  publishes each device it enumerates into the live hardware tree and the
  device manager autoloads the matching driver in turn (recursive,
  data-driven discovery — `AGENTS.md` §18.1 / §18.3). The host adds no
  authority: the kernel admits the node only when every requested
  `HwResource` is covered by one of this driver's own grants, so a child can
  never carry more authority than its emitter (§4); a refusal surfaces as
  `DriverError::PermissionDenied`.

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

The syscalls (`resource_grants`, `mmio_map`, `dma_alloc`, `irq_bind` /
`irq_wait`, `ipc_call`, and `hw_emit_node`) live behind the `GrantSyscalls`
trait, so the host's grant delivery decode, grant resolution, bus→CPU
translation, map-once caching, node publishing, and every fail-closed path are
unit-tested on the host without a kernel (§7). Production driver processes use
`RtGrantSyscalls`, which forwards to the matching `rustos_rt` wrappers — the
one syscall trap (§2.2).

## Stability

Tier: `experimental` (see the crate `README.md`).
