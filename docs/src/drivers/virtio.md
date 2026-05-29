# Virtio transport

`drivers/bus/virtio` ships the cross-arch **split-virtqueue** plumbing
that `drivers/storage/virtio_blk` and `drivers/network/virtio_net` both
build on. Per `AGENTS.md` §2.2 the queue protocol lives once, here,
and the device drivers carry only the device-specific wire format.

## Scope

The transport crate covers:

- A `Transport` trait abstracting the PCI (`x86_64`) and MMIO
  (`aarch64`, `riscv64 virt`) bus seams behind a single interface,
  plus a concrete modern-PCI implementation, `PciTransport`
  (see [Modern PCI transport](#modern-pci-transport-pcitransport)).
- Virtio 1.1 §3.1 device-initialisation status sequencing
  (`reset` → `ACKNOWLEDGE` → `DRIVER` → `FEATURES_OK` → `DRIVER_OK`).
- The virtio 1.1 §2.6 **split virtqueue**: descriptor table, avail
  ring, used ring, free-descriptor pool, descriptor chaining.
- A `VirtioHost` trait through which a driver requests DMA-backed
  bounce buffers (`alloc_dma_zeroed`) and parks pending completion
  (`notify_wait`). `alloc_dma_zeroed` returns an **owned**
  [`DmaSlab`](#dma-ownership-model) so a driver can hold several
  simultaneously-live regions (e.g. the descriptor table + avail
  ring + used ring inside `SplitQueue`) without re-borrowing the
  host on every accessor.
- A `BounceBuffer` wrapper that honours
  [`BufferClass::Sensitive`](../abi/driver_traits.md) by zeroing its
  staging on drop (`AGENTS.md` §4).
- A `MockTransport` + `ChainView` test seam used by the two device
  driver crates' host-side tests.

## Out of scope (intentionally deferred)

| Feature                                    | Why deferred                                                                       | Tracked in                                  |
|--------------------------------------------|------------------------------------------------------------------------------------|---------------------------------------------|
| Packed virtqueues (virtio 1.1 §2.7)        | Split queues meet the Stage 4 acceptance bar; packed is Stage 5 follow-up          | this page                                    |
| Driver-host `DmaPool` wiring               | The driver host does not yet thread a per-process `DmaPool` through to its modules | `.junie/next-session-prompt.md` item 0      |
| IRQ routing into user-space drivers        | The kernel does not yet expose an IRQ capability                                   | `.junie/next-session-prompt.md` item 2      |
| MMIO `Transport` implementation            | The modern-PCI `PciTransport` lands first (Stage 4.D Item 4 prerequisite); the MMIO transport follows with the riscv64 QEMU work | `.junie/next-session-prompt.md` item 4 |
| Boot-time PCI walk → live driver host      | The kernel binary does not yet enumerate PCI and construct a live `drvhost::Host`  | `.junie/next-session-prompt.md` item 4      |
| QEMU integration tests (PCI + MMIO)        | Depend on the boot-time bring-up above plus the userland net stack from `.junie/next-session-prompt.md` item 5 | `.junie/next-session-prompt.md` item 4      |

## Layering picture

```
+-------------------------------------+
|  drivers/storage/virtio_blk         |  device-specific wire (virtio §5.2)
|  drivers/network/virtio_net         |  device-specific wire (virtio §5.1)
+-------------------+-----------------+
                    | Transport, SplitQueue, BounceBuffer, VirtioHost
                    v
+-------------------------------------+
|  drivers/bus/virtio                 |  virtio 1.1 §2.6 split queues, §3.1 init
+-------------------+-----------------+
                    | PciBackend / MmioBackend (own a RegisterWindow)
                    v
+-------------------------------------+
|  drivers/bus/pci  /  drivers/bus/mmio
+-------------------------------------+
```

## Modern PCI transport (`PciTransport`)

`PciTransport` (`drivers/bus/virtio/src/transport_pci.rs`) is the
concrete `Transport` for a modern (virtio-1.x) PCI device. It owns
the four capability-checked `RegisterWindow`s the bus driver resolves
from the device's virtio PCI capabilities (virtio 1.1 §4.1.4) —
*common configuration*, *notification*, *ISR status*, and
*device-specific configuration* — plus the notification
capability's `notify_off_multiplier`:

```rust
pub struct PciTransportWindows {
    pub common: RegisterWindow,
    pub notify: RegisterWindow,
    pub isr: RegisterWindow,
    pub device: RegisterWindow,
    pub notify_off_multiplier: u32,
}
```

Because a window can only be minted by the kernel MMIO-map facility
after a `CAP_MMIO_MAP` check, the transport holds **no** ambient
authority and performs **no** pointer arithmetic: every register
access goes through the bounds-checked `RegisterWindow` accessors
(`AGENTS.md` §4). The 64-bit queue-address registers (`queue_desc`,
`queue_driver`, `queue_device`) are written as two little-endian
`u32` halves because the window exposes no `u64` accessor.

`PciTransport::new` validates that the common-configuration window
is at least `virtio_pci_common_cfg` length (`0x38` bytes) and reads
`num_queues` up front. Every common-cfg offset the infallible
`Transport` methods touch is a compile-time constant below that
bound, so those methods treat their accesses as in-bounds and fall
back to a safe default on the (then impossible) error rather than
panicking (`AGENTS.md` §2.9). The device-supplied notify offset is
bounds-checked against the notification window on the fallible
`queue_set` path, so the infallible `notify` only ever writes within
a pre-validated offset and fails closed (skips the write) for an
unprogrammed queue.

## DMA ownership model

`alloc_dma_zeroed` returns a `DmaSlab` — an owned handle of the
shape

```
struct DmaSlab {
    phys: u64,
    ptr: NonNull<u8>,
    len: usize,
    pool_id: PoolId,
    slot: usize,
    /* type-erased free shim */
}
```

The slab carries the disjoint-slot invariant in its `pool_id` /
`slot` fields: every slab minted from the same pool carries a
distinct `slot` index, the pool's slot bitmap guarantees the byte
range `[ptr, ptr + len)` does not overlap any other live slab, and
`DmaSlab::as_bytes_mut` cites that bitmap as its `// SAFETY:`
witness. The owning shape means the consumer driver code can hold
three live slabs in `SplitQueue` (descriptor table + avail ring +
used ring) and three more in a transaction (header + payload +
status) without ever re-borrowing the pool.

The in-process `MockHost` mints slabs with `PoolId::MOCK`, a
monotonic `slot` counter, and a no-op free shim (the leak contract
is unchanged).

### Kernel host (`KernelVirtioHost`)

Stage 4.D Item 0 ships the real, capability-checked
`VirtioHost` implementation. It lives in
`drivers/bus/virtio/src/kernel_host.rs`, behind the crate's
`kernel-host` Cargo feature (off by default — `AGENTS.md` §2.3),
and is generic over the page-table backend `P` and the audit
`Sink` `S`:

```rust
pub struct KernelVirtioHost<'a, P: PageTableOps, S: Sink + ?Sized> {
    /* RefCell<DmaPool<'a, P>>, &'a TaskCapabilities, &'a S,
       fresh PoolId, monotonic slot counter, live-slot table,
       &'a IrqTable, IrqHandle, &'a dyn IrqWaiter */
}
```

The host **owns** its `DmaPool` (the `'a` lifetime now bounds only
the pool's `FrameAllocator` borrow, not the pool itself). Ownership
is what lets the kernel binary's `KernelVirtioFactory` mint a fresh
per-driver host from behind a shared `&self` borrow — a
borrowed-`&mut` pool could not be handed out that way (see
[Kernel-binary factory](#kernel-binary-factory-kernelvirtiofactory)).

`alloc_dma_zeroed` routes every request through
`kernel/sec::dma::alloc_dma`, which performs the
`CapabilityId::MEM_DMA` check and emits the
`AuditEvent::DmaAllocated` (or `…Denied`) record. The host then
calls `DmaPool::slot_base(&buf)` and mints a `DmaSlab` via
`DmaSlab::from_pool`, stamping its own fresh `PoolId` and a
monotonic slot index. The slab carries a free shim
(`unsafe fn(*const(), usize, usize)`) that re-enters the host on
drop, looks the buffer up by slot, and routes it back through
`kernel/sec::dma::free_dma`. The shim is monomorphised per
`(P, S)` so the `*const ()` cast back to
`*const KernelVirtioHost<'_, P, S>` is the inverse of the one
performed at construction (`AGENTS.md` §2.10 — every `unsafe`
block carries its `// SAFETY:` justification).

`notify_wait` blocks the loaded driver task on the device's
pre-bound interrupt line through `kernel/irq::block_until_ready`
(Stage 4.D Item 2-tail.3); the host borrows the kernel `IrqTable`,
the bus-driver-minted `IrqHandle`, and the scheduler/clock
`IrqWaiter` seam for this.

### Kernel-binary factory (`KernelVirtioFactory`)

Stage 4.D Item 2-tail.4 wires the host into the userland driver
host. The userland `drvhost` defines a `VirtioHostFactory` trait
(`mint(&self, granted) -> Option<Box<dyn VirtioHost>>`) and calls
it just before a driver's `register()`; the kernel binary supplies
the concrete implementation, `KernelVirtioFactory`, in
`kernel/rustos-kernel/src/virtio_factory.rs`. Keeping the impl in
the kernel binary lets `drvhost` stay free of every `kernel/*`
dependency (`AGENTS.md` §3): only the kernel binary depends on both
`drvhost` and the `kernel-host` build of this crate.

Each `mint` call builds a brand-new `AddressSpace` (via a
`make_table` closure) and `DmaPool`, then hands ownership to a fresh
`KernelVirtioHost`, so every loaded driver gets its own per-process
heap (`AGENTS.md` §4). A driver whose granted capability set lacks
`CAP_MEM_DMA` is refused a host outright (`mint` returns `None`),
failing closed before any pool is allocated.

Capability refusals surface as `DriverError::PermissionDenied`;
allocator failures (oversize requests, OOM, internal pool config
errors) collapse to `DriverError::LengthOutOfRange` — the same
shape the `MockHost` uses when its 64 MiB cap is hit, so a driver
consumer sees a single failure surface regardless of which host
minted it.

## Capability model

- `register` requires `CAP_DRV_LOAD` (load-time).
- The transport crate is loaded as a user-space driver; it never
  asserts `CAP_DRV_KERNEL`.
- Per-method capabilities for block / net are documented on the
  `Block` / `Net` traits in [Driver traits](../abi/driver_traits.md).

## Test surface

`cargo test -p rustos-drv-bus-virtio --lib` covers the host-side
tests, including the eleven `transport_pci` tests added as the
Stage 4.D Item 4 prerequisite (short-window rejection, `num_queues`
read, status write/read + reset, driver-feature halves, queue-select
range check, queue programming + notify-offset recording, oversize
and out-of-bounds-notify rejection, no-op notify for an unprogrammed
queue, device-config read with zero-fill overflow, and a
`SplitQueue`-drives-`PciTransport` integration check): queue
free-list initialisation, descriptor chaining (single
+ multi), exhaustion, used-ring wrap-around, status progression,
mock-peer round-trip, sensitive-class scrub on drop, the four
`DmaSlab` tests added in Stage 4.D Item 0a (round-trip; three
simultaneous disjoint writes; `drop` invokes the free shim once
with the right `(slot, len)`; `pool_id` distinguishes slabs across
pools), and the seven `KernelVirtioHost` tests added in Item 0
(zero-initialisation + audit emit, drop routes through
`free_dma`, `CapabilityId::MEM_DMA` refusal returns
`PermissionDenied`, zero-size short-circuit, two simultaneous
disjoint slabs, `notify_wait` records queue index, oversize
collapses to `LengthOutOfRange`). Coverage of the crate's public
surface is comfortably above the 75% Stage 4 bar
(`AGENTS.md` §7).
