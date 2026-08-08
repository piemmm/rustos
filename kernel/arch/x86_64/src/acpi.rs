//! Minimal `no_alloc` ACPI table parser sufficient for SMP bring-up.
//!
//! What this module exposes:
//!
//! * [`Rsdp`] — validation of the Root System Description Pointer
//!   structure handed in by the Multiboot2 ACPI 1.0 (tag 14) or
//!   ACPI 2.0 (tag 15) tag. v1 is a 20-byte block ending in a single
//!   one-byte checksum over the whole block; v2 extends to 36 bytes
//!   with an additional `extended_checksum` covering all 36 bytes.
//! * [`SdtHeader`] — generic 36-byte ACPI System Description Table
//!   header with checksum validation.
//! * [`Madt`] / [`MadtIter`] / [`MadtEntry`] — typed iterator over the
//!   APIC Interrupt Controller Structures defined in ACPI 6.5 §5.2.12.
//!
//! Higher-level code calls [`Rsdp::validate`] on the Multiboot2-supplied
//! payload to recover the (R|X)SDT physical address, then re-enters this
//! module with the kernel-mapped (R|X)SDT bytes to find the MADT and
//! enumerate the Local APIC IDs feeding the SMP bring-up sequence.
//!
//! Nothing here touches MMIO. Everything is pure byte-slice parsing,
//! making the whole surface trivially host-unit-testable.
//!
//! References:
//! * ACPI Specification, Revision 6.5, §5.2.5 (RSDP) and §5.2.12 (MADT).

#![allow(clippy::module_name_repetitions)]

use core::mem::size_of;

/// Errors returned by RSDP / SDT / MADT validators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcpiError {
    /// Payload shorter than the structure requires.
    Truncated,
    /// Signature mismatch (e.g. RSDP `"RSD PTR "` or MADT `"APIC"`).
    BadSignature,
    /// One-byte modular checksum did not sum to zero.
    BadChecksum,
    /// Stated `length` field is inconsistent with the input or the
    /// minimum imposed by the table type.
    BadLength,
    /// Unsupported ACPI revision (only 0 and ≥ 2 are accepted for RSDP).
    UnsupportedRevision,
}

/// Sum every byte of `bytes` and return whether the low 8 bits are zero,
/// the ACPI checksum convention for both the RSDP and SDT headers.
fn checksum_zero(bytes: &[u8]) -> bool {
    bytes.iter().fold(0u8, |acc, b| acc.wrapping_add(*b)) == 0
}

// --- RSDP (ACPI 6.5 §5.2.5) ------------------------------------------

/// 8-byte signature at the head of every RSDP.
pub const RSDP_SIGNATURE: [u8; 8] = *b"RSD PTR ";

/// Byte length of a v2.0+ RSDP (ACPI 6.5 §5.2.5.3) — the widest form,
/// and therefore the read size a consumer holding only a physical
/// address (the PVH `rsdp_paddr`) uses to slice the record before
/// [`Rsdp::validate`] re-checks both checksums.
pub const RSDP_V2_LEN: usize = 36;

/// Physical base of the legacy BIOS read-only window the RSDP may live
/// in when the boot protocol supplies no pointer (ACPI 6.5 §5.2.5.1:
/// `0xE0000..=0xFFFFF`). `SeaBIOS` publishes its RSDP here, so a PVH
/// boot whose start-info carries no `rsdp_paddr` recovers it by
/// scanning this window with [`find_rsdp`].
pub const LEGACY_REGION_BASE: u64 = 0xE_0000;

/// Byte length of the [`LEGACY_REGION_BASE`] scan window.
pub const LEGACY_REGION_LEN: usize = 0x2_0000;

/// Decoded RSDP, both v1 and v2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rsdp {
    /// ACPI revision: 0 for v1.0, ≥ 2 for v2.0+.
    pub revision: u8,
    /// Physical address of the RSDT (32-bit table pointers).
    pub rsdt_address: u32,
    /// Physical address of the XSDT (64-bit pointers). 0 for v1.0.
    pub xsdt_address: u64,
}

impl Rsdp {
    /// Validate the supplied RSDP bytes and decode the SDT pointers.
    ///
    /// `bytes` must be at least 20 bytes long (ACPI 1.0). If it is at
    /// least 36 bytes the v2.0 extended fields are validated too and
    /// `xsdt_address` is populated.
    ///
    /// # Errors
    ///
    /// Returns [`AcpiError`] on any structural defect — closed-fail.
    pub fn validate(bytes: &[u8]) -> Result<Self, AcpiError> {
        if bytes.len() < 20 {
            return Err(AcpiError::Truncated);
        }
        if bytes[..8] != RSDP_SIGNATURE {
            return Err(AcpiError::BadSignature);
        }
        // v1 checksum covers the first 20 bytes.
        if !checksum_zero(&bytes[..20]) {
            return Err(AcpiError::BadChecksum);
        }
        let revision = bytes[15];
        let rsdt_address = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
        if revision == 0 {
            return Ok(Self {
                revision,
                rsdt_address,
                xsdt_address: 0,
            });
        }
        if revision < 2 {
            return Err(AcpiError::UnsupportedRevision);
        }
        if bytes.len() < 36 {
            return Err(AcpiError::Truncated);
        }
        // v2 stated length lives at offset 20..24; we don't require
        // exact equality (the firmware may extend the record) but it
        // must cover at least 36 bytes and must fit the buffer.
        let stated_len = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]) as usize;
        if stated_len < 36 || stated_len > bytes.len() {
            return Err(AcpiError::BadLength);
        }
        // Extended checksum covers the full stated_len.
        if !checksum_zero(&bytes[..stated_len]) {
            return Err(AcpiError::BadChecksum);
        }
        let xsdt_address = u64::from_le_bytes([
            bytes[24], bytes[25], bytes[26], bytes[27], bytes[28], bytes[29], bytes[30], bytes[31],
        ]);
        Ok(Self {
            revision,
            rsdt_address,
            xsdt_address,
        })
    }
}

