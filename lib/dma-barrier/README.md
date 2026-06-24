# rustos-dma-barrier

DMA memory-ordering barriers for user-space drivers.

A user-space driver shares memory with a device that is a separate, possibly
non-I/O-coherent, bus master (the Raspberry Pi 4's PCIe root complex is the
standing example). That shared memory is mapped Normal **Non-Cacheable**
(`PageFlags::DMA_COHERENT`), which removes the cache-coherency problem but does
**not** order the CPU's accesses with respect to the device. This crate is the
single home (`AGENTS.md` §2.2) of the architecture-specific barrier the silicon
requires for that ordering — the user-space analogue of `rustos-abi-trap`'s
syscall-trap carve-out and part of the §1 assembly carve-out.

- `dma_wmb()` — write barrier: after writing device-visible data, before the
  MMIO doorbell that hands it to the device.
- `dma_rmb()` — read barrier: after reading a device-written ownership/cycle
  flag and finding the entry owned, before reading the rest of the entry.

`core::sync::atomic::fence` is **not** a substitute: on AArch64 it lowers to an
inner-shareable `dmb ish`, which does not order accesses against a non-coherent
outer/system-domain DMA master.

| Target  | `dma_wmb`        | `dma_rmb`        |
|---------|------------------|------------------|
| aarch64 | `dmb oshst`      | `dmb oshld`      |
| x86_64  | `sfence`         | `lfence`         |
| riscv64 | `fence iorw,iorw`| `fence iorw,iorw`|
| host/wasm32 | no-op        | no-op            |

The per-arch instruction is selected by `build.rs` (`dma_barrier_<arch>` cfgs),
keeping the target choice out of the source tree the §17.2 `cfg-check` guards.

## Stability

`experimental`.
