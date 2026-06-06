//! Shared flattened-device-tree (FDT / DTB) reader (`AGENTS.md` §2.2 /
//! §18.2).
//!
//! A flattened device tree is the boot-time hardware description that
//! both the aarch64 and riscv64 platforms hand the kernel. The wire
//! format is identical across architectures, so the parser lives here
//! **once** and every architecture port builds its platform discovery on
//! it (`AGENTS.md` §2.2 — no duplication); the arch-specific *queries*
//! (riscv64 `timebase-frequency`, aarch64 PSCI method / timer PPI) layer
//! on top in each port's `fdt`/`platform` module.
//!
//! The parser is `no_std`, allocation-free, and bounds-checks every read
//! against the blob length, returning [`FdtError`] rather than panicking
//! (`AGENTS.md` §2.9). It is host-testable: [`Fdt::new`] accepts a
//! borrowed blob so the unit tests drive it against a hand-built fixture
//! without a freestanding target.
//!
//! The format is the Devicetree Specification v0.4 flattened layout: a
//! big-endian header, a structure block of `FDT_*` tokens, and a strings
//! block.
//!
//! The blob is firmware/bootloader-supplied, so it is untrusted input
//! (`AGENTS.md` §19.5): every read is bounds-checked and a malformed tree is
//! rejected, never trusted (§5.4 — fail closed). That decode path carries a
//! §19.6 fuzz harness (`tests/fuzz_fdt.rs`, registered as `fuzz_fdt` in
//! `cargo xtask fuzz`), which drives mutated, truncated, and arbitrary device
//! trees through [`Fdt::new`] and every public reader and asserts none of them
//! ever panics or reads out of bounds.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

#[cfg(any(test, feature = "test-fixtures"))]
extern crate alloc;

#[cfg(any(test, feature = "test-fixtures"))]
pub mod fixture;

/// FDT header magic (`0xd00dfeed`, big-endian on the wire).
const FDT_MAGIC: u32 = 0xd00d_feed;

/// `FDT_BEGIN_NODE` — opens a node; followed by its NUL-terminated name,
/// padded to a 4-byte boundary.
const FDT_BEGIN_NODE: u32 = 0x0000_0001;
/// `FDT_END_NODE` — closes the most recently opened node.
const FDT_END_NODE: u32 = 0x0000_0002;
/// `FDT_PROP` — a property: `len: u32`, `nameoff: u32`, then `len` value
/// bytes padded to a 4-byte boundary.
const FDT_PROP: u32 = 0x0000_0003;
/// `FDT_NOP` — padding token, ignored.
const FDT_NOP: u32 = 0x0000_0004;
/// `FDT_END` — terminates the structure block.
const FDT_END: u32 = 0x0000_0009;

/// Devicetree default `#address-cells` when the root omits it.
const DEFAULT_ADDRESS_CELLS: u32 = 2;
/// Devicetree default `#size-cells` when the root omits it.
const DEFAULT_SIZE_CELLS: u32 = 1;