/// Scan `region` — a mapping (or copy) of a legacy BIOS window such as
/// [`LEGACY_REGION_BASE`] — for a valid RSDP and return its byte offset
/// and decoded form.
///
/// The RSDP is guaranteed to sit on a 16-byte boundary (ACPI 6.5
/// §5.2.5.1), so only aligned offsets are considered; every candidate
/// signature is checksum-validated through [`Rsdp::validate`] before it
/// is accepted — a corrupt or decoy record is skipped, never trusted.
#[must_use]
pub fn find_rsdp(region: &[u8]) -> Option<(usize, Rsdp)> {
    let mut offset = 0;
    while offset + 20 <= region.len() {
        if region[offset..offset + 8] == RSDP_SIGNATURE {
            let end = (offset + RSDP_V2_LEN).min(region.len());
            if let Ok(rsdp) = Rsdp::validate(&region[offset..end]) {
                return Some((offset, rsdp));
            }
        }
        offset += 16;
    }
    None
}

// --- SDT header (ACPI 6.5 §5.2.6) ------------------------------------

/// Length in bytes of the ACPI System Description Table common header.
pub const SDT_HEADER_LEN: usize = 36;

/// Decoded ACPI table header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdtHeader {
    /// 4-byte ASCII signature (e.g. `"APIC"`, `"XSDT"`, `"RSDT"`).
    pub signature: [u8; 4],
    /// Total length of the table including the header.
    pub length: u32,
    /// Revision of the table layout.
    pub revision: u8,
}

impl SdtHeader {
    /// Validate the header at the start of `bytes` and confirm the
    /// 8-bit checksum across `length` bytes is zero.
    ///
    /// `expected_sig` lets the caller refuse a mismatch up-front (e.g.
    /// the MADT walker passes `*b"APIC"`).
    ///
    /// # Errors
    ///
    /// [`AcpiError::Truncated`] if `bytes.len() < length`, or
    /// [`AcpiError::BadSignature`] / [`AcpiError::BadChecksum`].
    pub fn validate(bytes: &[u8], expected_sig: &[u8; 4]) -> Result<Self, AcpiError> {
        if bytes.len() < SDT_HEADER_LEN {
            return Err(AcpiError::Truncated);
        }
        let signature = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if &signature != expected_sig {
            return Err(AcpiError::BadSignature);
        }
        let length = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        if length < SDT_HEADER_LEN || length > bytes.len() {
            return Err(AcpiError::BadLength);
        }
        if !checksum_zero(&bytes[..length]) {
            return Err(AcpiError::BadChecksum);
        }
        // `length` came from a u32 in the input and the bounds check
        // above proved it fits the slice, so this back-conversion can
        // never truncate.
        let length_u32 = u32::try_from(length).map_err(|_| AcpiError::BadLength)?;
        Ok(Self {
            signature,
            length: length_u32,
            revision: bytes[8],
        })
    }
}

// --- MADT (ACPI 6.5 §5.2.12) -----------------------------------------

/// 4-byte ASCII signature for the Multiple APIC Description Table.
pub const MADT_SIGNATURE: [u8; 4] = *b"APIC";

/// MADT-wide flags as defined in ACPI 6.5 §5.2.12.
///
/// Bit 0: `PCAT_COMPAT` — the system has a dual-8259 setup that must
/// be masked before enabling the APIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MadtFlags(pub u32);

impl MadtFlags {
    /// `true` iff the legacy 8259 PIC is wired and must be masked.
    #[must_use]
    pub fn pcat_compat(self) -> bool {
        (self.0 & 0x1) != 0
    }
}

/// Validated MADT view over a kernel-mapped byte slice.
#[derive(Debug, Clone, Copy)]
pub struct Madt<'a> {
    /// Physical address of the boot CPU's LAPIC MMIO window (from the
    /// MADT fixed-length area, may be overridden by entry type 5).
    pub lapic_address: u32,
    /// MADT-wide flags.
    pub flags: MadtFlags,
    entries: &'a [u8],
}

