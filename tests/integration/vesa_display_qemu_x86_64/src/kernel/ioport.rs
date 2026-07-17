//! x86_64 I/O-port transport for the shared `fw_cfg` DMA client
//! ([`tairix_fwcfg`]).
//!
//! The `fw_cfg` DMA protocol itself lives in the shared crate; this
//! module supplies only the x86_64 half of the [`DmaAddressRegister`]
//! seam: the device's 64-bit big-endian DMA address register is the
//! I/O-port pair `0x514` (most-significant half) / `0x518`
//! (least-significant half), and the write to the low half triggers the
//! operation (QEMU `fw_cfg` spec, x86 register locations).
//!
//! The register is big-endian on the wire, so a 32-bit `out` of
//! `half.to_be()` lands the bytes the device reconstructs into `half` —
//! exactly the byte dance the riscv64 MMIO transport performs against its
//! big-endian register, only over I/O ports here.
//!
//! The DMA target buffers and the staged control structure are kernel
//! pointers in a -2 GiB higher-half kernel: a stack buffer is identity-
//! mapped low RAM (the x86_64 boot maps `0..4 GiB`), but a heap buffer is
//! a higher-half virtual address (`KERNEL_VMA_BASE + phys`). The
//! [`DmaAddressRegister::to_physical`] override below maps both back to
//! the physical address QEMU's DMA engine reads/writes.

use tairix_abi::PortIo;
use tairix_arch_x86_64::pio::x86_port_io;
use tairix_fwcfg::DmaAddressRegister;

/// I/O port holding the most-significant half of the `fw_cfg` DMA address
/// register.
const DMA_ADDR_HIGH_PORT: u16 = 0x514;
/// I/O port holding the least-significant half; the write here triggers
/// the operation.
const DMA_ADDR_LOW_PORT: u16 = 0x518;

/// I/O-port transport over the x86 `fw_cfg` DMA address register.
pub struct IoPortDma;

impl DmaAddressRegister for IoPortDma {
    fn to_physical(&self, virt: u64) -> u64 {
        // Higher-half kernel virtual-to-physical: pointers at or above
        // KERNEL_VMA_BASE (statics, heap) are linked at `base + phys`, so
        // subtract the base; low pointers (the boot stack) are identity-
        // mapped and pass through unchanged (linker.ld / boot.s
        // SAFETY-INVARIANT 9).
        let base = tairix_arch_x86_64::paging::KERNEL_VMA_BASE;
        if virt >= base {
            virt - base
        } else {
            virt
        }
    }

    fn write_dma_address(&self, dma_phys: u64) {
        let io = x86_port_io();
        let high = u32::try_from(dma_phys >> 32).unwrap_or(0);
        let low = (dma_phys & 0xFFFF_FFFF) as u32;
        // Most-significant half first (does not trigger), then the
        // least-significant half (triggers). `to_be()` lands the bytes
        // the big-endian register expects.
        io.write32(DMA_ADDR_HIGH_PORT, high.to_be());
        io.write32(DMA_ADDR_LOW_PORT, low.to_be());
    }
}
