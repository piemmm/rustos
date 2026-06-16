# rustos-drvrt

User-space driver runtime host for RustOS (`lib/drvrt`, `AGENTS.md` §6 / §2.2
— `plans/PI.md` P10 chunk 5d).

A first-party driver that runs in **user space** (the §4 microkernel goal) is
handed the same `DriverHost` surface its `register(host: &dyn DriverHost)`
entry expects, but the concrete host can no longer reach the kernel's frame
allocator or identity map directly. `RtDriverHost` is that host: it maps a
**granted** device resource over the `abi-v1` syscall surface (chunk 5d-0).

When the device manager autoloads a driver for a hardware-tree node
(`AGENTS.md` §18.3), the kernel mints the driver one unforgeable handle per
`HwResource` that node requested — and no more (§4). `RtDriverHost` holds those
grants and:

- implements `MmioMapper` — it resolves a bus driver's requested
  `(phys_base, len)` register window to the grant that covers it, maps that
  grant's window once with the `mmio_map` syscall, and hands back a
  `RegisterWindow` at the right offset (translating an outbound PCIe-bus BAR
  address to the mapped CPU window, `AGENTS.md` §18.1);
- implements `VirtioHost` — it carves the device-shared DMA region with the
  `dma_alloc` syscall, bounded by the grant's addressing constraint, and
  returns a `DmaSlab` carrying the device-visible base;
- reports the load-time capability set and `DriverKind::UserSpace`.

## API

- `RtDriverHost::new(caps, syscalls, grants, coherency)` — build a host over
  the granted capability set, a `GrantSyscalls` provider, the kernel-issued
  grants, and an optional non-coherent-interconnect cache shim.
- `GrantedResource::new(handle, resource)` — pair a grant handle with the
  `HwResource` it names.
- `GrantSyscalls` — the `mmio_map` / `dma_alloc` seam; production code uses
  `RtGrantSyscalls`, which forwards to `rustos_rt`.

## Design

- `no_std`, **allocation-free** — the grant table is a fixed `MAX_GRANTS`
  array, so a driver process works before the userland heap is available
  (`plans/SPAWN.md` `SP5b`).
- **Not a privileged path** (§5.4): the host adds no authority. It only names
  the grant handle the kernel already minted and issues the syscall; every
  capability check and bound is enforced kernel-side, and a forged or another
  task's handle resolves to nothing. The up-front capability check only fails
  fast; the kernel re-checks regardless.
- **Fail-closed** (§2.9): a missing capability, an unmappable request, a
  window no grant covers, an over-long grant table, or a kernel refusal returns
  an error — never a fabricated pointer or a panic.
- **Platform-neutral** (§2.20): the non-coherent cache-maintenance shim is
  supplied by the (architecture-aware) driver process, never synthesised here.
- **One mapping per window**: each grant is mapped at most once; later requests
  into the same window reuse the cached base (§2.16).
- **Testable** (§7): the two syscalls live behind `GrantSyscalls`, so the
  resolution and translation logic is unit-tested on the host without a kernel.

## Stability

Tier: `experimental`.