/// Maximum node-nesting depth the walker tracks. QEMU's `virt` tree is
/// shallow; 32 is generous headroom and bounds the parser's stack usage
/// (`AGENTS.md` §2.9 — no unbounded recursion).
const MAX_DEPTH: usize = 32;

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
    /// than the 40-byte header, or a header offset/size escapes the blob.
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
    /// `ptr` must point at the first byte of a flattened device tree whose
    /// `totalsize` header field truthfully describes its length, and the
    /// whole `totalsize` range must be readable for the lifetime `'a`. On
    /// the `virt` boards this is the pointer firmware hands the kernel,
    /// which lives in firmware-reserved RAM for the life of the guest.
    ///
    /// # Errors
    ///
    /// Propagates [`Fdt::new`]'s validation errors.
    pub unsafe fn from_ptr(ptr: *const u8) -> Result<Self, FdtError> {
        // Read the 8-byte prefix (magic + totalsize) to learn the length
        // before forming the full slice.
        // SAFETY: the caller guarantees `ptr` addresses a valid FDT; the
        // first 8 bytes (magic + totalsize) are always present in a
        // well-formed blob.
        let header = unsafe { core::slice::from_raw_parts(ptr, 8) };
        if be_u32(header, 0) != Some(FDT_MAGIC) {
            return Err(FdtError::BadMagic);
        }
        let total = be_u32(header, 4).ok_or(FdtError::TooShort)? as usize;
        if total < 40 {
            return Err(FdtError::TooShort);
        }
        // SAFETY: `total` is the blob's self-described length; the caller
        // guarantees the whole range is readable.
        let blob = unsafe { core::slice::from_raw_parts(ptr, total) };
        Self::new(blob)
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

    /// Read the `/cpus` `timebase-frequency` (the riscv64 `time` CSR tick
    /// rate in Hz).
    ///
    /// Returns the first occurrence found while walking the tree, or
    /// `None` if the property is absent.
    #[must_use]
    pub fn timebase_frequency(&self) -> Option<u64> {
        self.walk().ok().and_then(|w| w.timebase)
    }

    /// Enumerate every `/cpus/cpu@*` node in tree order, invoking
    /// `f(reg, capacity)` once per CPU node.
    ///
    /// * `reg` is the integer value of the node's `reg` property — the
    ///   CPU's `MPIDR_EL1` affinity on aarch64 / hart id on riscv64 —
    ///   decoded from a one-cell (`u32`) or two-cell (`u64`) value. A CPU
    ///   node with no readable `reg` is skipped (it cannot be matched to a
    ///   logical CPU).
    /// * `capacity` is the `capacity-dmips-mhz` value (the per-core DMIPS
    ///   rating used to classify `big.LITTLE` cores), or `None` when the
    ///   node omits it — a homogeneous machine.
    ///
    /// # Errors
    ///
    /// Returns [`FdtError::Malformed`] if the structure block is
    /// malformed (a truncated token, an unterminated name, or unbalanced
    /// node nesting); the closure is not invoked for a malformed tree.
    pub fn each_cpu<F: FnMut(u64, Option<u64>)>(&self, mut f: F) -> Result<(), FdtError> {
        let struct_end = self.struct_off + self.struct_size;
        let mut pos = self.struct_off;

        // Per-depth "is this the `/cpus` container" / "is this a `cpu@*`
        // node" flags, restored implicitly by `depth` on `FDT_END_NODE`.
        let mut is_cpus = [false; MAX_DEPTH];
        let mut is_cpu = [false; MAX_DEPTH];
        let mut depth: usize = 0;

        // Accumulators for the cpu node currently open (they never nest).
        let mut reg: Option<u64> = None;
        let mut capacity: Option<u64> = None;

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
                    // `/cpus` is a direct child of root (open-time depth 1);
                    // a `cpu@*` node is a direct child of `/cpus`
                    // (open-time depth 2 with the parent flagged).
                    is_cpus[depth] = depth == 1 && name_stem(name) == b"cpus";
                    is_cpu[depth] = depth == 2 && is_cpus[depth - 1] && name_stem(name) == b"cpu";
                    if is_cpu[depth] {
                        reg = None;
                        capacity = None;
                    }
                    depth += 1;
                }
                FDT_END_NODE => {
                    depth = depth.checked_sub(1).ok_or(FdtError::Malformed)?;
                    if is_cpu[depth] {
                        if let Some(mpidr) = reg {
                            f(mpidr, capacity);
                        }
                    }
                }
                FDT_PROP => {
                    let (prop_name, value) = self.read_prop(&mut pos, struct_end)?;
                    if depth >= 1 && is_cpu[depth - 1] {
                        if prop_name == b"reg" {
                            reg = read_int_cells(value);
                        } else if prop_name == b"capacity-dmips-mhz" {
                            capacity = read_int_cells(value);
                        }
                    }
                }
                _ => return Err(FdtError::Malformed),
            }
        }
        Ok(())
    }

    /// Read the raw bytes of property `name` on the node reached by the
    /// child-name `path` from the root.
    ///
    /// Each `path` component matches a node's unit name with any
    /// `@<unit-address>` suffix stripped (so `b"psci"` matches `psci` and
    /// `b"memory"` matches `memory@80000000`). Returns the property value
    /// of the first matching node, or `None` if no such node/property
    /// exists or the tree is malformed.
    #[must_use]
    pub fn property(&self, path: &[&[u8]], name: &[u8]) -> Option<&'a [u8]> {
        if path.len() > MAX_DEPTH {
            return None;
        }
        self.find_property(path, name).ok().flatten()
    }

    /// Read property `name` on the node at `path` as an integer of one
    /// (`u32`) or two (`u64`) big-endian cells.
    #[must_use]
    pub fn property_u64(&self, path: &[&[u8]], name: &[u8]) -> Option<u64> {
        self.property(path, name).and_then(read_int_cells)
    }

    /// Single pass over the structure block collecting the memory region
    /// and timebase frequency.
    fn walk(&self) -> Result<WalkResult, FdtError> {
        let struct_end = self.struct_off + self.struct_size;
        let mut pos = self.struct_off;

        // Root `#address-cells` / `#size-cells` govern the `/memory` `reg`
        // layout; default to the Devicetree-spec values until the root
        // node overrides them.
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
                    // A `/memory` node lives directly under root (depth 1
                    // after this push); its unit name is `memory` or
                    // `memory@<addr>`.
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

    /// Walk the tree looking for `name` on the node matched by `path`.
    fn find_property(&self, path: &[&[u8]], name: &[u8]) -> Result<Option<&'a [u8]>, FdtError> {
        let struct_end = self.struct_off + self.struct_size;
        let mut pos = self.struct_off;

        // Per-level match flag. Flag index 0 is the unnamed root and is
        // always "matched" (transparent); a node at flag index `k >= 1`
        // corresponds to path component `path[k - 1]`. `depth` is the
        // number of currently-open nodes, so the node just opened sits at
        // flag index `depth - 1`.
        let mut matched = [false; MAX_DEPTH];
        let mut depth: usize = 0;

        while pos < struct_end {
            let token = be_u32(self.blob, pos).ok_or(FdtError::Malformed)?;
            pos += 4;
            match token {
                FDT_NOP => {}
                FDT_END => break,
                FDT_BEGIN_NODE => {
                    let node_name = self.read_node_name(&mut pos, struct_end)?;
                    if depth >= MAX_DEPTH {
                        return Err(FdtError::Malformed);
                    }
                    let idx = depth;
                    matched[idx] = if idx == 0 {
                        // The unnamed root is transparent.
                        true
                    } else {
                        let comp = idx - 1;
                        comp < path.len()
                            && matched[..idx].iter().all(|m| *m)
                            && name_stem(node_name) == path[comp]
                    };
                    depth += 1;
                }
                FDT_END_NODE => {
                    depth = depth.checked_sub(1).ok_or(FdtError::Malformed)?;
                }
                FDT_PROP => {
                    let (prop_name, value) = self.read_prop(&mut pos, struct_end)?;
                    // The current node sits at flag index `depth - 1`; it
                    // matches the full path iff that index equals
                    // `path.len()` (the root consumes no component) and
                    // every level matched.
                    if depth == path.len() + 1
                        && matched[..depth].iter().all(|m| *m)
                        && prop_name == name
                    {
                        return Ok(Some(value));
                    }
                }
                _ => return Err(FdtError::Malformed),
            }
        }
        Ok(None)
    }

    /// Read a NUL-terminated node name at `*pos`, advancing `*pos` past the
    /// 4-byte-aligned end of the name.
    fn read_node_name(&self, pos: &mut usize, struct_end: usize) -> Result<&'a [u8], FdtError> {
        read_node_name(self.blob, pos, struct_end)
    }

    /// Read an `FDT_PROP` body at `*pos` (`len`, `nameoff`, then the padded
    /// value), advancing `*pos` past it. Returns the property name and
    /// value slice.
    fn read_prop(
        &self,
        pos: &mut usize,
        struct_end: usize,
    ) -> Result<(&'a [u8], &'a [u8]), FdtError> {
        read_prop(
            self.blob,
            self.strings_off,
            self.strings_size,
            pos,
            struct_end,
        )
    }

    /// Iterate every node of the tree in document order.
    ///
    /// Each item is a [`Node`] handle exposing the node's properties
    /// ([`Node::property`] / [`Node::is_compatible`]). The iterator yields
    /// `Err(FdtError)` and then stops if it meets a malformed token, so a
    /// hostile blob fails closed rather than silently under-enumerating
    /// (`AGENTS.md` §2.9). This is the generic walk the bus enumerators and
    /// the QEMU verticals discover the `virt` tree through — one parser for
    /// every consumer (`AGENTS.md` §2.2).
    #[must_use]
    pub fn nodes(&self) -> NodeIter<'a> {
        NodeIter {
            blob: self.blob,
            strings_off: self.strings_off,
            strings_size: self.strings_size,
            struct_end: self.struct_off + self.struct_size,
            pos: self.struct_off,
            depth: 0,
        }
    }
}

