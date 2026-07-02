//! Xen HVM (PVH) direct-boot `hvm_start_info` parser.
//!
//! QEMU's `-kernel` ELF loader honours the `XEN_ELFNOTE_PHYS32_ENTRY`
//! note (`boot.s`) and enters `pvh_start` with `%ebx` pointing at an
//! `hvm_start_info` record (Xen ABI, xen.git
//! `xen/include/public/arch-x86/hvm/start_info.h`). Version 1 of that
//! record carries the two facts boot needs: the physical address of the
//! ACPI RSDP and an E820-style memory map.
//!
//! Like [`crate::multiboot2`], this module is a pure, bounds-checked
//! decoder over byte slices: it performs no raw memory access itself
//! (the consumer builds the slices from the identity-mapped window) and
//! every structural defect is a typed error — fail closed, no panic.

/// `hvm_start_info.magic`: `"xEn3"` little-endian.
///
/// `boot.s` loads this value into the trampoline's protocol register so
/// `entry.rs` can tell a PVH entry from a multiboot2 one, and the parser
/// re-validates it against the record's own leading field.
pub const PVH_BOOT_MAGIC: u32 = 0x336E_C578;

/// Byte length of a version-1 `hvm_start_info` record.
///
/// Layout (all fields little-endian):
///
/// | offset | field            |
/// |--------|------------------|
/// | 0      | `magic: u32`     |
/// | 4      | `version: u32`   |
/// | 8      | `flags: u32`     |
/// | 12     | `nr_modules: u32`|
/// | 16     | `modlist_paddr: u64` |
/// | 24     | `cmdline_paddr: u64` |
/// | 32     | `rsdp_paddr: u64`    |
/// | 40     | `memmap_paddr: u64`  |
/// | 48     | `memmap_entries: u32`|
/// | 52     | `reserved: u32`      |
pub const START_INFO_V1_LEN: usize = 56;

/// Byte length of one `hvm_memmap_table_entry`
/// (`addr: u64`, `size: u64`, `type: u32`, `reserved: u32`).
pub const MEMMAP_ENTRY_LEN: usize = 24;

/// Why a start-info record or memory-map table was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// The supplied slice is shorter than the structure it must hold.
    Truncated,
    /// The leading `magic` field is not [`PVH_BOOT_MAGIC`].
    BadMagic,
    /// `version` is 0: pre-v1 records carry no RSDP/memory-map fields,
    /// so nothing useful can be decoded from them.
    UnsupportedVersion,
    /// The stated entry count is zero or its table would be empty —
    /// a boot without a usable memory map cannot proceed.
    EmptyMemoryMap,
}

/// Decoded, validated version-1 `hvm_start_info` fields the kernel
/// consumes. Module list and command line are intentionally not
/// surfaced: no consumer exists today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartInfo {
    /// Record version (≥ 1 by construction).
    pub version: u32,
    /// Physical address of the ACPI RSDP, or 0 when the loader did not
    /// provide one.
    pub rsdp_paddr: u64,
    /// Physical address of the `hvm_memmap_table_entry` array.
    pub memmap_paddr: u64,
    /// Number of entries in that array.
    pub memmap_entries: u32,
}

impl StartInfo {
    /// Validate and decode a version-1 `hvm_start_info` record.
    ///
    /// `bytes` must be at least [`START_INFO_V1_LEN`] long. The memory
    /// map is required (`memmap_paddr != 0`, `memmap_entries > 0`): a
    /// PVH boot without one cannot size RAM and is refused.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] on any structural defect — fail closed.
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseError> {
        if bytes.len() < START_INFO_V1_LEN {
            return Err(ParseError::Truncated);
        }
        if read_u32(bytes, 0) != PVH_BOOT_MAGIC {
            return Err(ParseError::BadMagic);
        }
        let version = read_u32(bytes, 4);
        if version < 1 {
            return Err(ParseError::UnsupportedVersion);
        }
        let rsdp_paddr = read_u64(bytes, 32);
        let memmap_paddr = read_u64(bytes, 40);
        let memmap_entries = read_u32(bytes, 48);
        if memmap_paddr == 0 || memmap_entries == 0 {
            return Err(ParseError::EmptyMemoryMap);
        }
        Ok(Self {
            version,
            rsdp_paddr,
            memmap_paddr,
            memmap_entries,
        })
    }

    /// Total byte length of the memory-map table this record describes,
    /// or `None` if the multiply overflows (a hostile count).
    #[must_use]
    pub fn memmap_len_bytes(&self) -> Option<usize> {
        (self.memmap_entries as usize).checked_mul(MEMMAP_ENTRY_LEN)
    }
}

