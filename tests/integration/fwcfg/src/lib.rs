//! Transport-agnostic QEMU `fw_cfg` DMA client plus the `ramfb`
//! programming helper the display-class QEMU verticals share.
//!
//! The `fw_cfg` DMA protocol is identical across platforms — an in-RAM
//! [`FWCfgDmaAccess`](https://www.qemu.org/docs/master/specs/fw_cfg.html)
//! structure (all fields big-endian) whose physical address is written
//! to the device's 64-bit big-endian DMA address register; writing the
//! least-significant half triggers a synchronous operation. Only the
//! *location* of that register differs: it is MMIO at `base + 16` on the
//! Arm/riscv `virt` boards and the I/O ports `0x514`/`0x518` on x86. That
//! single difference is the [`DmaAddressRegister`] seam each vertical
//! supplies; everything else — request staging, file-directory scanning,
//! and `RAMFBCfg` programming — lives here once (`AGENTS.md` §2.2 — the
//! transport is the only deliberate parallel-implementation difference,
//! not duplicated logic).
//!
//! The client assumes the staging structure and the data buffers live in
//! identity-mapped RAM, so a buffer's virtual address is the physical
//! address QEMU's DMA engine reads/writes. Both display verticals run
//! under that assumption (riscv64 `virt` boots with paging off; the
//! x86_64 boot identity-maps the bottom 4 GiB).

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::vec;
use core::ptr;
use core::sync::atomic::{compiler_fence, Ordering};

/// `FW_CFG_SIGNATURE` selector key; reading four bytes yields `QEMU`.
pub const KEY_SIGNATURE: u16 = 0x0000;
/// `FW_CFG_FILE_DIR` selector key.
pub const KEY_FILE_DIR: u16 = 0x0019;

/// `fw_cfg` item name QEMU's `ramfb` device registers.
pub const RAMFB_ITEM: &str = "etc/ramfb";

/// DRM `XRGB8888` fourcc (`'X','R','2','4'`). In memory the byte order
/// is B, G, R, X, matching a 32-bpp little-endian BGRA scan-out word.
pub const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;

/// `control` bit: the device sets this when an operation failed.
const CTL_ERROR: u32 = 1 << 0;
/// `control` bit: perform a read (device → guest RAM).
const CTL_READ: u32 = 1 << 1;
/// `control` bit: select the item named in the upper 16 bits.
const CTL_SELECT: u32 = 1 << 3;
/// `control` bit: perform a write (guest RAM → device).
const CTL_WRITE: u32 = 1 << 4;

/// Bytes per `FWCfgFile` directory entry (`size` u32 + `select` u16 +
/// `reserved` u16 + `name[56]`).
const DIR_ENTRY_LEN: usize = 64;
/// Offset of the big-endian `select` u16 within a directory entry.
const DIR_SELECT_OFFSET: usize = 4;
/// Offset of the null-terminated name within a directory entry.
const DIR_NAME_OFFSET: usize = 8;
/// Upper bound on directory entries scanned (keeps the read buffer
/// bounded; a real directory is far smaller).
const MAX_DIR_ENTRIES: usize = 256;

/// Wire length in bytes of a `RAMFBCfg` structure.
pub const RAMFB_CFG_LEN: usize = 28;

/// The device's 64-bit big-endian DMA address register.
///
/// The single platform-specific seam of the `fw_cfg` DMA client. An
/// implementor writes `dma_phys` into the register most-significant half
/// first; the least-significant-half write triggers the operation
/// QEMU then completes synchronously.
pub trait DmaAddressRegister {
    /// Write the physical address of a staged `FWCfgDmaAccess` to the
    /// device register, triggering the operation.
    fn write_dma_address(&self, dma_phys: u64);

    /// Translate a kernel virtual address (of a DMA control structure or
    /// a data buffer) into the physical address QEMU's DMA engine
    /// dereferences.
    ///
    /// Defaults to the identity map, which is correct for host tests and
    /// for any platform whose kernel buffers are identity-mapped. A
    /// higher-half kernel — whose statics and heap live at virtual
    /// `KERNEL_VMA_BASE + phys` — overrides this to recover the physical
    /// address (see the x86_64 `IoPortDma`). Without it a heap buffer's
    /// high virtual address would be handed to the device verbatim and
    /// the transfer would fault.
    fn to_physical(&self, virt: u64) -> u64 {
        virt
    }
}

/// In-RAM DMA control structure (QEMU `FWCfgDmaAccess`). All fields are
/// big-endian on the wire; staged through volatile stores in
/// [`FwCfg::op`]. One lives on the stack per operation; its address and
/// the data-buffer address are mapped to physical through
/// [`DmaAddressRegister::to_physical`] before they reach the device.
#[repr(C, align(8))]
struct FWCfgDmaAccess {
    control: u32,
    length: u32,
    address: u64,
}