/// Read the NUL-terminated string at `nameoff` in a strings block of
/// `[strings_off, strings_off + strings_size)` within `blob`.
fn string_at(
    blob: &[u8],
    strings_off: usize,
    strings_size: usize,
    nameoff: usize,
) -> Option<&[u8]> {
    let start = strings_off.checked_add(nameoff)?;
    if nameoff >= strings_size {
        return None;
    }
    let block_end = strings_off.checked_add(strings_size)?;
    let region = blob.get(start..block_end)?;
    let len = region.iter().position(|&b| b == 0)?;
    Some(&region[..len])
}

/// Read a NUL-terminated node name at `*pos`, advancing `*pos` past the
/// 4-byte-aligned end of the name.
fn read_node_name<'a>(
    blob: &'a [u8],
    pos: &mut usize,
    struct_end: usize,
) -> Result<&'a [u8], FdtError> {
    let region = blob.get(*pos..struct_end).ok_or(FdtError::Malformed)?;
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

/// Read an `FDT_PROP` body at `*pos` (`len`, `nameoff`, then the padded
/// value), advancing `*pos` past it. Returns the property name and value
/// slice.
fn read_prop<'a>(
    blob: &'a [u8],
    strings_off: usize,
    strings_size: usize,
    pos: &mut usize,
    struct_end: usize,
) -> Result<(&'a [u8], &'a [u8]), FdtError> {
    let len = be_u32(blob, *pos).ok_or(FdtError::Malformed)? as usize;
    let nameoff = be_u32(blob, *pos + 4).ok_or(FdtError::Malformed)? as usize;
    let value_start = pos.checked_add(8).ok_or(FdtError::Malformed)?;
    let value_end = value_start.checked_add(len).ok_or(FdtError::Malformed)?;
    if value_end > struct_end {
        return Err(FdtError::Malformed);
    }
    let value = &blob[value_start..value_end];
    let name = string_at(blob, strings_off, strings_size, nameoff).ok_or(FdtError::Malformed)?;
    *pos = align_up(value_end, 4);
    Ok((name, value))
}