impl<'a> Madt<'a> {
    /// Validate the MADT in `bytes` and decode the fixed-length area.
    ///
    /// # Errors
    ///
    /// Returns [`AcpiError`] on any structural defect.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, AcpiError> {
        let header = SdtHeader::validate(bytes, &MADT_SIGNATURE)?;
        // MADT's fixed area: SDT header (36) + lapic_address (4) +
        // flags (4) = 44 bytes; entries follow.
        if (header.length as usize) < 44 {
            return Err(AcpiError::BadLength);
        }
        let lapic_address = u32::from_le_bytes([bytes[36], bytes[37], bytes[38], bytes[39]]);
        let flags = u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]);
        let entries = &bytes[44..header.length as usize];
        Ok(Self {
            lapic_address,
            flags: MadtFlags(flags),
            entries,
        })
    }

    /// Iterate the Interrupt Controller Structures following the
    /// fixed-length area.
    #[must_use]
    pub fn entries(&self) -> MadtIter<'a> {
        MadtIter { rest: self.entries }
    }
}

/// One Interrupt Controller Structure from the MADT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MadtEntry {
    /// Type 0 — Processor Local APIC.
    LocalApic {
        /// ACPI processor UID.
        processor_uid: u8,
        /// Local APIC ID (target of INIT-SIPI-SIPI).
        apic_id: u8,
        /// Per-entry flags. Bit 0 == "enabled".
        flags: u32,
    },
    /// Type 1 — I/O APIC.
    IoApic {
        /// I/O APIC ID.
        id: u8,
        /// MMIO address of this I/O APIC.
        address: u32,
        /// Global System Interrupt base for this I/O APIC's input lines.
        gsi_base: u32,
    },
    /// Type 2 — Interrupt Source Override.
    InterruptSourceOverride {
        /// Bus (always 0 for ISA).
        bus: u8,
        /// Source IRQ on the bus.
        source: u8,
        /// Global System Interrupt the source maps to.
        gsi: u32,
        /// MPS INTI flags (polarity, trigger mode).
        flags: u16,
    },
    /// Type 4 — Local APIC NMI source.
    LocalApicNmi {
        /// ACPI processor UID, or 0xFF for "all".
        processor_uid: u8,
        /// MPS INTI flags.
        flags: u16,
        /// LINT# pin number on the target CPU's LAPIC.
        lint: u8,
    },
    /// Type 5 — Local APIC Address Override (replaces `Madt::lapic_address`).
    LocalApicAddressOverride {
        /// 64-bit MMIO address of the LAPIC window.
        address: u64,
    },
    /// Any entry type not parsed by this module. Carries the raw type.
    Other(u8),
}

/// Iterator over the ICSes in a [`Madt`].
#[derive(Debug, Clone)]
pub struct MadtIter<'a> {
    rest: &'a [u8],
}

impl Iterator for MadtIter<'_> {
    type Item = MadtEntry;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.len() < 2 {
            return None;
        }
        let entry_type = self.rest[0];
        let len = self.rest[1] as usize;
        if len < 2 || len > self.rest.len() {
            // Malformed; refuse to advance further.
            self.rest = &[];
            return None;
        }
        let body = &self.rest[..len];
        self.rest = &self.rest[len..];
        Some(decode_entry(entry_type, body))
    }
}

fn decode_entry(ty: u8, body: &[u8]) -> MadtEntry {
    match (ty, body.len()) {
        (0, 8) => MadtEntry::LocalApic {
            processor_uid: body[2],
            apic_id: body[3],
            flags: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
        },
        (1, 12) => MadtEntry::IoApic {
            id: body[2],
            address: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
            gsi_base: u32::from_le_bytes([body[8], body[9], body[10], body[11]]),
        },
        (2, 10) => MadtEntry::InterruptSourceOverride {
            bus: body[2],
            source: body[3],
            gsi: u32::from_le_bytes([body[4], body[5], body[6], body[7]]),
            flags: u16::from_le_bytes([body[8], body[9]]),
        },
        (4, 6) => MadtEntry::LocalApicNmi {
            processor_uid: body[2],
            flags: u16::from_le_bytes([body[3], body[4]]),
            lint: body[5],
        },
        (5, 12) => MadtEntry::LocalApicAddressOverride {
            address: u64::from_le_bytes([
                body[4], body[5], body[6], body[7], body[8], body[9], body[10], body[11],
            ]),
        },
        _ => MadtEntry::Other(ty),
    }
}

// --- MADT discovery via (X|R)SDT walk (bare-metal only) --------------
//
// The following helpers walk the firmware-supplied XSDT (preferred) or
// RSDT to find the Multiple APIC Description Table. They read raw
// physical addresses through the boot trampoline's identity-mapped
// 0..4 GiB window (`boot.s` SAFETY-INVARIANT 4) and are therefore
// gated to the freestanding x86_64 target.
//
// — both the `tests/integration/kernel_arch_boot`
// boot test (Stage 3a (c7-bin)) and the existing `scheduler_stress_qemu`
// integration test need to find the MADT this way. Centralising the
// logic here removes the duplication that would otherwise grow with
// every new bin.

