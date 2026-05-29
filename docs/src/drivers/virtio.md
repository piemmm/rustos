# Virtio transport

`drivers/bus/virtio` ships the cross-arch **split-virtqueue** plumbing
that `drivers/storage/virtio_blk` and `drivers/network/virtio_net` both
build on. Per `AGENTS.md` §2.2 the queue protocol lives once, here,
and the device drivers carry only the device-specific wire format.

## Scope

The transport crate covers:

- A `Transport` trait abstracting the PCI (`x86_64`) and MMIO
  (`aarch64`, `riscv64 virt`) bus seams behind a single interface.
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
| Bus-handle hand-off from PCI/MMIO drivers  | The `drivers/bus/pci` and `drivers/bus/mmio` shells stop at enumeration            | `.junie/next-session-prompt.md` item 3      |
| QEMU integration tests (PCI + MMIO)        | Depend on items 1–3 plus the userland net stack from `.junie/next-session-prompt.md` item 5 | `.junie/next-session-prompt.md` item 4      |

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

`cargo test -p rustos-drv-bus-virtio --lib` covers 41 host-side
tests: queue free-list initialisation, descriptor chaining (single
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