/// `hvm_memmap_table_entry.type` values (Xen ABI `XEN_HVM_MEMMAP_TYPE_*`,
/// which mirror the E820 vocabulary).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PvhMemoryKind {
    /// Type 1: usable RAM.
    Ram,
    /// Type 2: firmware-reserved.
    Reserved,
    /// Type 3: ACPI tables, reclaimable once parsed.
    AcpiReclaimable,
    /// Type 4: ACPI non-volatile storage.
    AcpiNvs,
    /// Type 5: RAM the firmware found defective.
    Unusable,
    /// Any other (including future) type — treated as untouchable.
    Other(u32),
}

impl PvhMemoryKind {
    fn from_raw(v: u32) -> Self {
        match v {
            1 => Self::Ram,
            2 => Self::Reserved,
            3 => Self::AcpiReclaimable,
            4 => Self::AcpiNvs,
            5 => Self::Unusable,
            other => Self::Other(other),
        }
    }
}

/// One decoded memory-map entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PvhMemoryEntry {
    /// Physical start address.
    pub addr: u64,
    /// Length in bytes.
    pub size: u64,
    /// What the region holds.
    pub kind: PvhMemoryKind,
}

/// A validated view over the `hvm_memmap_table_entry` array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryMap<'a> {
    bytes: &'a [u8],
    entries: usize,
}

impl<'a> MemoryMap<'a> {
    /// Validate the memory-map table bytes against the entry count the
    /// start-info record stated.
    ///
    /// # Errors
    ///
    /// * [`ParseError::EmptyMemoryMap`] when `entries` is zero.
    /// * [`ParseError::Truncated`] when `bytes` cannot hold `entries`
    ///   records.
    pub fn parse(bytes: &'a [u8], entries: u32) -> Result<Self, ParseError> {
        let entries = entries as usize;
        if entries == 0 {
            return Err(ParseError::EmptyMemoryMap);
        }
        let need = entries
            .checked_mul(MEMMAP_ENTRY_LEN)
            .ok_or(ParseError::Truncated)?;
        if bytes.len() < need {
            return Err(ParseError::Truncated);
        }
        Ok(Self {
            bytes: &bytes[..need],
            entries,
        })
    }

    /// Iterate the decoded entries.
    #[must_use]
    pub fn entries(&self) -> PvhMemoryEntryIter<'a> {
        PvhMemoryEntryIter {
            bytes: self.bytes,
            remaining: self.entries,
        }
    }
}

/// Iterator over [`PvhMemoryEntry`]s; see [`MemoryMap::entries`].
pub struct PvhMemoryEntryIter<'a> {
    bytes: &'a [u8],
    remaining: usize,
}

impl Iterator for PvhMemoryEntryIter<'_> {
    type Item = PvhMemoryEntry;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 || self.bytes.len() < MEMMAP_ENTRY_LEN {
            return None;
        }
        let entry = PvhMemoryEntry {
            addr: read_u64(self.bytes, 0),
            size: read_u64(self.bytes, 8),
            kind: PvhMemoryKind::from_raw(read_u32(self.bytes, 16)),
        };
        self.bytes = &self.bytes[MEMMAP_ENTRY_LEN..];
        self.remaining -= 1;
        Some(entry)
    }
}

