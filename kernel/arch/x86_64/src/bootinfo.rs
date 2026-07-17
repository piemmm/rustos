//! Boot-protocol record and protocol-neutral boot-info access.
//!
//! The x86_64 trampoline (`boot.s`) is entered by one of two loaders —
//! GRUB's multiboot2 loader (`_start`) or QEMU's PVH direct-boot ELF
//! loader (`pvh_start`) — and both hand `entry.rs` a magic plus a
//! boot-info physical address. `entry.rs` validates the magic and
//! records the protocol here, exactly once, before `kernel_main` runs.
//!
//! Consumers (the `tairix-kernel` boot pipeline and the SMP QEMU
//! verticals) then obtain a [`BootData`] via [`BootData::load`] and read
//! the memory map and RSDP through it without caring which loader
//! booted the machine. This is the one place the two protocols are told
//! apart; a third copy of the dispatch is forbidden duplication.

use core::sync::atomic::{AtomicU8, Ordering};

use crate::{acpi, multiboot2, pvh};

/// Which loader entered the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootProtocol {
    /// GRUB (or another multiboot2 bootloader) entered `_start`.
    Multiboot2,
    /// QEMU's `-kernel` PVH ELF loader entered `pvh_start`.
    Pvh,
}

const PROTO_UNSET: u8 = 0;
const PROTO_MULTIBOOT2: u8 = 1;
const PROTO_PVH: u8 = 2;

/// The recorded protocol. Written exactly once by [`record`] on the BSP
/// before `kernel_main`; read-only afterwards (APs included).
static PROTOCOL: AtomicU8 = AtomicU8::new(PROTO_UNSET);

/// A second [`record`] call — the boot path runs once, so this is a
/// boot-path defect the caller must fail closed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadyRecorded;

/// Record the protocol the trampoline was entered by. Called exactly
/// once, by the `entry.rs` boot stub, before `kernel_main`; a second
/// call is refused so the record can never be rewritten.
pub fn record(protocol: BootProtocol) -> Result<(), AlreadyRecorded> {
    record_in(&PROTOCOL, protocol)
}

/// The protocol the machine booted with, or `None` before [`record`]
/// (e.g. on a host unit-test build, where no trampoline ran).
#[must_use]
pub fn protocol() -> Option<BootProtocol> {
    protocol_in(&PROTOCOL)
}

/// [`record`] against an injected slot (unit-testable without touching
/// the process-global static).
fn record_in(slot: &AtomicU8, protocol: BootProtocol) -> Result<(), AlreadyRecorded> {
    let raw = match protocol {
        BootProtocol::Multiboot2 => PROTO_MULTIBOOT2,
        BootProtocol::Pvh => PROTO_PVH,
    };
    slot.compare_exchange(PROTO_UNSET, raw, Ordering::Release, Ordering::Relaxed)
        .map(|_| ())
        .map_err(|_| AlreadyRecorded)
}

/// [`protocol`] against an injected slot.
fn protocol_in(slot: &AtomicU8) -> Option<BootProtocol> {
    match slot.load(Ordering::Acquire) {
        PROTO_MULTIBOOT2 => Some(BootProtocol::Multiboot2),
        PROTO_PVH => Some(BootProtocol::Pvh),
        _ => None,
    }
}

/// Why [`BootData::load`] refused the boot-info blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootDataError {
    /// No protocol was recorded — the trampoline never ran (or the
    /// entry magic was invalid, which `entry.rs` already fails on).
    NoProtocolRecorded,
    /// The multiboot2 record failed structural validation.
    Multiboot2(multiboot2::ParseError),
    /// The PVH start-info record or its memory-map table failed
    /// structural validation.
    Pvh(pvh::ParseError),
}

/// Protocol-neutral view of the loader-provided boot information.
#[derive(Debug, Clone, Copy)]
pub enum BootData<'a> {
    /// Multiboot2 tag stream (GRUB path).
    Multiboot2(multiboot2::BootInfo<'a>),
    /// PVH start-info record plus its memory-map table (QEMU `-kernel`
    /// path).
    Pvh {
        /// The validated version-1 start-info fields.
        start_info: pvh::StartInfo,
        /// The validated memory-map table the record points at.
        memmap: pvh::MemoryMap<'a>,
    },
}

