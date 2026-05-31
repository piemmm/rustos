//! Minimal `fw_cfg` MMIO DMA client for the `virt` board, used to
//! program QEMU's `ramfb` device.
//!
//! Only the operations this vertical needs are implemented: a DMA read
//! (used to fetch the `FW_CFG_FILE_DIR` and validate the signature) and
//! a DMA write (used to push the `RAMFBCfg` into the `etc/ramfb` item).
//! The interface is the one documented in the QEMU `fw_cfg` spec: a
//! 64-bit big-endian DMA address register at `base + 16` whose
//! least-significant-half write triggers an operation described by an
//! in-RAM [`DmaAccess`] structure (all fields big-endian).
//!
//! The boot pipeline runs with paging off (`satp == 0`) on the `virt`
//! board, so a `static` buffer's address is its physical address and is
//! reachable both by the CPU and by QEMU's DMA engine without any
//! mapping step (the same identity-map assumption the virtio-MMIO
//! vertical relies on).

use core::ptr;
use core::sync::atomic::{compiler_fence, Ordering};

use rustos_util::dtb::Dtb;

/// `compatible` string the `virt` board's `fw_cfg` node advertises.
const FW_CFG_COMPATIBLE: &str = "qemu,fw-cfg-mmio";

/// Offset of the 64-bit big-endian DMA address register from the
/// `fw_cfg` MMIO base (spec: Arm/riscv layout).
const DMA_REG_OFFSET: u64 = 16;

/// `control` bit: the device sets this when an operation failed.
const CTL_ERROR: u32 = 1 << 0;
/// `control` bit: perform a read (device → guest RAM).
const CTL_READ: u32 = 1 << 1;
/// `control` bit: select the item named in the upper 16 bits.
const CTL_SELECT: u32 = 1 << 3;
/// `control` bit: perform a write (guest RAM → device).
const CTL_WRITE: u32 = 1 << 4;

/// `FW_CFG_SIGNATURE` selector key; reading it yields the bytes `QEMU`.
pub const KEY_SIGNATURE: u16 = 0x0000;
/// `FW_CFG_FILE_DIR` selector key.
pub const KEY_FILE_DIR: u16 = 0x0019;

/// In-RAM DMA control structure (QEMU `FWCfgDmaAccess`). All fields are
/// big-endian on the wire; this mirror is written through volatile
/// stores in [`FwCfg::op`]. One lives on the stack per operation — the
/// boot stack is ordinary RAM, so its address is a valid DMA physical
/// address under the boot identity map.
#[repr(C, align(8))]
struct DmaAccess {
    control: u32,
    length: u32,
    address: u64,
}

/// Handle to the `virt` board's `fw_cfg` device.
pub struct FwCfg {
    base: u64,
}

/// Failure modes of the `fw_cfg` client.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FwCfgError {
    /// No `qemu,fw-cfg-mmio` node in the device tree.
    NotFound,
    /// The device reported the error bit (or a non-zero residual
    /// control) after an operation.
    Transfer,
    /// A buffer length or address did not fit the device's `u32`/`u64`
    /// registers.
    OutOfRange,
}

impl FwCfg {
    /// Locate the `fw_cfg` device in `dtb` and return a handle.
    ///
    /// # Errors
    ///
    /// [`FwCfgError::NotFound`] if no compatible node is present.
    pub fn from_dtb(dtb: &Dtb<'_>) -> Result<Self, FwCfgError> {
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
        Err(FwCfgError::NotFound)
    }

