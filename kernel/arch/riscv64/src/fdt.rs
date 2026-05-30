//! Minimal flattened-device-tree (FDT / DTB) reader.
//!
//! `qemu-system-riscv64 -M virt` hands the kernel a pointer to a
//! flattened device tree in `a1` (the OpenSBI → S-mode hand-off). This
//! module reads exactly the two facts the boot pipeline needs from it
//! and nothing more (`AGENTS.md` §2.3 — no bloat):
//!
//! * the physical base/size of the first `/memory` node, which the
//!   downstream boot consumer uses to build the firmware
//!   `BootMemoryMap`;
//! * the `/cpus` `timebase-frequency`, the divisor that converts the
//!   `time` CSR ticks into nanoseconds for the monotonic clock.
//!
//! The parser is `no_std`, allocation-free, and bounds-checks every
//! read against the blob length, returning [`FdtError`] rather than
//! panicking (`AGENTS.md` §2.9). It is host-testable: [`Fdt::new`]
//! accepts a borrowed blob so the unit tests below drive it against a
//! hand-built fixture without a riscv64 target.
//!
//! The format is the Devicetree Specification v0.4 flattened layout:
//! a big-endian header, a structure block of `FDT_*` tokens, and a
//! strings block. Only the subset needed for the two queries above is
//! interpreted.

/// FDT header magic (`0xd00dfeed`, big-endian on the wire).
const FDT_MAGIC: u32 = 0xd00d_feed;

/// `FDT_BEGIN_NODE` — opens a node; followed by its NUL-terminated
/// name, padded to a 4-byte boundary.
const FDT_BEGIN_NODE: u32 = 0x0000_0001;
/// `FDT_END_NODE` — closes the most recently opened node.
const FDT_END_NODE: u32 = 0x0000_0002;
/// `FDT_PROP` — a property: `len: u32`, `nameoff: u32`, then `len`
/// value bytes padded to a 4-byte boundary.
const FDT_PROP: u32 = 0x0000_0003;
/// `FDT_NOP` — padding token, ignored.
const FDT_NOP: u32 = 0x0000_0004;
/// `FDT_END` — terminates the structure block.
const FDT_END: u32 = 0x0000_0009;

/// Devicetree default `#address-cells` when the root omits it.
const DEFAULT_ADDRESS_CELLS: u32 = 2;
/// Devicetree default `#size-cells` when the root omits it.
const DEFAULT_SIZE_CELLS: u32 = 1;

/// Reasons the FDT reader rejected a blob.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum FdtError {
    /// The header magic did not match the FDT magic (`0xd00dfeed`).
    BadMagic,
    /// The blob is shorter than the fixed 40-byte header.
    TooShort,
    /// A header offset/size pointed outside the blob.
    OutOfBounds,
    /// The structure block was malformed (truncated token, unterminated
    /// name, unknown token, or unbalanced node nesting).
    Malformed,
}

/// A read-only view over a flattened device tree blob.
pub struct Fdt<'a> {
    blob: &'a [u8],
    struct_off: usize,
    struct_size: usize,
    strings_off: usize,
    strings_size: usize,
}

/// Maximum node-nesting depth the walker tracks. QEMU's `virt` tree is
/// shallow (root → bus → device); 32 is generous headroom and bounds
/// the parser's stack usage (`AGENTS.md` §2.9 — no unbounded recursion).
const MAX_DEPTH: usize = 32;