impl BootData<'static> {
    /// Build the protocol-neutral view from the verbatim boot-info
    /// pointer the trampoline handed `kernel_main`.
    ///
    /// Dispatches on the protocol [`record`]ed at entry and validates
    /// the whole blob before returning — fail closed on any structural
    /// defect.
    ///
    /// # Safety
    ///
    /// `boot_info` must be the verbatim pointer the boot trampoline
    /// passed to `kernel_main` (`boot.s` SAFETY-INVARIANT 7). The blob
    /// and every table it points at sit below 4 GiB in the trampoline's
    /// identity-mapped window (SAFETY-INVARIANT 4), so the raw reads
    /// below stay inside mapped memory the loader populated.
    pub unsafe fn load(boot_info: u64) -> Result<Self, BootDataError> {
        match protocol().ok_or(BootDataError::NoProtocolRecorded)? {
            BootProtocol::Multiboot2 => {
                // The record's first 4 bytes are `total_size`; bound the
                // full slice by it, then let the validator re-check the
                // structure.
                //
                // SAFETY: the caller guarantees `boot_info` is the
                // loader-published record inside the identity-mapped
                // window (see the function contract above).
                let header = unsafe { core::slice::from_raw_parts(boot_info as *const u8, 8) };
                let total_size =
                    u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
                // SAFETY: same contract; `total_size` is the loader's
                // stated length of its own record, which the validator
                // re-bounds.
                let bytes =
                    unsafe { core::slice::from_raw_parts(boot_info as *const u8, total_size) };
                multiboot2::BootInfo::parse(bytes)
                    .map(BootData::Multiboot2)
                    .map_err(BootDataError::Multiboot2)
            }
            BootProtocol::Pvh => {
                // SAFETY: the caller guarantees `boot_info` is the
                // loader-published `hvm_start_info` inside the
                // identity-mapped window (see the function contract).
                let bytes = unsafe {
                    core::slice::from_raw_parts(boot_info as *const u8, pvh::START_INFO_V1_LEN)
                };
                let start_info = pvh::StartInfo::parse(bytes).map_err(BootDataError::Pvh)?;
                // `parse` guaranteed `memmap_paddr != 0` and a non-zero,
                // non-overflowing entry count.
                let len = start_info
                    .memmap_len_bytes()
                    .ok_or(BootDataError::Pvh(pvh::ParseError::Truncated))?;
                // SAFETY: `memmap_paddr` is the loader-published table
                // address inside the same identity-mapped window; `len`
                // is the loader's stated table length, which the
                // validator re-bounds.
                let table = unsafe {
                    core::slice::from_raw_parts(start_info.memmap_paddr as *const u8, len)
                };
                let memmap = pvh::MemoryMap::parse(table, start_info.memmap_entries)
                    .map_err(BootDataError::Pvh)?;
                Ok(BootData::Pvh { start_info, memmap })
            }
        }
    }
}

impl BootData<'_> {
    /// Locate and validate the ACPI RSDP, whichever protocol delivered
    /// it. A PVH loader that publishes no usable `rsdp_paddr` (QEMU's
    /// direct-boot start-info stopped carrying one) falls back to the
    /// ACPI 6.5 §5.2.5.1 scan of the legacy BIOS window the machine
    /// firmware publishes the RSDP in. `None` when neither source
    /// yields a record that passes validation — fail closed.
    ///
    /// # Safety
    ///
    /// Same contract as [`BootData::load`]: the loader-published tables
    /// and the legacy BIOS window live below 4 GiB in the
    /// identity-mapped window, so reading [`acpi::RSDP_V2_LEN`] bytes
    /// at the PVH `rsdp_paddr` and scanning
    /// [`acpi::LEGACY_REGION_LEN`] bytes at
    /// [`acpi::LEGACY_REGION_BASE`] stay inside mapped memory.
    #[must_use]
    pub unsafe fn validated_rsdp(&self) -> Option<acpi::Rsdp> {
        match self {
            BootData::Multiboot2(info) => acpi::Rsdp::validate(info.rsdp()?).ok(),
            BootData::Pvh { start_info, .. } => {
                if start_info.rsdp_paddr != 0 {
                    // SAFETY: `rsdp_paddr` is the loader-published RSDP
                    // address inside the identity-mapped window (the
                    // function contract); the v2 length is the widest
                    // form and `Rsdp::validate` re-checks both
                    // checksums.
                    let bytes = unsafe {
                        core::slice::from_raw_parts(
                            start_info.rsdp_paddr as *const u8,
                            acpi::RSDP_V2_LEN,
                        )
                    };
                    if let Ok(rsdp) = acpi::Rsdp::validate(bytes) {
                        return Some(rsdp);
                    }
                }
                // SAFETY: the legacy BIOS window is firmware-populated
                // physical memory below 4 GiB inside the same
                // identity-mapped window (the function contract);
                // `find_rsdp` checksum-validates every candidate before
                // accepting it.
                let region = unsafe {
                    core::slice::from_raw_parts(
                        acpi::LEGACY_REGION_BASE as *const u8,
                        acpi::LEGACY_REGION_LEN,
                    )
                };
                acpi::find_rsdp(region).map(|(_, rsdp)| rsdp)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_is_set_once_and_readable() {
        let slot = AtomicU8::new(PROTO_UNSET);
        assert_eq!(protocol_in(&slot), None);
        assert_eq!(record_in(&slot, BootProtocol::Pvh), Ok(()));
        assert_eq!(protocol_in(&slot), Some(BootProtocol::Pvh));
    }

    #[test]
    fn second_record_fails_closed() {
        let slot = AtomicU8::new(PROTO_UNSET);
        assert_eq!(record_in(&slot, BootProtocol::Multiboot2), Ok(()));
        assert_eq!(record_in(&slot, BootProtocol::Pvh), Err(AlreadyRecorded));
        // The first record survives the rejected second attempt.
        assert_eq!(protocol_in(&slot), Some(BootProtocol::Multiboot2));
    }
}
