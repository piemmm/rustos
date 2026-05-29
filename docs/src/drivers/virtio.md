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
  (`notify_wait`).
- A `BounceBuffer<'_>` wrapper that honours
  [`BufferClass::Sensitive`](../abi/driver_traits.md) by zeroing its
  staging on drop (`AGENTS.md` §4).
- A `MockTransport` + `ChainView` test seam used by the two device
  driver crates' host-side tests.

## Out of scope (intentionally deferred)

| Feature                                    | Why deferred                                                                       | Tracked in                                  |
|--------------------------------------------|------------------------------------------------------------------------------------|---------------------------------------------|
| Packed virtqueues (virtio 1.1 §2.7)        | Split queues meet the Stage 4 acceptance bar; packed is Stage 5 follow-up          | this page                                    |
| Per-process-heap DMA with `phys_of()`      | The kernel memory-capability surface does not exist yet                            | `.junie/next-session-prompt.md` item 1      |
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
                    | PciBackend / MmioBackend (shells today)
                    v
+-------------------------------------+
|  drivers/bus/pci  /  drivers/bus/mmio
+-------------------------------------+
```

## Capability model

- `register` requires `CAP_DRV_LOAD` (load-time).
- The transport crate is loaded as a user-space driver; it never
  asserts `CAP_DRV_KERNEL`.
- Per-method capabilities for block / net are documented on the
  `Block` / `Net` traits in [Driver traits](../abi/driver_traits.md).

## Test surface

`cargo test -p rustos-drv-bus-virtio` covers 30 host-side tests:
queue free-list initialisation, descriptor chaining (single + multi),
exhaustion, used-ring wrap-around, status progression, mock-peer
round-trip, sensitive-class scrub on drop. Coverage of the crate's
public surface is comfortably above the 75% Stage 4 bar
(`AGENTS.md` §7).