/// Read a big-endian `u32` at byte offset `off` in `bytes`.
fn be_u32(bytes: &[u8], off: usize) -> Option<u32> {
    let end = off.checked_add(4)?;
    let slice = bytes.get(off..end)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

impl<'a> Fdt<'a> {
    /// Validate the header and build a reader over `blob`.
    ///
    /// # Errors
    ///
    /// Returns [`FdtError`] if the magic is wrong, the blob is shorter
    /// than the 40-byte header, or a header offset/size escapes the
    /// blob.
    pub fn new(blob: &'a [u8]) -> Result<Self, FdtError> {
        if blob.len() < 40 {
            return Err(FdtError::TooShort);
        }
        if be_u32(blob, 0) != Some(FDT_MAGIC) {
            return Err(FdtError::BadMagic);
        }
        let struct_off = be_u32(blob, 8).ok_or(FdtError::TooShort)? as usize;
        let strings_off = be_u32(blob, 12).ok_or(FdtError::TooShort)? as usize;
        let strings_size = be_u32(blob, 32).ok_or(FdtError::TooShort)? as usize;
        let struct_size = be_u32(blob, 36).ok_or(FdtError::TooShort)? as usize;

        let struct_end = struct_off
            .checked_add(struct_size)
            .ok_or(FdtError::OutOfBounds)?;
        let strings_end = strings_off
            .checked_add(strings_size)
            .ok_or(FdtError::OutOfBounds)?;
        if struct_end > blob.len() || strings_end > blob.len() {
            return Err(FdtError::OutOfBounds);
        }
        // The structure block is a sequence of 4-byte tokens.
        if struct_off % 4 != 0 || struct_size % 4 != 0 {
            return Err(FdtError::Malformed);
        }
        Ok(Self {
            blob,
            struct_off,
            struct_size,
            strings_off,
            strings_size,
        })
    }

    /// Build a reader from a raw pointer to a blob in memory.
    ///
    /// Reads the `totalsize` header field to bound the blob, then
    /// delegates to [`Fdt::new`].
    ///
    /// # Safety
    ///
    /// `ptr` must point at the first byte of a flattened device tree
    /// whose `totalsize` header field truthfully describes its length,
    /// and the whole `totalsize` range must be readable for the
    /// lifetime `'a`. On the `virt` board this is the `a1` pointer
    /// OpenSBI hands the kernel, which lives in firmware-reserved RAM
    /// for the life of the guest.
    ///
    /// # Errors
    ///
    /// Propagates [`Fdt::new`]'s validation errors.
    pub unsafe fn from_ptr(ptr: *const u8) -> Result<Self, FdtError> {
        // Read the 8-byte prefix (magic + totalsize) to learn the
        // length before forming the full slice.
        // SAFETY: the caller guarantees `ptr` addresses a valid FDT;
        // the first 8 bytes (magic + totalsize) are always present in a
        // well-formed blob.
        let header = unsafe { core::slice::from_raw_parts(ptr, 8) };
        if be_u32(header, 0) != Some(FDT_MAGIC) {
            return Err(FdtError::BadMagic);
        }
        let total = be_u32(header, 4).ok_or(FdtError::TooShort)? as usize;
        if total < 40 {
            return Err(FdtError::TooShort);
        }
        // SAFETY: `total` is the blob's self-described length; the
        // caller guarantees the whole range is readable.
        let blob = unsafe { core::slice::from_raw_parts(ptr, total) };
        Self::new(blob)
    }

    /// Read the NUL-terminated string at `nameoff` in the strings block.
    fn string_at(&self, nameoff: usize) -> Option<&'a [u8]> {
        let start = self.strings_off.checked_add(nameoff)?;
        if nameoff >= self.strings_size {
            return None;
        }
        let block_end = self.strings_off + self.strings_size;
        let region = self.blob.get(start..block_end)?;
        let len = region.iter().position(|&b| b == 0)?;
        Some(&region[..len])
    }

    /// Locate the first `/memory` node's first `reg` entry, returning
    /// `(base, size)` in bytes.
    ///
    /// Returns `None` if the tree contains no `/memory` node with a
    /// readable `reg` property.
    #[must_use]
    pub fn first_memory_region(&self) -> Option<(u64, u64)> {
        self.walk().ok().and_then(|w| w.memory)
    }

    /// Read the `timebase-frequency` (the `time` CSR tick rate in Hz).
    ///
    /// Returns the first occurrence found while walking the tree, or
    /// `None` if the property is absent.
    #[must_use]
    pub fn timebase_frequency(&self) -> Option<u64> {
        self.walk().ok().and_then(|w| w.timebase)
    }

    /// Single pass over the structure block collecting both queries.
    fn walk(&self) -> Result<WalkResult, FdtError> {
        let struct_end = self.struct_off + self.struct_size;
        let mut pos = self.struct_off;

        // Root `#address-cells` / `#size-cells` govern the `/memory`
        // `reg` layout; default to the Devicetree-spec values until the
        // root node overrides them.
        let mut addr_cells = DEFAULT_ADDRESS_CELLS;
        let mut size_cells = DEFAULT_SIZE_CELLS;

        // Per-depth "is this a `/memory` node" flags, restored on
        // `FDT_END_NODE`.
        let mut is_memory = [false; MAX_DEPTH];
        let mut depth: usize = 0;

        let mut result = WalkResult {
            memory: None,
            timebase: None,
        };

        while pos < struct_end {
            let token = be_u32(self.blob, pos).ok_or(FdtError::Malformed)?;
            pos += 4;
            match token {
                FDT_NOP => {}
                FDT_END => break,
                FDT_BEGIN_NODE => {
                    let name = self.read_node_name(&mut pos, struct_end)?;
                    if depth >= MAX_DEPTH {
                        return Err(FdtError::Malformed);
                    }
                    // A `/memory` node lives directly under root
                    // (depth 1 after this push); its unit name is
                    // `memory` or `memory@<addr>`.
                    is_memory[depth] = depth == 1 && name_is_memory(name);
                    depth += 1;
                }
                FDT_END_NODE => {
                    depth = depth.checked_sub(1).ok_or(FdtError::Malformed)?;
                }
                FDT_PROP => {
                    let (prop_name, value) = self.read_prop(&mut pos, struct_end)?;
                    // Root props (depth 1) carry the cell counts.
                    if depth == 1 {
                        if prop_name == b"#address-cells" {
                            if let Some(v) = be_u32(value, 0) {
                                addr_cells = v;
                            }
                        } else if prop_name == b"#size-cells" {
                            if let Some(v) = be_u32(value, 0) {
                                size_cells = v;
                            }
                        }
                    }
                    if result.timebase.is_none() && prop_name == b"timebase-frequency" {
                        result.timebase = read_int_cells(value);
                    }
                    if result.memory.is_none()
                        && prop_name == b"reg"
                        && depth >= 1
                        && is_memory[depth - 1]
                    {
                        result.memory = read_reg_pair(value, addr_cells, size_cells);
                    }
                }
                _ => return Err(FdtError::Malformed),
            }
        }
        Ok(result)
    }

    /// Read a NUL-terminated node name at `*pos`, advancing `*pos` past
    /// the 4-byte-aligned end of the name.
    fn read_node_name(&self, pos: &mut usize, struct_end: usize) -> Result<&'a [u8], FdtError> {
        let region = self.blob.get(*pos..struct_end).ok_or(FdtError::Malformed)?;
        let len = region
            .iter()
            .position(|&b| b == 0)
            .ok_or(FdtError::Malformed)?;
        let name = &region[..len];
        // Advance past the name + NUL, rounded up to a 4-byte boundary.
        let consumed = align_up(len + 1, 4);
        *pos = pos.checked_add(consumed).ok_or(FdtError::Malformed)?;
        if *pos > struct_end {
            return Err(FdtError::Malformed);
        }
        Ok(name)
    }

    /// Read an `FDT_PROP` body at `*pos` (`len`, `nameoff`, then the
    /// padded value), advancing `*pos` past it. Returns the property
    /// name and value slice.
    fn read_prop(
        &self,
        pos: &mut usize,
        struct_end: usize,
    ) -> Result<(&'a [u8], &'a [u8]), FdtError> {
        let len = be_u32(self.blob, *pos).ok_or(FdtError::Malformed)? as usize;
        let nameoff = be_u32(self.blob, *pos + 4).ok_or(FdtError::Malformed)? as usize;
        let value_start = pos.checked_add(8).ok_or(FdtError::Malformed)?;
        let value_end = value_start.checked_add(len).ok_or(FdtError::Malformed)?;
        if value_end > struct_end {
            return Err(FdtError::Malformed);
        }
        let value = &self.blob[value_start..value_end];
        let name = self.string_at(nameoff).ok_or(FdtError::Malformed)?;
        *pos = align_up(value_end, 4);
        Ok((name, value))
    }
}