/// Read a little-endian `u32` at `off`. Callers guarantee bounds; the
/// slice indexing still bounds-checks (a violation is a logic bug the
/// tests catch, never memory unsafety).
fn read_u32(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

/// Read a little-endian `u64` at `off`; same bounds contract as
/// [`read_u32`].
fn read_u64(bytes: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        bytes[off],
        bytes[off + 1],
        bytes[off + 2],
        bytes[off + 3],
        bytes[off + 4],
        bytes[off + 5],
        bytes[off + 6],
        bytes[off + 7],
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u32(buf: &mut [u8], off: usize, v: u32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn put_u64(buf: &mut [u8], off: usize, v: u64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// A well-formed v1 start-info record.
    fn v1_record() -> [u8; START_INFO_V1_LEN] {
        let mut b = [0u8; START_INFO_V1_LEN];
        put_u32(&mut b, 0, PVH_BOOT_MAGIC);
        put_u32(&mut b, 4, 1); // version
        put_u64(&mut b, 32, 0x000E_0000); // rsdp_paddr
        put_u64(&mut b, 40, 0x0009_E000); // memmap_paddr
        put_u32(&mut b, 48, 2); // memmap_entries
        b
    }

    #[test]
    fn parse_accepts_v1_record() {
        let si = StartInfo::parse(&v1_record()).expect("v1 record must parse");
        assert_eq!(si.version, 1);
        assert_eq!(si.rsdp_paddr, 0x000E_0000);
        assert_eq!(si.memmap_paddr, 0x0009_E000);
        assert_eq!(si.memmap_entries, 2);
        assert_eq!(si.memmap_len_bytes(), Some(2 * MEMMAP_ENTRY_LEN));
    }

    #[test]
    fn parse_rejects_truncated_record() {
        let b = v1_record();
        assert_eq!(
            StartInfo::parse(&b[..START_INFO_V1_LEN - 1]),
            Err(ParseError::Truncated)
        );
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let mut b = v1_record();
        put_u32(&mut b, 0, 0xDEAD_BEEF);
        assert_eq!(StartInfo::parse(&b), Err(ParseError::BadMagic));
    }

    #[test]
    fn parse_rejects_version_zero() {
        let mut b = v1_record();
        put_u32(&mut b, 4, 0);
        assert_eq!(StartInfo::parse(&b), Err(ParseError::UnsupportedVersion));
    }

    #[test]
    fn parse_rejects_missing_memory_map() {
        let mut no_paddr = v1_record();
        put_u64(&mut no_paddr, 40, 0);
        assert_eq!(StartInfo::parse(&no_paddr), Err(ParseError::EmptyMemoryMap));

        let mut no_entries = v1_record();
        put_u32(&mut no_entries, 48, 0);
        assert_eq!(
            StartInfo::parse(&no_entries),
            Err(ParseError::EmptyMemoryMap)
        );
    }

    #[test]
    fn parse_accepts_absent_rsdp_as_zero() {
        // rsdp_paddr = 0 is "not provided" — the record still parses;
        // the consumer decides whether it can proceed without ACPI.
        let mut b = v1_record();
        put_u64(&mut b, 32, 0);
        let si = StartInfo::parse(&b).expect("record without RSDP must parse");
        assert_eq!(si.rsdp_paddr, 0);
    }

    #[test]
    fn memory_map_decodes_entries() {
        let mut b = [0u8; 2 * MEMMAP_ENTRY_LEN];
        put_u64(&mut b, 0, 0);
        put_u64(&mut b, 8, 0x9_F000);
        put_u32(&mut b, 16, 1); // RAM
        put_u64(&mut b, 24, 0x10_0000);
        put_u64(&mut b, 32, 0xF00_0000);
        put_u32(&mut b, 40, 3); // ACPI reclaimable

        let map = MemoryMap::parse(&b, 2).expect("table must parse");
        let mut it = map.entries();
        assert_eq!(
            it.next(),
            Some(PvhMemoryEntry {
                addr: 0,
                size: 0x9_F000,
                kind: PvhMemoryKind::Ram,
            })
        );
        assert_eq!(
            it.next(),
            Some(PvhMemoryEntry {
                addr: 0x10_0000,
                size: 0xF00_0000,
                kind: PvhMemoryKind::AcpiReclaimable,
            })
        );
        assert_eq!(it.next(), None);
    }

    #[test]
    fn memory_map_rejects_truncated_table() {
        let b = [0u8; 2 * MEMMAP_ENTRY_LEN - 1];
        assert_eq!(MemoryMap::parse(&b, 2), Err(ParseError::Truncated));
        assert_eq!(MemoryMap::parse(&b, 0), Err(ParseError::EmptyMemoryMap));
    }

    #[test]
    fn memory_map_ignores_bytes_past_stated_count() {
        // A table longer than the stated count yields exactly `entries`.
        let mut b = [0u8; 3 * MEMMAP_ENTRY_LEN];
        put_u32(&mut b, 16, 1);
        put_u32(&mut b, 16 + MEMMAP_ENTRY_LEN, 2);
        put_u32(&mut b, 16 + 2 * MEMMAP_ENTRY_LEN, 1);
        let map = MemoryMap::parse(&b, 1).expect("table must parse");
        assert_eq!(map.entries().count(), 1);
    }

    #[test]
    fn memory_kinds_decode_the_full_vocabulary() {
        let cases = [
            (1, PvhMemoryKind::Ram),
            (2, PvhMemoryKind::Reserved),
            (3, PvhMemoryKind::AcpiReclaimable),
            (4, PvhMemoryKind::AcpiNvs),
            (5, PvhMemoryKind::Unusable),
            (7, PvhMemoryKind::Other(7)),
        ];
        for (raw, want) in cases {
            let mut b = [0u8; MEMMAP_ENTRY_LEN];
            put_u32(&mut b, 16, raw);
            let map = MemoryMap::parse(&b, 1).expect("table must parse");
            let entry = map.entries().next().expect("one entry");
            assert_eq!(entry.kind, want, "type {raw}");
        }
    }
}
