//! riscv64 `virt`-board MMIO transport for the shared `fw_cfg` DMA
//! client ([`rustos_itest_fwcfg`]).
//!
//! The `fw_cfg` DMA protocol itself lives in the shared crate; this
//! module supplies only the riscv64 half of the [`DmaAddressRegister`]
//! seam: the device's 64-bit big-endian DMA address register is MMIO at
//! `base + 16`, and the least-significant-half write at `base + 20`
//! triggers the operation. The base is discovered from the device tree.
//!
//! The boot pipeline runs with paging off (`satp == 0`) on the `virt`
//! board, so the MMIO base read from the device tree is identity-mapped
//! and directly addressable (the same assumption the virtio-MMIO
//! vertical relies on).

use core::ptr;

use rustos_itest_fwcfg::DmaAddressRegister;
use rustos_util::dtb::Dtb;

/// `compatible` string the `virt` board's `fw_cfg` node advertises.
const FW_CFG_COMPATIBLE: &str = "qemu,fw-cfg-mmio";

/// Offset of the 64-bit big-endian DMA address register from the
/// `fw_cfg` MMIO base (spec: Arm/riscv layout).
const DMA_REG_OFFSET: u64 = 16;

/// MMIO transport over the `virt` board's `fw_cfg` device.
pub struct MmioDma {
    base: u64,
}

/// Failure modes locating the device.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MmioDmaError {
    /// No `qemu,fw-cfg-mmio` node in the device tree.
    NotFound,
}

impl MmioDma {
    /// Locate the `fw_cfg` device in `dtb` and return a transport.
    ///
    /// # Errors
    ///
    /// [`MmioDmaError::NotFound`] if no compatible node is present.
    pub fn from_dtb(dtb: &Dtb<'_>) -> Result<Self, MmioDmaError> {
        for node in dtb.nodes() {
            let Ok(node) = node else {
                continue;
            };
            if !node.is_compatible(FW_CFG_COMPATIBLE) {
                continue;
            }
            let Some(reg) = node.property("reg") else {
                continue;
            };
            let Ok(base) = reg.read_be_u64(0) else {
                continue;
            };
            return Ok(Self { base });
        }
        Err(MmioDmaError::NotFound)
    }
}

impl DmaAddressRegister for MmioDma {
    fn write_dma_address(&self, dma_phys: u64) {
        let high = u32::try_from(dma_phys >> 32).unwrap_or(0);
        let low = (dma_phys & 0xFFFF_FFFF) as u32;
        // SAFETY: `base + DMA_REG_OFFSET` is the `fw_cfg` DMA address
        // register read from the device tree and identity-mapped on the
        // single-hart `virt` board; both halves are 4-byte aligned. The
        // big-endian register expects the most-significant half first
        // (at `+0`), then the least-significant half (at `+4`), whose
        // write triggers the operation; a little-endian `to_be()` store
        // lands the bytes the big-endian register expects.
        unsafe {
            ptr::write_volatile((self.base + DMA_REG_OFFSET) as *mut u32, high.to_be());
            ptr::write_volatile((self.base + DMA_REG_OFFSET + 4) as *mut u32, low.to_be());
        }
    }
}