/// Iterator over the nodes of an [`Fdt`] in document order, produced by
/// [`Fdt::nodes`].
#[derive(Clone)]
pub struct NodeIter<'a> {
    blob: &'a [u8],
    strings_off: usize,
    strings_size: usize,
    struct_end: usize,
    pos: usize,
    depth: u32,
}

impl<'a> Iterator for NodeIter<'a> {
    type Item = Result<Node<'a>, FdtError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.pos >= self.struct_end {
                return None;
            }
            let Some(token) = be_u32(self.blob, self.pos) else {
                return Some(Err(FdtError::Malformed));
            };
            self.pos += 4;
            match token {
                FDT_NOP => {}
                FDT_END => return None,
                FDT_BEGIN_NODE => {
                    let name = match read_node_name(self.blob, &mut self.pos, self.struct_end) {
                        Ok(n) => n,
                        Err(e) => return Some(Err(e)),
                    };
                    let depth = self.depth;
                    self.depth += 1;
                    // Properties precede child nodes in a valid blob
                    // (Devicetree Spec v0.4 §5.4.2), so the returned node's
                    // `PropIter` starting here stops at the first child.
                    return Some(Ok(Node {
                        blob: self.blob,
                        strings_off: self.strings_off,
                        strings_size: self.strings_size,
                        struct_end: self.struct_end,
                        name,
                        depth,
                        props_pos: self.pos,
                    }));
                }
                FDT_END_NODE => {
                    self.depth = match self.depth.checked_sub(1) {
                        Some(d) => d,
                        None => return Some(Err(FdtError::Malformed)),
                    };
                }
                FDT_PROP => {
                    if let Err(e) = read_prop(
                        self.blob,
                        self.strings_off,
                        self.strings_size,
                        &mut self.pos,
                        self.struct_end,
                    ) {
                        return Some(Err(e));
                    }
                }
                _ => return Some(Err(FdtError::Malformed)),
            }
        }
    }
}