/// Length of a single ACPI SDT header (the per-table preamble).
///
/// Re-exported as a `const` (rather than `size_of::<SdtHeader>()`) so
/// host code can index into raw byte slices without depending on the
/// internal type layout (no leaking representation
/// details across an API).
pub const ACPI_SDT_HEADER_LEN: usize = SDT_HEADER_LEN;

// --- MCFG (PCI Firmware Specification §4.1.2) ------------------------

/// 4-byte ASCII signature for the PCI Express memory-mapped
/// configuration-space description table (ECAM).
pub const MCFG_SIGNATURE: [u8; 4] = *b"MCFG";

/// One ECAM configuration-space allocation from the MCFG: the physical
/// base of a segment group's memory-mapped configuration window and the
/// bus-number range it covers (PCI Firmware Specification §4.1.2).
///
/// A function's configuration space lives at
/// `base + (bus << 20) + (device << 15) + (function << 12)`, so the
/// window for `[start_bus, end_bus]` spans
/// `(end_bus - start_bus + 1) << 20` bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EcamAllocation {
    /// Physical base of this segment group's ECAM window.
    pub base: u64,
    /// PCI segment group number.
    pub segment: u16,
    /// First bus number the window covers.
    pub start_bus: u8,
    /// Last bus number the window covers (inclusive).
    pub end_bus: u8,
}

impl EcamAllocation {
    /// Byte length of this allocation's ECAM window: one 1 MiB region per
    /// bus in `[start_bus, end_bus]` (256 functions × 4 KiB each).
    #[must_use]
    pub fn window_len(&self) -> u64 {
        (u64::from(self.end_bus - self.start_bus) + 1) << 20
    }
}

/// Byte length of one MCFG configuration-space allocation structure
/// (base `u64` + segment `u16` + start/end bus `u8` + 4 reserved).
const MCFG_ALLOCATION_LEN: usize = 16;

/// Byte offset of the first allocation structure in an MCFG: the 36-byte
/// SDT header plus 8 reserved bytes.
const MCFG_ALLOCATIONS_OFFSET: usize = SDT_HEADER_LEN + 8;

/// Parse the **first** ECAM allocation from an MCFG byte slice.
///
/// The MCFG (PCI Firmware Specification §4.1.2) is an SDT header, 8
/// reserved bytes, then one or more 16-byte configuration-space
/// allocation structures. The first covers segment group 0 on every
/// platform TAIRiX targets, which is the bus the PCI probe enumerates, so
/// its base is the ECAM window the kernel maps.
///
/// This is a **pure** parser (no MMIO), host-tested, so the byte
/// validation is exercised off-target: the on-target (bare-metal)
/// `locate_mcfg` supplies the slice.
///
/// # Errors
///
/// Returns `None` (fail closed) if the signature is not [`MCFG_SIGNATURE`],
/// the declared length is short, or the table carries no allocation.
#[must_use]
pub fn mcfg_first_ecam(bytes: &[u8]) -> Option<EcamAllocation> {
    let header = SdtHeader::validate(bytes, &MCFG_SIGNATURE).ok()?;
    let len = header.length as usize;
    // The declared length must reach at least one whole allocation past
    // the reserved area, and must not exceed the slice we were handed.
    if len < MCFG_ALLOCATIONS_OFFSET + MCFG_ALLOCATION_LEN || len > bytes.len() {
        return None;
    }
    let a = &bytes[MCFG_ALLOCATIONS_OFFSET..MCFG_ALLOCATIONS_OFFSET + MCFG_ALLOCATION_LEN];
    let base = u64::from_le_bytes([a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]]);
    let segment = u16::from_le_bytes([a[8], a[9]]);
    let start_bus = a[10];
    let end_bus = a[11];
    // A window whose end precedes its start is malformed; refuse it rather
    // than compute a wrapping length.
    if end_bus < start_bus {
        return None;
    }
    Some(EcamAllocation {
        base,
        segment,
        start_bus,
        end_bus,
    })
}

/// Locate the MADT by walking the firmware (X|R)SDT pointed at by
/// `rsdp`.
///
/// Returns the MADT bytes (header + body, exactly `length` bytes long)
/// as a `'static` slice if found. The caller is expected to hand the
/// bytes to [`Madt::parse`].
///
/// # Safety
///
/// * `rsdp.xsdt_address` (or `rsdp.rsdt_address` when `xsdt_address`
///   is zero) must point at a complete, well-formed ACPI SDT inside
///   the boot trampoline's 0..4 GiB identity-mapped physical window.
/// * The firmware tables must remain unmodified for the duration of
///   the returned slice's `'static` lifetime. The ACPI specification
///   guarantees this for the boot-time tables.
///
/// Returns `None` if no entry of the (X|R)SDT advertises the
/// [`MADT_SIGNATURE`].
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use]
pub unsafe fn locate_madt(rsdp: &Rsdp) -> Option<&'static [u8]> {
    // SAFETY: forwarded — caller's contract pins the tables into the
    // identity-mapped window.
    unsafe { locate_sdt(rsdp, MADT_SIGNATURE) }
}