    /// Stage a [`DmaAccess`] on the stack (fields in the device's
    /// big-endian wire form), trigger the operation, and spin until the
    /// device clears `control` (synchronous in QEMU).
    ///
    /// # Errors
    ///
    /// [`FwCfgError::Transfer`] if the device sets the error bit or
    /// leaves a non-zero residual `control`.
    fn op(&self, control: u32, length: u32, address: u64) -> Result<(), FwCfgError> {
        let mut access = DmaAccess {
            control: 0,
            length: 0,
            address: 0,
        };
        // Stage the request in the device's big-endian wire form.
        // Volatile stores keep the writes from being reordered past the
        // trigger.
        // SAFETY: `&mut access` is a unique, 8-byte-aligned stack local
        // that lives across the synchronous DMA; the stores touch only
        // its own fields.
        unsafe {
            ptr::write_volatile(ptr::addr_of_mut!(access.control), control.to_be());
            ptr::write_volatile(ptr::addr_of_mut!(access.length), length.to_be());
            ptr::write_volatile(ptr::addr_of_mut!(access.address), address.to_be());
        }

        // The DMA address register is big-endian; writing the
        // least-significant half triggers. Every buffer this client
        // uses lives in low RAM (< 4 GiB), so the high half is zero —
        // but write it explicitly for robustness, then the low half to
        // trigger. A little-endian `to_be()` store lands the bytes the
        // big-endian register expects.
        let dma_phys = ptr::addr_of!(access) as u64;
        let high = u32::try_from(dma_phys >> 32).unwrap_or(0);
        let low = (dma_phys & 0xFFFF_FFFF) as u32;
        // Ensure the staged structure is visible before the trigger.
        compiler_fence(Ordering::SeqCst);
        // SAFETY: `base + DMA_REG_OFFSET` is the `fw_cfg` DMA address
        // register read from the device tree and identity-mapped on the
        // single-hart `virt` board; both halves are 4-byte aligned.
        unsafe {
            ptr::write_volatile((self.base + DMA_REG_OFFSET) as *mut u32, high.to_be());
            ptr::write_volatile((self.base + DMA_REG_OFFSET + 4) as *mut u32, low.to_be());
        }
        compiler_fence(Ordering::SeqCst);
        // SAFETY: QEMU completes the operation synchronously and clears
        // `control`; this volatile read observes the result.
        let result = unsafe { ptr::read_volatile(ptr::addr_of!(access.control)) };
        // `control` was staged big-endian; convert back to test the bits.
        let result = u32::from_be(result);
        if result & CTL_ERROR != 0 || result != 0 {
            return Err(FwCfgError::Transfer);
        }
        Ok(())
    }

    /// Select `key` and read `buf.len()` bytes of its data into `buf`.
    ///
    /// # Errors
    ///
    /// [`FwCfgError::OutOfRange`] if `buf` is larger than a `u32`;
    /// [`FwCfgError::Transfer`] on a device-reported failure.
    pub fn read_item(&self, key: u16, buf: &mut [u8]) -> Result<(), FwCfgError> {
        let len = u32::try_from(buf.len()).map_err(|_| FwCfgError::OutOfRange)?;
        let addr = buf.as_mut_ptr() as u64;
        let control = (u32::from(key) << 16) | CTL_SELECT | CTL_READ;
        self.op(control, len, addr)
    }

    /// Continue reading `buf.len()` bytes of the currently-selected item
    /// (no re-select; the device's offset persists from a prior call).
    ///
    /// # Errors
    ///
    /// As [`Self::read_item`].
    pub fn read_more(&self, buf: &mut [u8]) -> Result<(), FwCfgError> {
        let len = u32::try_from(buf.len()).map_err(|_| FwCfgError::OutOfRange)?;
        let addr = buf.as_mut_ptr() as u64;
        self.op(CTL_READ, len, addr)
    }

    /// Select `key` and write `buf` into its data (DMA write).
    ///
    /// # Errors
    ///
    /// As [`Self::read_item`].
    pub fn write_item(&self, key: u16, buf: &[u8]) -> Result<(), FwCfgError> {
        let len = u32::try_from(buf.len()).map_err(|_| FwCfgError::OutOfRange)?;
        let addr = buf.as_ptr() as u64;
        let control = (u32::from(key) << 16) | CTL_SELECT | CTL_WRITE;
        self.op(control, len, addr)
    }
}