/// A single device-tree node visited by [`NodeIter`].
#[derive(Copy, Clone)]
pub struct Node<'a> {
    blob: &'a [u8],
    strings_off: usize,
    strings_size: usize,
    struct_end: usize,
    name: &'a [u8],
    depth: u32,
    props_pos: usize,
}

impl<'a> Node<'a> {
    /// The node's unit-name bytes (e.g. `b"virtio_mmio@a000000"`); empty
    /// for the root node.
    #[must_use]
    pub fn name(&self) -> &'a [u8] {
        self.name
    }

    /// Depth within the tree; the root node is `0`.
    #[must_use]
    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// Iterate this node's immediate properties (not those of children).
    #[must_use]
    pub fn properties(&self) -> PropIter<'a> {
        PropIter {
            blob: self.blob,
            strings_off: self.strings_off,
            strings_size: self.strings_size,
            struct_end: self.struct_end,
            pos: self.props_pos,
        }
    }

    /// Return the property named `name`, if present.
    #[must_use]
    pub fn property(&self, name: &str) -> Option<Property<'a>> {
        self.properties().find_map(|p| match p {
            Ok(p) if p.name == name.as_bytes() => Some(p),
            _ => None,
        })
    }

    /// `true` iff this node's `compatible` property lists `target` as one
    /// of its NUL-separated strings.
    #[must_use]
    pub fn is_compatible(&self, target: &str) -> bool {
        match self.property("compatible") {
            Some(p) => p.iter_strings().any(|s| s == target.as_bytes()),
            None => false,
        }
    }
}

/// Iterator over the properties immediately under a [`Node`].
#[derive(Clone)]
pub struct PropIter<'a> {
    blob: &'a [u8],
    strings_off: usize,
    strings_size: usize,
    struct_end: usize,
    pos: usize,
}

impl<'a> Iterator for PropIter<'a> {
    type Item = Result<Property<'a>, FdtError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.pos >= self.struct_end {
                return None;
            }
            let Some(token) = be_u32(self.blob, self.pos) else {
                return Some(Err(FdtError::Malformed));
            };
            self.pos += 4;
            match token {
                FDT_NOP => {}
                FDT_PROP => {
                    return Some(
                        read_prop(
                            self.blob,
                            self.strings_off,
                            self.strings_size,
                            &mut self.pos,
                            self.struct_end,
                        )
                        .map(|(name, value)| Property { name, value }),
                    );
                }
                // Properties stop at the first non-property boundary: a
                // child node, the end of this node, or the end of block.
                FDT_BEGIN_NODE | FDT_END_NODE | FDT_END => return None,
                _ => return Some(Err(FdtError::Malformed)),
            }
        }
    }
}

/// A single property of a [`Node`].
#[derive(Copy, Clone)]
pub struct Property<'a> {
    name: &'a [u8],
    value: &'a [u8],
}

impl<'a> Property<'a> {
    /// The property name bytes (e.g. `b"reg"`, `b"compatible"`).
    #[must_use]
    pub fn name(&self) -> &'a [u8] {
        self.name
    }

    /// The raw property payload (big-endian on the wire).
    #[must_use]
    pub fn value(&self) -> &'a [u8] {
        self.value
    }

    /// Iterate the NUL-separated strings inside a stringlist property such
    /// as `compatible`.
    #[must_use]
    pub fn iter_strings(&self) -> StringList<'a> {
        StringList { rem: self.value }
    }

    /// Read a single big-endian `u32` at `offset` inside the value.
    ///
    /// # Errors
    ///
    /// Returns [`FdtError::OutOfBounds`] if `offset + 4` exceeds the value
    /// length.
    pub fn read_be_u32(&self, offset: usize) -> Result<u32, FdtError> {
        be_u32(self.value, offset).ok_or(FdtError::OutOfBounds)
    }

    /// Read a single big-endian `u64` (two cells) at `offset` inside the
    /// value.
    ///
    /// # Errors
    ///
    /// Returns [`FdtError::OutOfBounds`] if `offset + 8` exceeds the value
    /// length.
    pub fn read_be_u64(&self, offset: usize) -> Result<u64, FdtError> {
        read_cells(self.value, offset, 2).ok_or(FdtError::OutOfBounds)
    }
}