/// Collected results of one [`Fdt::walk`] pass.
struct WalkResult {
    memory: Option<(u64, u64)>,
    timebase: Option<u64>,
}

/// Round `value` up to the next multiple of `align` (a power of two).
fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

/// `true` if `name` is the unit name of a `/memory` node (`memory` or
/// `memory@<addr>`).
fn name_is_memory(name: &[u8]) -> bool {
    name == b"memory" || name.starts_with(b"memory@")
}

/// Decode `cells` big-endian `u32` cells starting at `off` into a
/// `u64`. Returns `None` if the slice is too short.
fn read_cells(value: &[u8], off: usize, cells: u32) -> Option<u64> {
    let mut acc: u64 = 0;
    let mut o = off;
    for _ in 0..cells {
        let cell = be_u32(value, o)?;
        acc = (acc << 32) | u64::from(cell);
        o += 4;
    }
    Some(acc)
}

/// Read the first `(address, size)` pair from a `reg` property value.
fn read_reg_pair(value: &[u8], addr_cells: u32, size_cells: u32) -> Option<(u64, u64)> {
    if addr_cells == 0 || addr_cells > 2 || size_cells == 0 || size_cells > 2 {
        return None;
    }
    let base = read_cells(value, 0, addr_cells)?;
    let size = read_cells(value, (addr_cells as usize) * 4, size_cells)?;
    Some((base, size))
}