/// Locate the MCFG (PCI Express memory-mapped configuration space
/// description, PCI Firmware Specification §4.1.2) by walking the
/// firmware (X|R)SDT pointed at by `rsdp`.
///
/// Returns the MCFG bytes as a `'static` slice if found; the caller
/// hands them to [`mcfg_first_ecam`] to recover the ECAM base. The x86_64
/// PCI probe needs this to build a configuration-space bus.
///
/// # Safety
///
/// Identical to [`locate_madt`]: the firmware (X|R)SDT and every table it
/// references must lie in the identity-mapped 0..4 GiB window and remain
/// unmodified for the returned slice's `'static` lifetime.
///
/// Returns `None` if no entry advertises the [`MCFG_SIGNATURE`] (a
/// firmware without an ECAM description — the caller falls back or leaves
/// PCI undiscovered).
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use]
pub unsafe fn locate_mcfg(rsdp: &Rsdp) -> Option<&'static [u8]> {
    // SAFETY: forwarded — caller's contract pins the tables into the
    // identity-mapped window.
    unsafe { locate_sdt(rsdp, MCFG_SIGNATURE) }
}

/// Walk the firmware (X|R)SDT pointed at by `rsdp` for the first table
/// whose signature is `signature`, returning its bytes.
///
/// The one signature-parameterised walk both [`locate_madt`] and
/// [`locate_mcfg`] share, so the (X|R)SDT traversal is defined once.
///
/// # Safety
///
/// See [`locate_madt`].
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use]
unsafe fn locate_sdt(rsdp: &Rsdp, signature: [u8; 4]) -> Option<&'static [u8]> {
    if rsdp.xsdt_address != 0 {
        // SAFETY: forwarded — caller's contract pins the address into
        // the identity-mapped window.
        unsafe { locate_sdt_via_xsdt(rsdp.xsdt_address, signature) }
    } else {
        // SAFETY: forwarded.
        unsafe { locate_sdt_via_rsdt(u64::from(rsdp.rsdt_address), signature) }
    }
}

/// Walk an XSDT (64-bit entry pointers) for the table matching
/// `signature`.
///
/// # Safety
///
/// See [`locate_madt`]. `xsdt_phys` must point at a valid XSDT in the
/// identity-mapped 0..4 GiB window.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use]
unsafe fn locate_sdt_via_xsdt(xsdt_phys: u64, signature: [u8; 4]) -> Option<&'static [u8]> {
    // SAFETY: caller's contract — `xsdt_phys` is identity-mapped.
    let len = unsafe { read_phys_u32(xsdt_phys + 4) } as usize;
    if len < ACPI_SDT_HEADER_LEN {
        return None;
    }
    let n_entries = (len - ACPI_SDT_HEADER_LEN) / 8;
    for i in 0..n_entries {
        // SAFETY: caller's contract.
        let entry =
            unsafe { read_phys_u64(xsdt_phys + ACPI_SDT_HEADER_LEN as u64 + (i as u64) * 8) };
        // SAFETY: caller's contract.
        if let Some(bytes) = unsafe { try_sdt_at(entry, &signature) } {
            return Some(bytes);
        }
    }
    None
}

/// Walk an RSDT (32-bit entry pointers) for the table matching
/// `signature`.
///
/// # Safety
///
/// See [`locate_madt`]. `rsdt_phys` must point at a valid RSDT in the
/// identity-mapped 0..4 GiB window.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
#[must_use]
unsafe fn locate_sdt_via_rsdt(rsdt_phys: u64, signature: [u8; 4]) -> Option<&'static [u8]> {
    // SAFETY: caller's contract.
    let len = unsafe { read_phys_u32(rsdt_phys + 4) } as usize;
    if len < ACPI_SDT_HEADER_LEN {
        return None;
    }
    let n_entries = (len - ACPI_SDT_HEADER_LEN) / 4;
    for i in 0..n_entries {
        // SAFETY: caller's contract.
        let entry = u64::from(unsafe {
            read_phys_u32(rsdt_phys + ACPI_SDT_HEADER_LEN as u64 + (i as u64) * 4)
        });
        // SAFETY: caller's contract.
        if let Some(bytes) = unsafe { try_sdt_at(entry, &signature) } {
            return Some(bytes);
        }
    }
    None
}

/// Inspect a candidate SDT at `phys` and return the byte slice if its
/// signature matches `signature`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
unsafe fn try_sdt_at(phys: u64, signature: &[u8; 4]) -> Option<&'static [u8]> {
    // SAFETY: caller's contract pins `phys` into the identity-mapped
    // window. We only deref the first four bytes (signature) before
    // sizing the rest of the slice.
    let sig = unsafe { core::slice::from_raw_parts(phys as *const u8, 4) };
    if sig != signature {
        return None;
    }
    // SAFETY: caller's contract; the length is bounded by the
    // length field which the caller's parser re-validates.
    let len = unsafe { read_phys_u32(phys + 4) } as usize;
    // SAFETY: caller's contract.
    Some(unsafe { core::slice::from_raw_parts(phys as *const u8, len) })
}