/// Failure modes of the `fw_cfg` client.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FwCfgError {
    /// The device reported the error bit (or a non-zero residual
    /// `control`) after an operation.
    Transfer,
    /// A buffer length or address did not fit the device's `u32`/`u64`
    /// registers.
    OutOfRange,
    /// The signature item did not read back as `QEMU`.
    SignatureMismatch,
    /// The file directory reported an implausible entry count.
    DirectoryTooLarge,
    /// The requested file item was not present in the directory.
    ItemNotFound,
}

/// A `fw_cfg` DMA client parameterised over the platform's DMA
/// address-register transport.
pub struct FwCfg<T: DmaAddressRegister> {
    dma: T,
}

impl<T: DmaAddressRegister> FwCfg<T> {
    /// Wrap a DMA address-register transport.
    #[must_use]
    pub const fn new(dma: T) -> Self {
        Self { dma }
    }

    /// Stage a [`FWCfgDmaAccess`] on the stack (fields in the device's
    /// big-endian wire form), trigger the operation through the
    /// transport, and confirm the device cleared `control`.
    ///
    /// # Errors
    ///
    /// [`FwCfgError::Transfer`] if the device sets the error bit or
    /// leaves a non-zero residual `control`.
    fn op(&self, control: u32, length: u32, address: u64) -> Result<(), FwCfgError> {
        let mut access = FWCfgDmaAccess {
            control: 0,
            length: 0,
            address: 0,
        };
        // Stage the request in the device's big-endian wire form. Volatile
        // stores keep the writes from being reordered past the trigger.
        //
        // SAFETY: `&mut access` is a unique, 8-byte-aligned stack local
        // that lives across the synchronous DMA; the stores touch only
        // its own fields.
        let buffer_phys = self.dma.to_physical(address);
        unsafe {
            ptr::write_volatile(ptr::addr_of_mut!(access.control), control.to_be());
            ptr::write_volatile(ptr::addr_of_mut!(access.length), length.to_be());
            ptr::write_volatile(ptr::addr_of_mut!(access.address), buffer_phys.to_be());
        }

        let dma_phys = self.dma.to_physical(ptr::addr_of!(access) as u64);
        compiler_fence(Ordering::SeqCst);
        self.dma.write_dma_address(dma_phys);
        compiler_fence(Ordering::SeqCst);

        // SAFETY: QEMU completes the operation synchronously and clears
        // `control`; this volatile read observes the result of the same
        // live stack local.
        let result = unsafe { ptr::read_volatile(ptr::addr_of!(access.control)) };
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
        self.op((u32::from(key) << 16) | CTL_SELECT | CTL_READ, len, addr)
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
        self.op((u32::from(key) << 16) | CTL_SELECT | CTL_WRITE, len, addr)
    }

    /// Confirm the device answers with its `QEMU` signature, a
    /// round-trip sanity check on the DMA read path.
    ///
    /// # Errors
    ///
    /// [`FwCfgError::SignatureMismatch`] if the bytes are not `QEMU`;
    /// [`FwCfgError::Transfer`] on a device-reported failure.
    pub fn check_signature(&self) -> Result<(), FwCfgError> {
        let mut sig = [0u8; 4];
        self.read_item(KEY_SIGNATURE, &mut sig)?;
        if &sig != b"QEMU" {
            return Err(FwCfgError::SignatureMismatch);
        }
        Ok(())
    }

    /// Find the selector key of the named file item in the `fw_cfg`
    /// file directory.
    ///
    /// # Errors
    ///
    /// [`FwCfgError::DirectoryTooLarge`] if the directory reports an
    /// implausible entry count; [`FwCfgError::ItemNotFound`] if `name`
    /// is absent; [`FwCfgError::Transfer`] on a device-reported failure.
    pub fn file_selector(&self, name: &str) -> Result<u16, FwCfgError> {
        let mut count_buf = [0u8; 4];
        self.read_item(KEY_FILE_DIR, &mut count_buf)?;
        let count = u32::from_be_bytes(count_buf) as usize;
        if count == 0 || count > MAX_DIR_ENTRIES {
            return Err(FwCfgError::DirectoryTooLarge);
        }
        let mut entries = vec![0u8; count * DIR_ENTRY_LEN];
        self.read_more(&mut entries)?;
        find_selector(&entries, name).ok_or(FwCfgError::ItemNotFound)
    }

    /// Program QEMU's `ramfb` device to scan out from `cfg`.
    ///
    /// Locates the `etc/ramfb` item, verifies the device signature, and
    /// DMA-writes the big-endian [`RamfbConfig`] into it.
    ///
    /// # Errors
    ///
    /// Any error from [`Self::check_signature`], [`Self::file_selector`],
    /// or [`Self::write_item`].
    pub fn program_ramfb(&self, cfg: &RamfbConfig) -> Result<(), FwCfgError> {
        self.check_signature()?;
        let selector = self.file_selector(RAMFB_ITEM)?;
        self.write_item(selector, &cfg.to_be_bytes())
    }
}