/// Read an integer property whose value is one `u32` cell or two
/// (`u64`).
fn read_int_cells(value: &[u8]) -> Option<u64> {
    match value.len() {
        4 => be_u32(value, 0).map(u64::from),
        8 => read_cells(value, 0, 2),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    /// Builder for a minimal flattened device tree, used to drive the
    /// parser without a riscv64 target. Mirrors the on-disk layout
    /// QEMU's `virt` board produces closely enough to exercise every
    /// branch the boot pipeline relies on.
    struct DtbBuilder {
        strings: Vec<u8>,
        structure: Vec<u8>,
    }

    impl DtbBuilder {
        fn new() -> Self {
            Self {
                strings: Vec::new(),
                structure: Vec::new(),
            }
        }

        /// Intern `name` into the strings block, returning its offset.
        fn intern(&mut self, name: &str) -> u32 {
            let off = u32::try_from(self.strings.len()).expect("offset fits u32");
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
            off
        }

        fn token(&mut self, tok: u32) {
            self.structure.extend_from_slice(&tok.to_be_bytes());
        }

        fn pad4(&mut self) {
            while self.structure.len() % 4 != 0 {
                self.structure.push(0);
            }
        }

        fn begin_node(&mut self, name: &str) {
            self.token(FDT_BEGIN_NODE);
            self.structure.extend_from_slice(name.as_bytes());
            self.structure.push(0);
            self.pad4();
        }

        fn end_node(&mut self) {
            self.token(FDT_END_NODE);
        }

        fn prop(&mut self, name: &str, value: &[u8]) {
            let nameoff = self.intern(name);
            self.token(FDT_PROP);
            let len = u32::try_from(value.len()).expect("prop value fits u32");
            self.structure.extend_from_slice(&len.to_be_bytes());
            self.structure.extend_from_slice(&nameoff.to_be_bytes());
            self.structure.extend_from_slice(value);
            self.pad4();
        }

        fn prop_u32(&mut self, name: &str, v: u32) {
            self.prop(name, &v.to_be_bytes());
        }

        /// Finalise the blob: assemble the header, structure block (with
        /// a trailing `FDT_END`), and strings block.
        fn build(mut self) -> Vec<u8> {
            self.token(FDT_END);
            let header_len = 40usize;
            let struct_off = header_len;
            let struct_size = self.structure.len();
            let strings_off = struct_off + struct_size;
            let strings_size = self.strings.len();
            let total = strings_off + strings_size;

            let mut blob = Vec::with_capacity(total);
            let mut push = |v: u32| blob.extend_from_slice(&v.to_be_bytes());
            let u32_of = |v: usize| u32::try_from(v).expect("fits u32");
            push(FDT_MAGIC);
            push(u32_of(total));
            push(u32_of(struct_off));
            push(u32_of(strings_off));
            push(0); // off_mem_rsvmap (unused by this parser)
            push(17); // version
            push(16); // last_comp_version
            push(0); // boot_cpuid_phys
            push(u32_of(strings_size));
            push(u32_of(struct_size));
            blob.extend_from_slice(&self.structure);
            blob.extend_from_slice(&self.strings);
            blob
        }
    }

    /// A QEMU-`virt`-shaped tree: 2/2 root cells, a `/cpus` node with a
    /// `timebase-frequency`, and a `/memory@80000000` node.
    fn virt_like(base: u64, size: u64, timebase: u32) -> Vec<u8> {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("cpus");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 0);
        b.prop_u32("timebase-frequency", timebase);
        b.end_node();
        b.begin_node("memory@80000000");
        b.prop("device_type", b"memory\0");
        let mut reg = Vec::new();
        reg.extend_from_slice(&base.to_be_bytes());
        reg.extend_from_slice(&size.to_be_bytes());
        b.prop("reg", &reg);
        b.end_node();
        b.end_node();
        b.build()
    }

    #[test]
    fn rejects_bad_magic() {
        let mut blob = virt_like(0x8000_0000, 0x1000_0000, 10_000_000);
        blob[0] = 0;
        assert_eq!(Fdt::new(&blob).err(), Some(FdtError::BadMagic));
    }

    #[test]
    fn rejects_short_blob() {
        let blob = [0u8; 8];
        assert_eq!(Fdt::new(&blob).err(), Some(FdtError::TooShort));
    }

    #[test]
    fn reads_memory_region() {
        let blob = virt_like(0x8000_0000, 0x1000_0000, 10_000_000);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(fdt.first_memory_region(), Some((0x8000_0000, 0x1000_0000)));
    }

    #[test]
    fn reads_timebase_frequency() {
        let blob = virt_like(0x8000_0000, 0x1000_0000, 10_000_000);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(fdt.timebase_frequency(), Some(10_000_000));
    }

    #[test]
    fn memory_uses_root_cell_counts() {
        // A tree declaring 1/1 root cells must read 32-bit base+size.
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 1);
        b.begin_node("memory@80000000");
        let mut reg = Vec::new();
        reg.extend_from_slice(&0x8000_0000u32.to_be_bytes());
        reg.extend_from_slice(&0x0800_0000u32.to_be_bytes());
        b.prop("reg", &reg);
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(fdt.first_memory_region(), Some((0x8000_0000, 0x0800_0000)));
    }

    #[test]
    fn missing_memory_node_returns_none() {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("cpus");
        b.prop_u32("timebase-frequency", 10_000_000);
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(fdt.first_memory_region(), None);
        assert_eq!(fdt.timebase_frequency(), Some(10_000_000));
    }

    #[test]
    fn first_memory_node_wins_over_later_ones() {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("memory@80000000");
        let mut reg = Vec::new();
        reg.extend_from_slice(&0x8000_0000u64.to_be_bytes());
        reg.extend_from_slice(&0x1000_0000u64.to_be_bytes());
        b.prop("reg", &reg);
        b.end_node();
        b.begin_node("memory@90000000");
        let mut reg2 = Vec::new();
        reg2.extend_from_slice(&0x9000_0000u64.to_be_bytes());
        reg2.extend_from_slice(&0x2000_0000u64.to_be_bytes());
        b.prop("reg", &reg2);
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(fdt.first_memory_region(), Some((0x8000_0000, 0x1000_0000)));
    }

    #[test]
    fn from_ptr_matches_new() {
        let blob = virt_like(0x8000_0000, 0x1000_0000, 10_000_000);
        // SAFETY: `blob` is a valid FDT whose totalsize header equals
        // its length; the slice outlives the `Fdt` built from it.
        let fdt = unsafe { Fdt::from_ptr(blob.as_ptr()) }.expect("valid fdt");
        assert_eq!(fdt.first_memory_region(), Some((0x8000_0000, 0x1000_0000)));
    }
}