/// Iterator over the NUL-separated strings inside a stringlist property.
#[derive(Clone)]
pub struct StringList<'a> {
    rem: &'a [u8],
}

impl<'a> Iterator for StringList<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        if self.rem.is_empty() {
            return None;
        }
        let nul = self.rem.iter().position(|&b| b == 0)?;
        let (head, tail) = self.rem.split_at(nul);
        self.rem = tail.get(1..).unwrap_or(&[]);
        Some(head)
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

/// The portion of a node's unit name before any `@<unit-address>` suffix.
fn name_stem(name: &[u8]) -> &[u8] {
    match name.iter().position(|&b| b == b'@') {
        Some(at) => &name[..at],
        None => name,
    }
}

/// Decode `cells` big-endian `u32` cells starting at `off` into a `u64`.
/// Returns `None` if the slice is too short.
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

/// Read an integer property whose value is one `u32` cell or two (`u64`).
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
    use crate::fixture::{arm_with_cpus, virt_like, virt_like_arm, DtbBuilder};
    use alloc::vec::Vec;

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
        // SAFETY: `blob` is a valid FDT whose totalsize header equals its
        // length; the slice outlives the `Fdt` built from it.
        let fdt = unsafe { Fdt::from_ptr(blob.as_ptr()) }.expect("valid fdt");
        assert_eq!(fdt.first_memory_region(), Some((0x8000_0000, 0x1000_0000)));
    }

    #[test]
    fn property_reads_psci_method_and_timer_ppi() {
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 14);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(fdt.property(&[b"psci"], b"method"), Some(&b"hvc\0"[..]));
        // The /timer interrupts triple is <type, number, flags>; the
        // second cell is the PPI number.
        let interrupts = fdt.property(&[b"timer"], b"interrupts").expect("present");
        assert_eq!(be_u32(interrupts, 4), Some(14));
        assert_eq!(fdt.first_memory_region(), Some((0x4000_0000, 0x2000_0000)));
    }

    #[test]
    fn property_misses_unknown_node_and_prop() {
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "smc", 14);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(fdt.property(&[b"nope"], b"method"), None);
        assert_eq!(fdt.property(&[b"psci"], b"absent"), None);
        // A deeper path than the tree has no match.
        assert_eq!(fdt.property(&[b"psci", b"child"], b"method"), None);
    }

    #[test]
    fn each_cpu_reads_mpidr_and_capacity_in_tree_order() {
        // A big.LITTLE part: two performance cores (cap 1024) and two
        // efficiency cores (cap 512); the last core omits the capacity.
        let blob = arm_with_cpus(
            0x4000_0000,
            0x2000_0000,
            &[
                (0x0, Some(1024)),
                (0x1, Some(512)),
                (0x100, Some(1024)),
                (0x101, None),
            ],
        );
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let mut seen: Vec<(u64, Option<u64>)> = Vec::new();
        fdt.each_cpu(|mpidr, cap| seen.push((mpidr, cap)))
            .expect("walk succeeds");
        assert_eq!(
            seen,
            [
                (0x0, Some(1024)),
                (0x1, Some(512)),
                (0x100, Some(1024)),
                (0x101, None),
            ]
        );
    }

    #[test]
    fn each_cpu_yields_nothing_when_there_are_no_cpu_nodes() {
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 14);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let mut count = 0usize;
        fdt.each_cpu(|_, _| count += 1).expect("walk succeeds");
        assert_eq!(count, 0);
    }

    #[test]
    fn property_u64_decodes_single_and_double_cells() {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.begin_node("chosen");
        b.prop_u32("one-cell", 0x1234);
        let mut two = Vec::new();
        two.extend_from_slice(&0x0123_4567_89ab_cdefu64.to_be_bytes());
        b.prop("two-cell", &two);
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(fdt.property_u64(&[b"chosen"], b"one-cell"), Some(0x1234));
        assert_eq!(
            fdt.property_u64(&[b"chosen"], b"two-cell"),
            Some(0x0123_4567_89ab_cdef)
        );
    }

    #[test]
    fn nodes_enumerate_and_read_virtio_mmio_slots() {
        // A `virt`-shaped tree: two virtio-MMIO transports and an
        // unrelated `/memory` node, mirroring the QEMU `virt` layout the
        // bus enumerator walks.
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        for (i, base) in [0x0a00_0000u64, 0x0a00_0200].iter().enumerate() {
            let name = alloc::format!("virtio_mmio@{base:x}");
            b.begin_node(&name);
            b.prop("compatible", b"virtio,mmio\0");
            let mut reg = Vec::new();
            reg.extend_from_slice(&base.to_be_bytes());
            reg.extend_from_slice(&0x200u64.to_be_bytes());
            b.prop("reg", &reg);
            let mut irq = Vec::new();
            let irq_number = 0x10 + u32::try_from(i).expect("slot index fits u32");
            for cell in [0u32, irq_number, 0x04] {
                irq.extend_from_slice(&cell.to_be_bytes());
            }
            b.prop("interrupts", &irq);
            b.end_node();
        }
        b.begin_node("memory@40000000");
        b.prop("device_type", b"memory\0");
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");

        let mut slots: Vec<(u64, u64, u32)> = Vec::new();
        for node in fdt.nodes() {
            let node = node.expect("node parses");
            if !node.is_compatible("virtio,mmio") {
                continue;
            }
            let reg = node.property("reg").expect("reg present");
            let base = reg.read_be_u64(0).expect("base");
            let len = reg.read_be_u64(8).expect("len");
            let irq = node
                .property("interrupts")
                .expect("interrupts present")
                .read_be_u32(4)
                .expect("irq cell");
            slots.push((base, len, irq));
        }
        assert_eq!(
            slots,
            [(0x0a00_0000, 0x200, 0x10), (0x0a00_0200, 0x200, 0x11)]
        );
    }

    #[test]
    fn node_is_compatible_false_for_absent_or_mismatched() {
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 14);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let mut saw_psci = false;
        let mut saw_virtio = false;
        for node in fdt.nodes() {
            let node = node.expect("node parses");
            saw_psci |= node.is_compatible("arm,psci-1.0");
            saw_virtio |= node.is_compatible("virtio,mmio");
        }
        assert!(saw_psci);
        assert!(!saw_virtio);
        // The root node has no `compatible` and no arbitrary property.
        let root = fdt.nodes().next().expect("root present").expect("ok");
        assert!(!root.is_compatible("anything"));
        assert!(root.property("missing").is_none());
    }

    #[test]
    fn property_reads_fail_closed_past_the_value_end() {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.begin_node("dev");
        b.prop("reg", &0x1234u32.to_be_bytes());
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let dev = fdt
            .nodes()
            .find_map(|n| {
                let n = n.ok()?;
                (n.name() == b"dev").then_some(n)
            })
            .expect("dev node");
        let reg = dev.property("reg").expect("reg present");
        assert_eq!(reg.read_be_u32(0), Ok(0x1234));
        assert_eq!(reg.read_be_u64(0).err(), Some(FdtError::OutOfBounds));
        assert_eq!(reg.read_be_u32(4).err(), Some(FdtError::OutOfBounds));
    }

    #[test]
    fn nodes_fail_closed_on_malformed_token() {
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 14);
        let mut corrupt = blob.clone();
        // Overwrite the first structure-block token with an unknown value
        // (the structure block begins at the 40-byte header end).
        corrupt[40..44].copy_from_slice(&0x00ff_ff00u32.to_be_bytes());
        let fdt = Fdt::new(&corrupt).expect("header still valid");
        assert!(matches!(fdt.nodes().next(), Some(Err(FdtError::Malformed))));
    }
}