/// Scan a raw `fw_cfg` file directory entry buffer for `name`, returning
/// its big-endian selector key. Pure helper, separated for host
/// unit-testing.
fn find_selector(entries: &[u8], name: &str) -> Option<u16> {
    for entry in entries.chunks_exact(DIR_ENTRY_LEN) {
        let name_bytes = &entry[DIR_NAME_OFFSET..];
        let end = name_bytes.iter().position(|&b| b == 0).unwrap_or(0);
        if &name_bytes[..end] == name.as_bytes() {
            return Some(u16::from_be_bytes([
                entry[DIR_SELECT_OFFSET],
                entry[DIR_SELECT_OFFSET + 1],
            ]));
        }
    }
    None
}

/// Geometry handed to QEMU's `ramfb` device (`RAMFBCfg`).
///
/// All fields are serialised big-endian by [`RamfbConfig::to_be_bytes`],
/// the wire form the device expects.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct RamfbConfig {
    /// Physical base address of the scan-out surface in guest RAM.
    pub phys_base: u64,
    /// DRM fourcc pixel format (e.g. [`DRM_FORMAT_XRGB8888`]).
    pub drm_format: u32,
    /// Reserved flags (zero).
    pub flags: u32,
    /// Surface width in pixels.
    pub width: u32,
    /// Surface height in pixels.
    pub height: u32,
    /// Scanline stride in bytes.
    pub stride: u32,
}

impl RamfbConfig {
    /// Serialise to the device's 28-byte big-endian `RAMFBCfg` layout.
    #[must_use]
    pub fn to_be_bytes(&self) -> [u8; RAMFB_CFG_LEN] {
        let mut cfg = [0u8; RAMFB_CFG_LEN];
        cfg[0..8].copy_from_slice(&self.phys_base.to_be_bytes());
        cfg[8..12].copy_from_slice(&self.drm_format.to_be_bytes());
        cfg[12..16].copy_from_slice(&self.flags.to_be_bytes());
        cfg[16..20].copy_from_slice(&self.width.to_be_bytes());
        cfg[20..24].copy_from_slice(&self.height.to_be_bytes());
        cfg[24..28].copy_from_slice(&self.stride.to_be_bytes());
        cfg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Build a single 64-byte file-directory entry naming `name` with
    /// big-endian selector `select`.
    fn entry(select: u16, name: &str) -> Vec<u8> {
        let mut e = vec![0u8; DIR_ENTRY_LEN];
        e[DIR_SELECT_OFFSET..DIR_SELECT_OFFSET + 2].copy_from_slice(&select.to_be_bytes());
        let bytes = name.as_bytes();
        e[DIR_NAME_OFFSET..DIR_NAME_OFFSET + bytes.len()].copy_from_slice(bytes);
        e
    }

    #[test]
    fn find_selector_matches_the_named_entry() {
        let mut dir = entry(0x0041, "etc/other");
        dir.extend_from_slice(&entry(0x0030, RAMFB_ITEM));
        assert_eq!(find_selector(&dir, RAMFB_ITEM), Some(0x0030));
    }

    #[test]
    fn find_selector_returns_none_when_absent() {
        let dir = entry(0x0041, "etc/other");
        assert_eq!(find_selector(&dir, RAMFB_ITEM), None);
    }

    #[test]
    fn find_selector_does_not_prefix_match() {
        // "etc/ramfb-extra" must not match the shorter "etc/ramfb".
        let dir = entry(0x0030, "etc/ramfb-extra");
        assert_eq!(find_selector(&dir, RAMFB_ITEM), None);
    }

    #[test]
    fn ramfb_config_serialises_big_endian() {
        let cfg = RamfbConfig {
            phys_base: 0x1122_3344_5566_7788,
            drm_format: DRM_FORMAT_XRGB8888,
            flags: 0,
            width: 64,
            height: 48,
            stride: 256,
        };
        let bytes = cfg.to_be_bytes();
        assert_eq!(&bytes[0..8], &0x1122_3344_5566_7788u64.to_be_bytes());
        assert_eq!(&bytes[8..12], &DRM_FORMAT_XRGB8888.to_be_bytes());
        assert_eq!(&bytes[12..16], &[0, 0, 0, 0]);
        assert_eq!(&bytes[16..20], &64u32.to_be_bytes());
        assert_eq!(&bytes[20..24], &48u32.to_be_bytes());
        assert_eq!(&bytes[24..28], &256u32.to_be_bytes());
    }
}