/// Read a 4-byte little-endian `u32` from a physical address.
///
/// # Safety
///
/// `phys` must be inside the boot trampoline's 0..4 GiB identity map.
/// The four bytes at `phys..phys+4` must be a valid `u32`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
unsafe fn read_phys_u32(phys: u64) -> u32 {
    // SAFETY: caller's contract.
    unsafe { core::ptr::read_unaligned(phys as *const u32) }
}

/// Read an 8-byte little-endian `u64` from a physical address.
///
/// # Safety
///
/// `phys` must be inside the boot trampoline's 0..4 GiB identity map.
/// The eight bytes at `phys..phys+8` must be a valid `u64`.
#[cfg(all(target_arch = "x86_64", target_os = "none"))]
unsafe fn read_phys_u64(phys: u64) -> u64 {
    // SAFETY: caller's contract.
    unsafe { core::ptr::read_unaligned(phys as *const u64) }
}

// --- Compile-time sanity check ---------------------------------------

const _: () = {
    // SdtHeader layout: 4 + 4 + 1 + 1 + 6 + 8 + 4 + 4 + 4 = 36 bytes.
    assert!(SDT_HEADER_LEN == 36);
    // RSDP signature is 8 bytes by ACPI spec.
    assert!(size_of::<[u8; 8]>() == 8);
    // The public `ACPI_SDT_HEADER_LEN` and the module-private
    // `SDT_HEADER_LEN` must agree exactly. — no
    // duplicate-source-of-truth constants.
    assert!(ACPI_SDT_HEADER_LEN == SDT_HEADER_LEN);
};

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    extern crate std;
    use std::vec;
    use std::vec::Vec;

    // Helper: build a v1 RSDP with `revision`, `rsdt`, and a valid
    // checksum.
    fn build_rsdp_v1(revision: u8, rsdt: u32) -> [u8; 20] {
        let mut buf = [0u8; 20];
        buf[..8].copy_from_slice(&RSDP_SIGNATURE);
        // OEMID at 9..15 — leave zero.
        buf[15] = revision;
        buf[16..20].copy_from_slice(&rsdt.to_le_bytes());
        // Patch byte 8 (checksum) so the modular sum is zero.
        let s = buf.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        buf[8] = 0u8.wrapping_sub(s);
        buf
    }

    fn build_rsdp_v2(rsdt: u32, xsdt: u64) -> [u8; 36] {
        let mut buf = [0u8; 36];
        // Start from a valid v1 prefix with rev=2.
        let v1 = build_rsdp_v1(2, rsdt);
        buf[..20].copy_from_slice(&v1);
        // length at 20..24
        buf[20..24].copy_from_slice(&36u32.to_le_bytes());
        // xsdt at 24..32
        buf[24..32].copy_from_slice(&xsdt.to_le_bytes());
        // extended_checksum at byte 32; patch so full sum is zero.
        let s = buf.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        buf[32] = 0u8.wrapping_sub(s);
        buf
    }

    #[test]
    fn find_rsdp_locates_an_aligned_record() {
        let mut region = vec![0u8; 4096];
        let rsdp = build_rsdp_v1(0, 0x0FFE_22F4);
        region[0x150..0x150 + 20].copy_from_slice(&rsdp);
        let (offset, decoded) = find_rsdp(&region).expect("aligned RSDP must be found");
        assert_eq!(offset, 0x150);
        assert_eq!(decoded.rsdt_address, 0x0FFE_22F4);
    }

    #[test]
    fn find_rsdp_skips_a_corrupt_decoy_before_the_valid_record() {
        let mut region = vec![0u8; 4096];
        // A signature with a broken checksum at a lower aligned offset.
        region[0x100..0x100 + 8].copy_from_slice(&RSDP_SIGNATURE);
        let rsdp = build_rsdp_v2(0x1234_5678, 0xDEAD_BEEF_0000);
        region[0x200..0x200 + 36].copy_from_slice(&rsdp);
        let (offset, decoded) = find_rsdp(&region).expect("valid RSDP must be found");
        assert_eq!(offset, 0x200);
        assert_eq!(decoded.xsdt_address, 0xDEAD_BEEF_0000);
    }

    #[test]
    fn find_rsdp_ignores_a_misaligned_signature() {
        let mut region = vec![0u8; 4096];
        let rsdp = build_rsdp_v1(0, 1);
        // The spec guarantees 16-byte alignment; an unaligned copy is
        // not a legal RSDP and must not be returned.
        region[0x108..0x108 + 20].copy_from_slice(&rsdp);
        assert_eq!(find_rsdp(&region), None);
    }

    #[test]
    fn find_rsdp_is_none_for_an_empty_or_signatureless_region() {
        assert_eq!(find_rsdp(&[]), None);
        let region = vec![0xA5u8; 4096];
        assert_eq!(find_rsdp(&region), None);
    }

    #[test]
    fn rsdp_v1_round_trip() {
        let buf = build_rsdp_v1(0, 0x1234_5678);
        let r = Rsdp::validate(&buf).unwrap();
        assert_eq!(r.revision, 0);
        assert_eq!(r.rsdt_address, 0x1234_5678);
        assert_eq!(r.xsdt_address, 0);
    }

    #[test]
    fn rsdp_v2_round_trip() {
        let buf = build_rsdp_v2(0xAAAA_BBBB, 0xDEAD_BEEF_CAFE_F00D);
        let r = Rsdp::validate(&buf).unwrap();
        assert_eq!(r.revision, 2);
        assert_eq!(r.rsdt_address, 0xAAAA_BBBB);
        assert_eq!(r.xsdt_address, 0xDEAD_BEEF_CAFE_F00D);
    }

    #[test]
    fn rsdp_rejects_bad_signature() {
        let mut buf = build_rsdp_v1(0, 0);
        buf[0] = b'X';
        assert_eq!(Rsdp::validate(&buf).err(), Some(AcpiError::BadSignature));
    }

    #[test]
    fn rsdp_rejects_bad_checksum() {
        let mut buf = build_rsdp_v1(0, 0);
        buf[19] = buf[19].wrapping_add(1);
        assert_eq!(Rsdp::validate(&buf).err(), Some(AcpiError::BadChecksum));
    }

    #[test]
    fn rsdp_rejects_truncated() {
        assert_eq!(Rsdp::validate(&[0u8; 19]).err(), Some(AcpiError::Truncated));
    }

    #[test]
    fn rsdp_rejects_unsupported_revision() {
        let mut buf = [0u8; 20];
        buf[..8].copy_from_slice(&RSDP_SIGNATURE);
        buf[15] = 1; // ACPI rev 1 \u2014 spec-undefined; we refuse.
        let s = buf.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        buf[8] = 0u8.wrapping_sub(s);
        assert_eq!(
            Rsdp::validate(&buf).err(),
            Some(AcpiError::UnsupportedRevision),
        );
    }

    // Helper: build a MADT with the supplied entries appended.
    //
    // `pub(crate)` so the `platform` module's discovery tests drive the
    // same MADT builder rather than re-rolling the table layout.
    pub(crate) fn build_madt(lapic: u32, flags: u32, entries: &[u8]) -> Vec<u8> {
        let total = 44 + entries.len();
        let mut buf = vec![0u8; total];
        buf[..4].copy_from_slice(&MADT_SIGNATURE);
        let total_u32 = u32::try_from(total).expect("test MADT fits u32");
        buf[4..8].copy_from_slice(&total_u32.to_le_bytes());
        buf[8] = 4; // revision
                    // 9 = header checksum, fix below
                    // 10..16 OEMID, 16..24 OEM table id, 24..28 OEM rev,
                    // 28..32 creator id, 32..36 creator rev \u2014 all zero is fine.
        buf[36..40].copy_from_slice(&lapic.to_le_bytes());
        buf[40..44].copy_from_slice(&flags.to_le_bytes());
        buf[44..].copy_from_slice(entries);
        let s = buf.iter().fold(0u8, |a, b| a.wrapping_add(*b));
        buf[9] = 0u8.wrapping_sub(s);
        buf
    }

    #[test]
    fn madt_parses_lapic_and_ioapic() {
        // LocalApic entry: type=0, len=8, uid=0, apic_id=0, flags=1.
        let lapic_entry = [0u8, 8, 0, 0, 1, 0, 0, 0];
        // IoApic entry: type=1, len=12, id=2, reserved=0,
        // addr=0xFEC00000, gsi_base=0.
        let ioapic_entry = [1u8, 12, 2, 0, 0x00, 0x00, 0xC0, 0xFE, 0, 0, 0, 0];
        let mut entries = Vec::new();
        entries.extend_from_slice(&lapic_entry);
        entries.extend_from_slice(&ioapic_entry);

        let bytes = build_madt(0xFEE0_0000, 0x1, &entries);
        let madt = Madt::parse(&bytes).unwrap();
        assert_eq!(madt.lapic_address, 0xFEE0_0000);
        assert!(madt.flags.pcat_compat());

        let collected: Vec<_> = madt.entries().collect();
        assert_eq!(collected.len(), 2);
        assert!(matches!(
            collected[0],
            MadtEntry::LocalApic {
                processor_uid: 0,
                apic_id: 0,
                flags: 1
            }
        ));
        assert!(matches!(
            collected[1],
            MadtEntry::IoApic {
                id: 2,
                address: 0xFEC0_0000,
                gsi_base: 0
            }
        ));
    }

    #[test]
    fn madt_recognises_address_override() {
        let entry = [
            5u8, 12, 0, 0, // type, len, reserved x2
            0x00, 0x00, 0x00, 0xFE, 0xFF, 0xFF, 0xFF, 0xFF,
        ];
        let bytes = build_madt(0xFEE0_0000, 0, &entry);
        let madt = Madt::parse(&bytes).unwrap();
        let entries: Vec<_> = madt.entries().collect();
        assert_eq!(
            entries[0],
            MadtEntry::LocalApicAddressOverride {
                address: 0xFFFF_FFFF_FE00_0000,
            }
        );
    }

    #[test]
    fn madt_unknown_entry_type_becomes_other() {
        let entry = [99u8, 4, 0, 0];
        let bytes = build_madt(0, 0, &entry);
        let madt = Madt::parse(&bytes).unwrap();
        assert_eq!(madt.entries().next(), Some(MadtEntry::Other(99)));
    }

    #[test]
    fn madt_rejects_signature_mismatch() {
        let mut bytes = build_madt(0, 0, &[]);
        bytes[0] = b'X';
        assert_eq!(Madt::parse(&bytes).err(), Some(AcpiError::BadSignature));
    }

    #[test]
    fn madt_rejects_bad_checksum() {
        let mut bytes = build_madt(0, 0, &[]);
        bytes[36] ^= 0xFF;
        assert_eq!(Madt::parse(&bytes).err(), Some(AcpiError::BadChecksum));
    }

    #[test]
    fn madt_rejects_malformed_entry() {
        // Entry len=1 is below the 2-byte minimum.
        let entry = [0u8, 1];
        let bytes = build_madt(0, 0, &entry);
        let madt = Madt::parse(&bytes).unwrap();
        // Iterator yields None immediately when it sees the bad entry.
        assert!(madt.entries().next().is_none());
    }

    #[test]
    fn interrupt_source_override_decodes() {
        let entry = [
            2u8, 10, 0, 1, // type, len, bus=0, source=1
            9, 0, 0, 0, // gsi=9
            0b1101, 0, // flags: active high, level
        ];
        let bytes = build_madt(0, 0, &entry);
        let madt = Madt::parse(&bytes).unwrap();
        assert_eq!(
            madt.entries().next(),
            Some(MadtEntry::InterruptSourceOverride {
                bus: 0,
                source: 1,
                gsi: 9,
                flags: 0b1101,
            }),
        );
    }

    // Helper: build an MCFG with one ECAM allocation and a valid checksum.
    fn build_mcfg(base: u64, segment: u16, start_bus: u8, end_bus: u8) -> Vec<u8> {
        // SDT header (36) + 8 reserved + one 16-byte allocation.
        let total = MCFG_ALLOCATIONS_OFFSET + MCFG_ALLOCATION_LEN;
        let mut buf = vec![0u8; total];
        buf[..4].copy_from_slice(&MCFG_SIGNATURE);
        let total_u32 = u32::try_from(total).expect("test MCFG fits u32");
        buf[4..8].copy_from_slice(&total_u32.to_le_bytes());
        buf[8] = 1; // revision
                    // byte 9 = checksum, fixed below.
        let a = MCFG_ALLOCATIONS_OFFSET;
        buf[a..a + 8].copy_from_slice(&base.to_le_bytes());
        buf[a + 8..a + 10].copy_from_slice(&segment.to_le_bytes());
        buf[a + 10] = start_bus;
        buf[a + 11] = end_bus;
        let s = buf.iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
        buf[9] = 0u8.wrapping_sub(s);
        buf
    }

    #[test]
    fn mcfg_first_ecam_decodes_the_allocation() {
        // The QEMU q35 ECAM base, one segment, all 256 buses.
        let bytes = build_mcfg(0xB000_0000, 0, 0, 0xFF);
        let ecam = mcfg_first_ecam(&bytes).expect("ecam allocation");
        assert_eq!(
            ecam,
            EcamAllocation {
                base: 0xB000_0000,
                segment: 0,
                start_bus: 0,
                end_bus: 0xFF,
            }
        );
        // 256 buses × 1 MiB.
        assert_eq!(ecam.window_len(), 256 << 20);
    }

    #[test]
    fn mcfg_first_ecam_window_len_for_a_single_bus() {
        let bytes = build_mcfg(0xC000_0000, 0, 0, 0);
        let ecam = mcfg_first_ecam(&bytes).expect("ecam allocation");
        assert_eq!(ecam.window_len(), 1 << 20);
    }

    #[test]
    fn mcfg_first_ecam_rejects_a_bad_signature() {
        let mut bytes = build_mcfg(0xB000_0000, 0, 0, 0xFF);
        bytes[0] = b'X';
        assert_eq!(mcfg_first_ecam(&bytes), None);
    }

    #[test]
    fn mcfg_first_ecam_rejects_a_table_with_no_allocation() {
        // A well-formed MCFG header + reserved area but zero allocations
        // (length stops at the reserved area) carries no ECAM window.
        let total = MCFG_ALLOCATIONS_OFFSET;
        let mut buf = vec![0u8; total];
        buf[..4].copy_from_slice(&MCFG_SIGNATURE);
        buf[4..8].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
        buf[8] = 1;
        let s = buf.iter().fold(0u8, |acc, b| acc.wrapping_add(*b));
        buf[9] = 0u8.wrapping_sub(s);
        assert_eq!(mcfg_first_ecam(&buf), None);
    }

    #[test]
    fn mcfg_first_ecam_rejects_an_inverted_bus_range() {
        let bytes = build_mcfg(0xB000_0000, 0, 0x10, 0x00);
        assert_eq!(mcfg_first_ecam(&bytes), None);
    }
}
