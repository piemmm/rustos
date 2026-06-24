# `rustos-dma-barrier`

DMA memory-ordering barriers for user-space drivers.

A user-space driver shares a block of memory with a device that is a separate
bus master (an xHCI controller, a virtio device). On a platform whose device
DMA is **not** I/O-coherent — the Raspberry Pi 4's PCIe root complex is the
standing example — that block is mapped Normal **Non-Cacheable**
(`PageFlags::DMA_COHERENT`, see [the aarch64 port](../platform/aarch64.md)).
Non-cacheable removes the *cache*-coherency problem, but it does **not** order
the CPU's accesses with respect to the device. Two hazards follow, and this
crate supplies the one barrier each needs:

- **Doorbell before data is visible.** After the driver writes descriptors /
  TRBs / buffers it rings an MMIO doorbell so the device reads them. Without a
  barrier the device — a separate, non-coherent master — can observe the
  doorbell store before the data stores and act on stale memory. `dma_wmb()`,
  issued **before** the doorbell, orders the data writes ahead of it.
- **Torn read of a device-written ring entry.** The device writes a ring
  entry's body then sets its ownership/cycle bit last. Reading the whole entry
  unordered can pair a freshly-set cycle bit with the *previous* entry's stale
  body. `dma_rmb()`, issued **after** observing the cycle bit and **before**
  reading the body, orders the two.

`core::sync::atomic::fence` is **not** a substitute: on AArch64 it lowers to an
*inner*-shareable `dmb ish`, which does not order accesses against a
non-coherent outer/system-domain DMA master.

This is the single home of the barrier instruction (`AGENTS.md` §2.2) — the
user-space analogue of [`rustos-abi-trap`](./overview.md)'s syscall-trap
carve-out and part of the §1 assembly carve-out. The per-architecture
instruction is selected by `build.rs` (`dma_barrier_<arch>` cfgs), keeping the
target choice out of the source tree the §17.2 `cfg-check` guards.

| Target  | `dma_wmb`         | `dma_rmb`         |
|---------|-------------------|-------------------|
| aarch64 | `dmb oshst`       | `dmb oshld`       |
| x86_64  | `sfence`          | `lfence`          |
| riscv64 | `fence iorw,iorw` | `fence iorw,iorw` |
| host / wasm32 | no-op       | no-op             |

## Consumers

`rustos-usb` wires `dma_wmb()` into the controller-start and doorbell handoffs
and `dma_rmb()` into the event-ring read (cycle bit first, barrier, then the
entry body). The same ordering applies to any user-space driver sharing
Non-Cacheable memory with a device; `lib/virtio`'s queue-notify / used-ring
path is the next consumer to adopt it.

## Stability

`experimental`.
