//! In-memory flattened-device-tree builder for tests (`AGENTS.md` §2.2).
//!
//! Exposed behind the `test-fixtures` feature so this crate's own parser
//! tests **and** the architecture ports' discovery tests drive one DTB
//! builder rather than re-rolling the byte layout in each crate. It mirrors
//! the on-disk layout QEMU's `virt` boards produce closely enough to
//! exercise every branch the boot pipeline relies on.

use alloc::vec::Vec;

use crate::{FDT_BEGIN_NODE, FDT_END, FDT_END_NODE, FDT_MAGIC, FDT_PROP};

/// Builder for a minimal flattened device tree.
pub struct DtbBuilder {
    strings: Vec<u8>,
    structure: Vec<u8>,
}

impl Default for DtbBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl DtbBuilder {
    /// Start an empty builder.
    #[must_use]
    pub fn new() -> Self {
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

    /// Open a node with unit name `name`.
    pub fn begin_node(&mut self, name: &str) {
        self.token(FDT_BEGIN_NODE);
        self.structure.extend_from_slice(name.as_bytes());
        self.structure.push(0);
        self.pad4();
    }

    /// Close the most recently opened node.
    pub fn end_node(&mut self) {
        self.token(FDT_END_NODE);
    }

    /// Emit a property with raw `value` bytes.
    pub fn prop(&mut self, name: &str, value: &[u8]) {
        let nameoff = self.intern(name);
        self.token(FDT_PROP);
        let len = u32::try_from(value.len()).expect("prop value fits u32");
        self.structure.extend_from_slice(&len.to_be_bytes());
        self.structure.extend_from_slice(&nameoff.to_be_bytes());
        self.structure.extend_from_slice(value);
        self.pad4();
    }

    /// Emit a property holding a single big-endian `u32` cell.
    pub fn prop_u32(&mut self, name: &str, v: u32) {
        self.prop(name, &v.to_be_bytes());
    }

    /// Emit a property holding a NUL-terminated string.
    pub fn prop_str(&mut self, name: &str, v: &str) {
        let mut bytes = Vec::from(v.as_bytes());
        bytes.push(0);
        self.prop(name, &bytes);
    }

    /// Finalise the blob: header, structure block (with a trailing
    /// `FDT_END`), and strings block.
    #[must_use]
    pub fn build(mut self) -> Vec<u8> {
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

/// A QEMU-`virt`-shaped riscv64 tree: 2/2 root cells, a `/cpus` node with
/// a `timebase-frequency`, and a `/memory@80000000` node.
#[must_use]
pub fn virt_like(base: u64, size: u64, timebase: u32) -> Vec<u8> {
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

/// A QEMU-`virt`-shaped aarch64 tree: 2/2 root cells, a `/memory` node, a
/// `/psci` node with a `method` (`hvc`/`smc`), and a `/timer` node with an
/// `interrupts` cell list (the per-CPU PPI the generic timer raises).
#[must_use]
pub fn virt_like_arm(base: u64, size: u64, psci_method: &str, timer_ppi: u32) -> Vec<u8> {
    let mut b = DtbBuilder::new();
    b.begin_node("");
    b.prop_u32("#address-cells", 2);
    b.prop_u32("#size-cells", 2);

    b.begin_node("psci");
    b.prop_str("compatible", "arm,psci-1.0");
    b.prop_str("method", psci_method);
    b.end_node();

    b.begin_node("timer");
    b.prop_str("compatible", "arm,armv8-timer");
    // GIC interrupt specifier triple: <type, number, flags>. The fourth
    // (EL1 physical timer) entry is the one the kernel arms; the fixture
    // carries that PPI number for the discovery reader to surface.
    let mut interrupts = Vec::new();
    for cell in [1u32, timer_ppi, 0x08] {
        interrupts.extend_from_slice(&cell.to_be_bytes());
    }
    b.prop("interrupts", &interrupts);
    b.end_node();

    b.begin_node("memory@40000000");
    b.prop("device_type", b"memory\0");
    let mut reg = Vec::new();
    reg.extend_from_slice(&base.to_be_bytes());
    reg.extend_from_slice(&size.to_be_bytes());
    b.prop("reg", &reg);
    b.end_node();

    b.end_node();
    b.build()
}
