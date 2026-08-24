# Virtio transport

The bus-agnostic virtqueue protocol lives in `lib/virtio`
(crate `tairix-virtio`), together with the concrete virtio-MMIO
`Transport` (`MmioTransport`); `drivers/bus/virtio` adds only the
concrete PCI `Transport` implementation (and the register-window
backends) on top of it. The MMIO transport sits in `lib/virtio` so an
arch-neutral user-space virtio driver process can build it without a
`drivers/* → drivers/*` edge (`AGENTS.md` §17.4 / §2.2 — the `lib/usb`
↔ `drivers/bus/usb` precedent). Both wire
formats are implemented as parallel siblings (`AGENTS.md` §2.2): the
**split virtqueue** (virtio 1.1 §2.6, `SplitQueue`) and the **packed
virtqueue** (virtio 1.1 §2.7, `PackedQueue`). The device
drivers `drivers/storage/virtio_blk` and `drivers/network/virtio_net`
depend on `lib/virtio` and never on the bus driver crate — a driver
may depend on `lib/*` but not on another driver (`AGENTS.md` §17.4).
Per `AGENTS.md` §2.2 the queue protocol lives once, in `lib/virtio`,
and the device drivers carry only the device-specific wire format.

## Scope

`lib/virtio` (the protocol) covers:

- A `Transport` trait abstracting the PCI (`x86_64`) and MMIO
  (`aarch64`, `riscv64 virt`) bus seams behind a single interface.
  Its two concrete implementations are the modern-PCI `PciTransport`
  in `drivers/bus/virtio`
  (see [Modern PCI transport](#modern-pci-transport-pcitransport))
  and the virtio-MMIO `MmioTransport` in `lib/virtio`
  (see [Modern MMIO transport](#modern-mmio-transport-mmiotransport)).
- Virtio 1.1 §3.1 device-initialisation status sequencing
  (`reset` → `ACKNOWLEDGE` → `DRIVER` → `FEATURES_OK` → `DRIVER_OK`).
- The virtio 1.1 §2.6 **split virtqueue** (`SplitQueue`): descriptor
  table, avail ring, used ring, free-descriptor pool, descriptor
  chaining.
- The virtio 1.1 §2.7 **packed virtqueue** (`PackedQueue`): a single
  descriptor ring plus the driver- and device-event-suppression
  structures, with availability and completion signalled in-band
  through each descriptor's `AVAIL`/`USED` flag bits against the
  per-side wrap counters (see [Packed virtqueue](#packed-virtqueue)).
  Both queues share the `ChainSegment` / `UsedToken` vocabulary and
  the same `Transport` seam.
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
| Driver-host `DmaPool` wiring               | The driver host does not yet thread a per-process `DmaPool` through to its modules | `plans/WIRING.md` item 0      |
| IRQ routing into user-space drivers        | The kernel does not yet expose an IRQ capability                                   | `plans/WIRING.md` item 2      |
| Boot-time PCI/MMIO walk → live driver host | The kernel binary does not yet enumerate the bus and construct a live `drvhost::Host` | `plans/WIRING.md` item 4   |
| QEMU integration tests (PCI + MMIO)        | Depend on the boot-time bring-up above plus the userland net stack from `plans/WIRING.md` item 5 | `plans/WIRING.md` item 4      |

## Layering picture

```
+-------------------------------------+
|  drivers/storage/virtio_blk         |  device-specific wire (virtio §5.2)
|  drivers/network/virtio_net         |  device-specific wire (virtio §5.1)
+-------------------+-----------------+
                    | Transport, SplitQueue, BounceBuffer, VirtioHost
                    v
+-------------------------------------+
|  lib/virtio                         |  virtio 1.1 §2.6 split + §2.7 packed
|                                     |  queues, §3.1 init, MmioTransport
+-------------------+-----------------+
                    ^ PciTransport implements Transport
                    |
+-------------------+-----------------+
|  drivers/bus/virtio                 |  concrete PCI Transport impl
+-------------------+-----------------+
                    | PciBackend / MmioBackend (own a RegisterWindow)
                    v
+-------------------------------------+
|  drivers/bus/pci  /  drivers/bus/mmio
+-------------------------------------+
```

The kernel-side `VirtioHost` (`KernelVirtioHost`) and its per-driver
factory live one layer up, in `kernel/virtio`, because they link
`kernel/{mem,sec,irq}`; a driver crate may not (`AGENTS.md` §17.4).
They consume the same `lib/virtio` protocol the drivers do.

## Modern PCI transport (`PciTransport`)

`PciTransport` (`drivers/bus/virtio/src/transport_pci.rs`) is the
concrete `Transport` for a modern (virtio-1.x) PCI device. It owns
the four capability-checked `RegisterWindow`s the bus driver resolves
from the device's virtio PCI capabilities (virtio 1.1 §4.1.4) —
*common configuration*, *notification*, *ISR status*, and
*device-specific configuration* — plus the notification
capability's `notify_off_multiplier`. These are bundled in
`PciTransportWindows`, the transport-construction seam, which lives in
`lib/virtio` (not the bus driver) so the ring-0 provisioning walk in
`kernel/virtio` can assemble it and hand it to `PciTransport::new`
without naming the `drivers/bus/virtio` crate (`AGENTS.md` §17.4):

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
`u32` halves, low half first, because virtio defines its 64-bit
registers as two 32-bit accesses (virtio 1.1 §4.1.3.1, §4.2.2) — which
is why the window carries no `u64` accessor. Both transports share the
one `write_u64_halves` in `lib/virtio`'s `transport` module rather than
each carrying its own copy of the split.

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

## Modern MMIO transport (`MmioTransport`)

`MmioTransport` (`lib/virtio/src/transport_mmio.rs`) is the
concrete `Transport` for a modern (virtio-1.x) MMIO device — the
layout QEMU's `-M virt` `virtio-mmio` transport and the RISC-V /
`AArch64` device-tree nodes advertise (virtio 1.1 §4.2). It lives in
`lib/virtio` (not the bus driver) because it depends only on the
bounds-checked `RegisterWindow` and the protocol types, so both the
kernel-side consumers and an arch-neutral user-space virtio driver
process can construct it without a `drivers/* → drivers/*` edge
(`AGENTS.md` §17.4 / §2.2 — the `lib/usb` ↔ `drivers/bus/usb`
precedent). Unlike the four capability-selected PCI windows, a
virtio-MMIO device exposes a **single** contiguous register block, so
the transport owns one `RegisterWindow`. A consumer resolves the
block's `(base, length)` from the boot device tree and maps it through
the same `CAP_MMIO_MAP`-gated MMIO-map facility; the transport
therefore holds **no** ambient authority and performs **no** pointer
arithmetic (`AGENTS.md` §4).

`MmioTransport::new` validates the `MagicValue` (`"virt"`), a modern
`Version` of `2`, a non-zero `DeviceID`, and a window that spans the
whole register block (`regs::WINDOW_MIN_LEN`), so every register the
infallible `Transport` methods touch is a compile-time constant
below that bound and never panics (`AGENTS.md` §2.9). Two MMIO-only
differences from the PCI transport:

- There is no "number of queues" register; a queue's existence is
  advertised through a non-zero `QueueNumMax`, so `num_queues`
  reports the architectural 16-bit maximum and the driver probes
  per-queue via `queue_select` + `queue_max_size`.
- Notification is a single write of the queue index to the
  `QueueNotify` register — there is no per-queue notify offset or
  multiplier — so `notify` is a constant-offset write that always
  stays in bounds.

The 64-bit queue-address registers (`QueueDesc`, `QueueDriver`,
`QueueDevice`) are written as `Low`/`High` `u32` pairs, and
`QueueReady` is set to `1` to bring a programmed queue online.

## Virtqueue memory ordering

`SplitQueue` issues the virtio 1.1 §2.7.13.3 ordering barriers around the
shared driver/device ring memory, so the device always observes a
consistent ring snapshot:

- **Publish** (`add_chain`): a `fence(Release)` separates the
  descriptor-table and avail-ring *entry* stores from the avail-`idx`
  store that exposes them, so a device that sees the new index cannot read
  a not-yet-written descriptor.
- **Notify** (`kick`): a `fence(SeqCst)` precedes the `QueueNotify` write,
  so the published avail-`idx` is globally visible before the device is
  notified.
- **Consume** (`poll_used`): a `fence(Acquire)` follows the used-`idx`
  read, so the used-ring *entry* read cannot be reordered ahead of the
  index that announced it.

These barriers are mandatory, not advisory. A **synchronous** backend
(virtio-blk, which QEMU drains on the same `notify` the guest issues, in
the issuing context) happens to tolerate their omission; an
**asynchronous** device does not. The motivating case is virtio-input: it
pops an eventq buffer when an input event arrives out of band, reading the
ring from a different context, so without the publish/notify barriers it
observes an empty avail ring and reports queue-full, and without the
consume barrier the driver reads a stale used-`idx` and never drains. The
barriers are also required on real hardware with weakly-ordered memory and
non-synchronous DMA. The `tests/integration/autoload_input_qemu_aarch64`
vertical is the regression guard (it never delivers a key without them).

## Packed virtqueue

`PackedQueue` (`lib/virtio/src/packed.rs`) implements the packed-ring
format (virtio 1.1 §2.7) as a parallel sibling of `SplitQueue`, not a
replacement: a device advertises it through the
`VIRTIO_F_RING_PACKED` feature bit. Where the split format spreads
state across three structures, the packed format uses **one**
descriptor ring (`PackedQueue::desc_ring_size` bytes — 16 per entry)
plus two 4-byte event-suppression structures, programmed through the
*same* `Transport::queue_set(size, desc, driver_area, device_area)`
seam the split queue uses (the three address registers map to
`queue_desc` / `queue_driver` / `queue_device` either way, so no
transport-interface change was needed).

Availability and completion are signalled **in-band** in each
descriptor's `flags`:

- `VRING_PACKED_DESC_F_AVAIL (1 << 7)` and
  `VRING_PACKED_DESC_F_USED (1 << 15)` are interpreted relative to a
  single-bit wrap counter held independently by the driver
  (`avail_wrap`) and tracked for the device (`used_wrap`), both
  initialised to `1`.
- The driver marks a descriptor available by setting `AVAIL` to its
  wrap counter and `USED` to the inverse (`AVAIL != USED`). The
  device marks it used by setting both to its own wrap counter
  (`AVAIL == USED`). A wrap counter toggles each time its cursor
  steps off the last ring slot.

`add_chain` writes the chain across consecutive ring entries, sets
`VRING_PACKED_DESC_F_NEXT` on every entry but the last, stores the
buffer id in the last entry, and publishes the head descriptor's
flags last so the device never observes a partial scatter/gather
list (virtio 1.1 §2.7.6). It returns the buffer id (the chain's head
ring position); `poll_used` reads the in-band `USED` marker at its
cursor, reclaims the chain's slots, and returns a `UsedToken` —
the same `ChainSegment` / `UsedToken` vocabulary the split queue
uses. The in-process `MockTransport::drain_packed_queue` is the
packed peer the unit tests drive, mirroring `drain_queue` for the
split ring.

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
`kernel/virtio/src/kernel_host.rs` — the kernel crate, because it
links `kernel/{mem,sec,irq}`, which a driver crate may not
(`AGENTS.md` §17.4) — and is generic over the page-table backend `P`
and the audit `Sink` `S`:

```rust
pub struct KernelVirtioHost<'a, P: PageTable, S: Sink + ?Sized> {
    /* RefCell<DmaPool<'a, P>>, &'a TaskCapabilities, &'a S,
       fresh PoolId, monotonic slot counter, live-slot table,
       &'a IrqTable, IrqHandle, &'a dyn IrqWaiter */
}
```

The host **owns** its `DmaPool` (the `'a` lifetime now bounds only
the pool's `FrameAllocator` borrow, not the pool itself). Ownership
is what lets `kernel/virtio`'s `KernelVirtioFactory` mint a fresh
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

The wait is bounded by the **caller's** `timeout_ns` and answers
`CompletionSignal::Fired` or `CompletionSignal::TimedOut`. A driver with a
request outstanding passes its device class's per-request deadline
(`BlkDeviceClass::budget().deadline_ns`); a driver waiting for an unsolicited
event with nothing pending (an idle input device) passes `u64::MAX`. A request
wait must never be unbounded: the waiting task holds the device's lock for the
duration of its request, so one lost or coalesced completion interrupt would
park it forever and stall every other user of that disk behind it — silently,
with no error to explain it. `virtio_blk` therefore fails a silent request
closed with `DriverError::DeviceOffline` after one final used-ring re-scan
(a completion whose interrupt was lost is already in the ring), and does not
reissue in place: the device may still own the published descriptor chain, so
re-publishing the same staging could have an abandoned request complete into
the next one's buffers. Reissue policy belongs to the consumer above, which
knows whether the request is safe to repeat.

### Kernel-binary factory (`KernelVirtioFactory`)

Stage 4.D Item 2-tail.4 wires the host into the userland driver
host. The `VirtioHostFactory` trait
(`mint(&self, granted: &dyn CapabilityQuery) -> Option<Box<dyn VirtioHost>>`)
lives in the bus-agnostic `lib/virtio` host seam; the userland
`drvhost` calls it just before a driver's `register()`, and
`kernel/virtio` supplies the concrete implementation,
`KernelVirtioFactory`, in `kernel/virtio/src/virtio_factory.rs`.
Hosting the trait in `lib/virtio` lets both sides depend on `lib/*`
instead of on each other (`AGENTS.md` §17.4): `drvhost` stays free
of every `kernel/*` dependency and `kernel/virtio` stays free of
every `userland/*` dependency. `kernel/virtio` links
`kernel/{mem,sec,irq}` for the concrete host, while the bus driver
and device drivers stay on `lib/*` only. `mint` gates on the
driver's granted capabilities through `&dyn tairix_abi::CapabilityQuery`,
so the seam never names `lib/caps`.

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

## Untrusted device input

The used ring and the descriptor table are **device-written**: under
the `AGENTS.md` §4 / §3.6 threat model a buggy or hostile device (a
DMA-capable, Thunderclap-class peer, CWE-1257) may write a completion
naming a descriptor head outside the granted table, or DMA-scribble a
chain `next` link so the reclaim walk would leave the region.
`SplitQueue::poll_used` therefore validates every device-supplied head
against `queue_size` and bounds the reclaim walk to the table: an
out-of-range head is rejected with `VirtioError::MalformedCompletion`
(mapped to `DriverError::DeviceFault`) and no chain is reclaimed, while
a corrupted `next` link makes the walk bail at the boundary. The driver
never dereferences a descriptor outside the granted region — it fails
closed (§5.4) rather than trusting the device.

## Test surface

The protocol and the kernel host are tested in the crates that own
them (`AGENTS.md` §7 — unit tests next to the code):

- `cargo test -p tairix-virtio` covers the bus-agnostic protocol:
  split-queue free-list initialisation, descriptor chaining (single
  + multi), exhaustion, used-ring wrap-around, status progression,
  mock-peer round-trip, sensitive-class scrub on drop, and the
  `DmaSlab` ownership tests (round-trip; three simultaneous disjoint
  writes; `drop` invokes the free shim once with the right
  `(slot, len)`; `pool_id` distinguishes slabs across pools). The
  packed ring (virtio 1.1 §2.7) is covered alongside the split ring:
  the `AVAIL`/`USED` flag truth table and descriptor byte round-trip,
  plus end-to-end queue initialisation, non-power-of-two rejection,
  slot consumption, mock-peer round-trip, empty/too-long and
  free-pool-exhaustion rejection, ring-wrap-with-reclaim across the
  ring boundary (toggling both wrap counters), and the empty
  no-completion / no-op-drain paths. The §3.6 adversarial tests
  (`poll_used_rejects_a_device_head_outside_the_descriptor_table`,
  `poll_used_reclaim_bails_on_a_corrupted_next_link`) and the
  `fuzz_virtqueue` harness drive a hostile device-written used ring /
  descriptor table and assert the consumer fails closed. It also
  covers the concrete virtio-MMIO transport (`transport_mmio`), which
  lives here for the riscv64 / `AArch64` MMIO bus seam: short-window,
  bad-magic, legacy-version and empty-slot rejection, status
  write/read + reset, device/driver-feature halves, queue-select
  register write, queue programming + `QueueReady`, oversize
  rejection, single-register notify, device-config read with zero-fill
  overflow, and a `SplitQueue`-drives-`MmioTransport` integration
  check.
- `cargo test -p tairix-drv-bus-virtio` covers the concrete PCI
  transport: the `transport_pci` tests (short-window rejection,
  `num_queues` read, status write/read + reset, driver-feature
  halves, queue-select range check, queue programming + notify-offset
  recording, oversize and out-of-bounds-notify rejection, no-op
  notify for an unprogrammed queue, device-config read with zero-fill
  overflow, and a `SplitQueue`-drives-`PciTransport` integration
  check).
- `cargo test -p tairix-kernel-virtio` covers the kernel host and
  MMIO mapper: zero-initialisation + audit emit, drop routes through
  `free_dma`, `CapabilityId::MEM_DMA` refusal returns
  `PermissionDenied`, zero-size short-circuit, two simultaneous
  disjoint slabs, the `notify_wait` IRQ-park paths, and oversize
  collapsing to `LengthOutOfRange`.

Coverage of each crate's public surface is comfortably above the 75%
Stage 4 bar (`AGENTS.md` §7).
