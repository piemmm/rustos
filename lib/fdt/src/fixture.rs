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

/// An aarch64 tree carrying a `/cpus` node whose `cpu@*` children declare
/// per-core `reg` (the `MPIDR_EL1` affinity) and an optional
/// `capacity-dmips-mhz` rating, plus the usual `/memory` node.
///
/// Each entry of `cpus` is `(mpidr, capacity)`: a `Some` capacity emits a
/// `capacity-dmips-mhz` property (a `big.LITTLE` part), a `None` omits it
/// (a homogeneous part). Used to exercise [`crate::Fdt::each_cpu`] and the
/// aarch64 heterogeneous-core classifier.
#[must_use]
pub fn arm_with_cpus(base: u64, size: u64, cpus: &[(u64, Option<u32>)]) -> Vec<u8> {
    let mut b = DtbBuilder::new();
    b.begin_node("");
    b.prop_u32("#address-cells", 2);
    b.prop_u32("#size-cells", 2);

    b.begin_node("cpus");
    b.prop_u32("#address-cells", 1);
    b.prop_u32("#size-cells", 0);
    for (mpidr, capacity) in cpus {
        let name = alloc::format!("cpu@{mpidr:x}");
        b.begin_node(&name);
        b.prop_str("device_type", "cpu");
        // The fixture writes a single-cell (`#address-cells = 1`) `reg`;
        // `try_from` keeps the cast honest rather than silently truncating
        // a value that does not fit one cell (`AGENTS.md` §2.1).
        b.prop_u32(
            "reg",
            u32::try_from(*mpidr).expect("fixture MPIDR fits one cell"),
        );
        if let Some(cap) = capacity {
            b.prop_u32("capacity-dmips-mhz", *cap);
        }
        b.end_node();
    }
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

/// A Raspberry-Pi-shaped aarch64 tree carrying the two console UARTs the
/// Pi exposes — a PrimeCell PL011 (`arm,pl011`) and a BCM2835 AUX
/// mini-UART (`brcm,bcm2835-aux-uart`) — a GIC-400 interrupt controller
/// (`arm,gic-400`) at the BCM2711 bases, the `VideoCore` firmware mailbox
/// (`brcm,bcm2835-mbox`) at the BCM2711 ARM-physical base `0xFE00_B880`
/// with a `0x40`-byte doorbell window, plus a `/psci` (`smc`) node and a
/// 1 GiB `/memory@0` node.
///
/// The tree mirrors the real `bcm2711-rpi-4-b.dtb` shape: the root
/// declares `#address-cells = 2` / `#size-cells = 1`, and every
/// peripheral sits under a `/soc` `simple-bus` whose `#address-cells` /
/// `#size-cells` are both `1` and whose three-entry `ranges` remap the
/// legacy bus windows into CPU-physical space (`0x7E00_0000 →
/// 0xFE00_0000`, `0x7C00_0000 → 0xFC00_0000`, `0x4000_0000 →
/// 0xFF80_0000`). `pl011_base` and `miniuart_base` are therefore the
/// *bus* addresses the nodes' `reg` cells carry (e.g. `0x7E20_1000` /
/// `0x7E21_5040`); readers must translate them through the `/soc`
/// `ranges` exactly as on the real board. A `pl011_base` of `0` omits
/// the PL011 node, leaving the mini-UART as the only console — used to
/// exercise the aarch64 port's console-model fallback. The PL011 window
/// is `0x200` bytes; the mini-UART window is `0x40` bytes (the
/// `AUX_MU_*` register block); the GIC-400 carries the real tree's four
/// one-cell regions (GICD/GICC/GICH/GICV).
#[must_use]
pub fn raspi_like_arm(pl011_base: u64, miniuart_base: u64) -> Vec<u8> {
    let mut b = DtbBuilder::new();
    b.begin_node("");
    b.prop_u32("#address-cells", 2);
    b.prop_u32("#size-cells", 1);

    b.begin_node("psci");
    b.prop_str("compatible", "arm,psci-1.0");
    b.prop_str("method", "smc");
    b.end_node();

    // One `reg` entry under `/soc`: a one-cell bus address plus a
    // one-cell length, exactly as the real BCM2711 tree encodes them.
    let soc_reg = |base: u64, size: u32| {
        let mut reg = Vec::new();
        reg.extend_from_slice(
            &u32::try_from(base)
                .expect("bus address fits one cell")
                .to_be_bytes(),
        );
        reg.extend_from_slice(&size.to_be_bytes());
        reg
    };

    b.begin_node("soc");
    b.prop_str("compatible", "simple-bus");
    b.prop_u32("#address-cells", 1);
    b.prop_u32("#size-cells", 1);
    // The real tree's three windows: one-cell child address, two-cell
    // parent address, one-cell size per entry.
    let mut ranges = Vec::new();
    for (child, parent, size) in [
        (0x7e00_0000u32, 0xfe00_0000u64, 0x0180_0000u32),
        (0x7c00_0000, 0xfc00_0000, 0x0200_0000),
        (0x4000_0000, 0xff80_0000, 0x0080_0000),
    ] {
        ranges.extend_from_slice(&child.to_be_bytes());
        ranges.extend_from_slice(&parent.to_be_bytes());
        ranges.extend_from_slice(&size.to_be_bytes());
    }
    b.prop("ranges", &ranges);

    // GIC-400 (a GICv2) at the real tree's bus addresses (CPU-physical
    // GICD `0xFF84_1000`, GICC `0xFF84_2000` through the `0x4000_0000 →
    // 0xFF80_0000` range): four `reg` regions — GICD, GICC, GICH, GICV.
    b.begin_node("interrupt-controller@40041000");
    b.prop_str("compatible", "arm,gic-400");
    let mut gic_reg = soc_reg(0x4004_1000, 0x1000);
    gic_reg.extend_from_slice(&soc_reg(0x4004_2000, 0x2000));
    gic_reg.extend_from_slice(&soc_reg(0x4004_4000, 0x2000));
    gic_reg.extend_from_slice(&soc_reg(0x4004_6000, 0x2000));
    b.prop("reg", &gic_reg);
    b.end_node();

    // The VideoCore firmware mailbox doorbell block at its bus address
    // (CPU-physical `0xFE00_B880`), the node the HVS framebuffer
    // discovery binds.
    b.begin_node("mailbox@7e00b880");
    b.prop_str("compatible", "brcm,bcm2835-mbox");
    b.prop("reg", &soc_reg(0x7e00_b880, 0x40));
    b.end_node();

    if pl011_base != 0 {
        b.begin_node(&alloc::format!("serial@{pl011_base:x}"));
        b.prop_str("compatible", "arm,pl011");
        b.prop("reg", &soc_reg(pl011_base, 0x200));
        b.end_node();
    }

    b.begin_node(&alloc::format!("serial@{miniuart_base:x}"));
    b.prop_str("compatible", "brcm,bcm2835-aux-uart");
    b.prop("reg", &soc_reg(miniuart_base, 0x40));
    b.end_node();

    b.end_node(); // /soc

    b.begin_node("memory@0");
    b.prop("device_type", b"memory\0");
    // Root cells: two-cell address, one-cell size (the real tree's
    // shape; the firmware patches the size in at boot).
    let mut mem_reg = Vec::new();
    mem_reg.extend_from_slice(&0u64.to_be_bytes());
    mem_reg.extend_from_slice(&0x4000_0000u32.to_be_bytes());
    b.prop("reg", &mem_reg);
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

    // GICv2 interrupt controller (`intc@8000000` on the real `virt`
    // board): distributor `0x0800_0000`, CPU interface `0x0801_0000`,
    // each a 0x10000 window. Two `reg` regions, the layout the aarch64
    // GIC discovery reads.
    b.begin_node("intc@8000000");
    b.prop_str("compatible", "arm,cortex-a15-gic");
    let mut gic_reg = Vec::new();
    for cell in [0x0800_0000u64, 0x1_0000, 0x0801_0000, 0x1_0000] {
        gic_reg.extend_from_slice(&cell.to_be_bytes());
    }
    b.prop("reg", &gic_reg);
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
