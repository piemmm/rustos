//! aarch64 device-tree access.
//!
//! The flattened-device-tree parser itself is architecture-neutral and
//! lives once in [`tairix_fdt`] (no duplication); this
//! module re-exports it and layers the aarch64-specific *queries* the boot
//! path needs from the `virt` board's device tree:
//!
//! * the first `/memory` region (delegated to
//!   [`tairix_fdt::Fdt::first_memory_region`]);
//! * the `/psci` `method` — the conduit (`hvc`/`smc`) the kernel uses to
//!   call PSCI firmware for secondary-core bring-up (the prerequisite for
//!   aarch64 SMP, `plans/WIRING.md` Stage W6);
//! * the optional generic-timer `clock-frequency` counter-rate override
//!   the `/timer` node may carry (`plans/PI.md` P4).
//!
//! Devices reach [`tairix_abi::hwtree`] through the *generic* walk in
//! [`crate::platform`] — no per-device query is needed for a node to be
//! discovered and matched.
//!
//! The bus-aware `reg` decoding those walks rest on ([`tairix_fdt::bus`])
//! is pure Devicetree Specification and lives beside the parser, so every
//! port shares one definition; only the GIC interrupt specifier below is
//! genuinely this architecture's.

use tairix_fdt::{read_cells, scan_translated, translated_reg, BusLevel, Node};

pub use tairix_fdt::{Fdt, FdtError};

/// The `compatible` string identifying a per-PE external-debug component
/// in the device tree (the Linux `arm,coresight-cpu-debug` binding).
///
/// A node bearing it exposes that CPU's memory-mapped external-debug
/// registers (`EDPCSR`, …) at its `reg` window and names the CPU it
/// belongs to through its `cpu` phandle — the two facts the cross-core PC
/// sampler ([`crate::coresight`]) needs to read a wedged core's PC.
pub const CORESIGHT_CPU_DEBUG_COMPATIBLE: &str = "arm,coresight-cpu-debug";

/// Discover each CPU's CoreSight external-debug component base from the
/// device tree, writing the CPU-physical base of the debug node bound to
/// dense CPU `i` into `out[i]`.
///
/// `cpu_affinities[i]` is the MPIDR affinity of dense `CpuId` `i` (the boot
/// path's ordered `/cpus` map); `out` must be the same length and is left
/// untouched — the caller's "no component" sentinel — for a CPU with no
/// described debug node. For every `arm,coresight-cpu-debug` node the walk
/// translates its `reg` base through the ancestor buses' `ranges`
/// ([`translated_reg`]) and resolves its `cpu` phandle to that CPU's
/// affinity (`cpu_affinity_for_phandle`), recording the base under the
/// matching dense id. A node whose base cannot be translated, whose phandle
/// names no known CPU, or whose CPU is not in `cpu_affinities` is skipped
/// (fail closed — never a fabricated base).
///
/// Alloc-free: it writes directly into `out` and resolves phandles with
/// nested tree lookups, so it runs in the freestanding kernel with no heap.
/// It walks the whole tree, so it must be called with the MMU on (the
/// early-returning discovery queries are the MMU-off ones).
pub fn debug_component_bases(fdt: &Fdt<'_>, cpu_affinities: &[u64], out: &mut [usize]) {
    let _: Option<()> = scan_translated(fdt, |node, levels, depth| {
        if node.is_compatible(CORESIGHT_CPU_DEBUG_COMPATIBLE) {
            if let Some((idx, phys)) =
                debug_node_dense_base(fdt, node, levels, depth, cpu_affinities)
            {
                if let Some(slot) = out.get_mut(idx) {
                    *slot = phys;
                }
            }
        }
        // Never early-return: visit every node so *every* CPU's component
        // is found, not just the first.
        None
    });
}

/// Resolve one `arm,coresight-cpu-debug` `node` to
/// `(dense_cpu_id, cpu_physical_base)`, or `None` when any step fails
/// closed (untranslatable base, missing/unknown `cpu` phandle, or a CPU
/// not present in `cpu_affinities`).
fn debug_node_dense_base(
    fdt: &Fdt<'_>,
    node: &Node<'_>,
    levels: &[BusLevel<'_>],
    depth: usize,
    cpu_affinities: &[u64],
) -> Option<(usize, usize)> {
    let (base, _len) = translated_reg(node, depth, levels, 0)?;
    let phandle = node.property("cpu")?.read_be_u32(0).ok()?;
    let affinity = cpu_affinity_for_phandle(fdt, phandle)?;
    let idx = cpu_affinities.iter().position(|&a| a == affinity)?;
    Some((idx, usize::try_from(base).ok()?))
}

/// Resolve a CPU-node phandle to that CPU's MPIDR affinity (its `reg`), or
/// `None` when no node carries the phandle or it has no readable `reg`.
///
/// Phandles are unique within a device tree, so the first node whose
/// `phandle` (or the legacy `linux,phandle`) matches is the referenced CPU;
/// its `reg` is the affinity, read in the `/cpus` `#address-cells` width
/// (one cell on the common binding, two where the upper affinity bits are
/// used — size cells are zero, so there is no window to translate).
fn cpu_affinity_for_phandle(fdt: &Fdt<'_>, phandle: u32) -> Option<u64> {
    fdt.nodes().flatten().find_map(|node| {
        let node_phandle = node
            .property("phandle")
            .or_else(|| node.property("linux,phandle"))?
            .read_be_u32(0)
            .ok()?;
        if node_phandle != phandle {
            return None;
        }
        let reg = node.property("reg")?.value();
        let cells = if reg.len() >= 8 { 2 } else { 1 };
        read_cells(reg, 0, cells)
    })
}

/// The PSCI conduit a platform uses to call firmware (
/// `plans/WIRING.md` W6).
///
/// Discovered from the `/psci` node's `method` property; selects the
/// instruction (`hvc` at EL2-hosted, `smc` at EL3-hosted) the secondary
/// bring-up path issues.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PsciMethod {
    /// Hypervisor call — the firmware lives at EL2 (QEMU `virt` default).
    Hvc,
    /// Secure-monitor call — the firmware lives at EL3.
    Smc,
}

impl PsciMethod {
    /// Parse a `/psci` `method` property value (a NUL-terminated string).
    #[must_use]
    pub fn from_property(value: &[u8]) -> Option<Self> {
        // Trim a trailing NUL the device tree stores on string properties.
        let s = match value.iter().position(|&b| b == 0) {
            Some(nul) => &value[..nul],
            None => value,
        };
        match s {
            b"hvc" => Some(Self::Hvc),
            b"smc" => Some(Self::Smc),
            _ => None,
        }
    }
}

/// Read the PSCI conduit from the `/psci` node, or `None` if the tree
/// declares no PSCI node or an unrecognised method.
///
/// Matches the `/psci` node through the shared `Fdt::nodes` early-return
/// walk — the same byte-safe traversal [`crate::gic::configure_from_fdt`]
/// and [`timer_clock_frequency`] use — and reads
/// `method` from that node's own properties. It stops at the first
/// matching node and never scans the whole tree, so it stays safe with
/// the MMU off (the secondary-bring-up boot path discovers the conduit
/// before the page tables are live; a whole-tree `Fdt::property`/`walk`
/// scan faults there once the compiler widens the byte reads —
/// `plans/PI.md` P4/P5 watch-out).
#[must_use]
pub fn psci_method(fdt: &Fdt<'_>) -> Option<PsciMethod> {
    let node = fdt.nodes().flatten().find(node_is_psci)?;
    PsciMethod::from_property(node.property("method")?.value())
}

/// `true` iff `node` is the standard `/psci` node — matched by an
/// `arm,psci` `compatible` prefix (covering `arm,psci`, `arm,psci-0.2`,
/// `arm,psci-1.0`, …) so the conduit is read from the firmware's PSCI
/// node regardless of the exact revision it advertises.
fn node_is_psci(node: &tairix_fdt::Node<'_>) -> bool {
    node.property("compatible").is_some_and(|compatible| {
        compatible
            .iter_strings()
            .any(|s| s.starts_with(b"arm,psci"))
    })
}

/// Read the generic-timer counter frequency (Hz) the `/timer` node
/// declares through its `clock-frequency` property, if any.
///
/// `clock-frequency` is the standard `arm,armv?-timer` override the
/// device tree carries when firmware leaves `CNTFRQ_EL0` mis-programmed
/// (the Linux binding honours it ahead of the register). The canonical
/// source is still the `CNTFRQ_EL0` register (`kernel_arch::read_cntfrq`);
/// this surfaces only the optional tree override so the boot path can
/// prefer it. Returns `None` when the node omits the property or the
/// value is not a single big-endian `u32`.
#[must_use]
pub fn timer_clock_frequency(fdt: &Fdt<'_>) -> Option<u32> {
    // Match the generic-timer node by its `compatible` string and read
    // `clock-frequency` from that node's own properties, via the shared
    // `Fdt::nodes` walk — the same early-returning traversal
    // [`crate::gic::configure_from_fdt`] uses. This
    // stops at the first matching node and only ever reads that node's
    // properties, never scanning the whole tree.
    let node = fdt
        .nodes()
        .flatten()
        .find(|node| node.is_compatible("arm,armv8-timer"))?;
    let value = node.property("clock-frequency")?.value();
    let bytes = value.get(0..4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Select the generic-timer frequency (Hz) to drive preemption with,
/// preferring the device-tree `clock-frequency` override
/// ([`timer_clock_frequency`]) when present and non-zero, otherwise the
/// `CNTFRQ_EL0` register value `cntfrq`.
///
/// A zero `clock-frequency` is treated as absent (a board that declares
/// `0` has not really told us a rate), so the register value is used
/// rather than a divide-by-zero timer interval (fail
/// closed). Pure and host-testable: the register read is the caller's
/// responsibility (`kernel_arch::timer_frequency_hz` composes the two on
/// the freestanding target).
#[must_use]
pub const fn effective_timer_hz(tree_clock_frequency: Option<u32>, cntfrq: u64) -> u64 {
    match tree_clock_frequency {
        Some(hz) if hz != 0 => hz as u64,
        _ => cntfrq,
    }
}

/// First cell of a GIC `interrupts` specifier: the interrupt *type*.
///
/// The Arm GIC device-tree binding (Linux
/// `Documentation/devicetree/bindings/interrupt-controller/arm,gic.yaml`)
/// encodes each interrupt as a three-cell tuple `<type number flags>`.
/// `type` is [`GIC_TYPE_SPI`] (shared peripheral) or [`GIC_TYPE_PPI`]
/// (private peripheral); any other value is not a GICv2 interrupt this
/// port understands and is refused (fail closed).
pub const GIC_TYPE_SPI: u32 = 0;

/// First cell of a GIC `interrupts` specifier naming a private-peripheral
/// interrupt (PPI). See [`GIC_TYPE_SPI`].
pub const GIC_TYPE_PPI: u32 = 1;

/// Lowest GICv2 INTID a device-tree PPI maps to.
///
/// PPIs occupy INTIDs `16..32` (the SGIs below them are software-only and
/// have no device-tree `interrupts` form), so a binding's PPI `number` is
/// offset by this base. The SPI base is [`crate::gic::MIN_SPI_INTID`] —
/// the one definition both this decoder and the GICv2 routing share.
pub const GIC_PPI_INTID_BASE: u32 = 16;

/// Map a GIC `interrupts` specifier `(kind, number)` pair to its global
/// GICv2 INTID.
///
/// `kind` is the first `interrupts` cell ([`GIC_TYPE_SPI`] /
/// [`GIC_TYPE_PPI`]) and `number` the second. An SPI is offset by
/// [`crate::gic::MIN_SPI_INTID`] (32) and a PPI by [`GIC_PPI_INTID_BASE`]
/// (16), matching the kernel Arm GIC binding. Returns `None` — never a
/// guessed line — when:
///
/// * `kind` is neither SPI nor PPI (e.g. a GICv3-only extended-SPI
///   binding this GICv2 port does not drive),
/// * the offset addition overflows, or
/// * the resulting INTID exceeds [`crate::gic::MAX_INTID`] (an SPI
///   `number` the controller cannot address).
#[must_use]
pub fn gic_intid_from_cells(kind: u32, number: u32) -> Option<u32> {
    let intid = match kind {
        GIC_TYPE_SPI => number.checked_add(crate::gic::MIN_SPI_INTID)?,
        GIC_TYPE_PPI => number.checked_add(GIC_PPI_INTID_BASE)?,
        _ => return None,
    };
    (intid <= crate::gic::MAX_INTID).then_some(intid)
}

/// Decode the first GIC interrupt the device `node`'s `interrupts`
/// property names into its global GICv2 INTID.
///
/// Reads the `<type number flags>` triple through the `lib/fdt` cell
/// reader ([`Node::property`] + `read_be_u32`) — never a raw byte poke
/// (: discovery uses the enumerable source only) — and
/// maps it through [`gic_intid_from_cells`]. Returns `None` when the node
/// has no `interrupts` property, the property is shorter than the two
/// cells the decode needs, or the specifier is not a GICv2 SPI/PPI this
/// port can route. The caller (the device-IRQ bring-up in
/// [`crate::platform`] / the kernel binary) then leaves the device
/// unbound rather than guessing a line.
///
/// Only the first interrupt of a multi-interrupt device is decoded; a
/// device that raises several lines is served as further specifiers are
/// needed (no speculative multi-line surface).
#[must_use]
pub fn gic_device_intid(node: &Node<'_>) -> Option<u32> {
    let interrupts = node.property("interrupts")?;
    let kind = interrupts.read_be_u32(0).ok()?;
    let number = interrupts.read_be_u32(4).ok()?;
    gic_intid_from_cells(kind, number)
}

#[cfg(test)]
mod tests {
    use super::{
        debug_component_bases, effective_timer_hz, gic_device_intid, gic_intid_from_cells,
        psci_method, timer_clock_frequency, Fdt, PsciMethod, GIC_TYPE_PPI, GIC_TYPE_SPI,
    };
    use crate::gic::{MAX_INTID, MIN_SPI_INTID};
    use tairix_fdt::fixture::{raspi_like_arm, virt_like_arm, DtbBuilder};

    /// Build a tree with two CPUs and, optionally, a per-CPU
    /// `arm,coresight-cpu-debug` node under a `/soc` bus, so
    /// [`debug_component_bases`] discovery is exercised end to end. Each
    /// entry of `debug` is `(cpu_phandle, base)`; a `soc` bus (address/size
    /// cells 2, identity `ranges`) holds the debug nodes so their `reg` is
    /// translated like a real board's peripheral. CPU affinities are `0` and
    /// `1` with phandles `1` and `2`.
    fn tree_with_debug_nodes(debug: &[(u32, u64)]) -> std::vec::Vec<u8> {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);

        b.begin_node("cpus");
        b.prop_u32("#address-cells", 1);
        b.prop_u32("#size-cells", 0);
        for (i, phandle) in [1u32, 2u32].into_iter().enumerate() {
            b.begin_node(if i == 0 { "cpu@0" } else { "cpu@1" });
            b.prop_str("device_type", "cpu");
            b.prop("reg", &u32::try_from(i).unwrap().to_be_bytes());
            b.prop_u32("phandle", phandle);
            b.end_node();
        }
        b.end_node(); // cpus

        b.begin_node("soc");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.prop("ranges", &[]); // identity: child == parent address space
        for (n, &(cpu_phandle, base)) in debug.iter().enumerate() {
            b.begin_node(if n == 0 { "debug@a" } else { "debug@b" });
            b.prop_str("compatible", "arm,coresight-cpu-debug");
            let mut reg = std::vec::Vec::new();
            reg.extend_from_slice(&base.to_be_bytes()); // 2 address cells
            reg.extend_from_slice(&0x1000u64.to_be_bytes()); // 2 size cells
            b.prop("reg", &reg);
            b.prop_u32("cpu", cpu_phandle);
            b.end_node();
        }
        b.end_node(); // soc

        b.end_node(); // root
        b.build()
    }

    #[test]
    fn discovers_a_per_cpu_debug_base_bound_to_its_cpu() {
        // cpu0 (phandle 1) → debug base A, cpu1 (phandle 2) → debug base B.
        let blob = tree_with_debug_nodes(&[(1, 0xf651_0000), (2, 0xf653_0000)]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let mut out = [0usize; 2];
        debug_component_bases(&fdt, &[0, 1], &mut out);
        assert_eq!(out, [0xf651_0000, 0xf653_0000]);
    }

    #[test]
    fn a_cpu_without_a_debug_node_keeps_its_sentinel() {
        // Only cpu1 (phandle 2) has a debug node; cpu0's slot is left as the
        // caller's "no component" sentinel (0), never fabricated.
        let blob = tree_with_debug_nodes(&[(2, 0xf653_0000)]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let mut out = [0usize; 2];
        debug_component_bases(&fdt, &[0, 1], &mut out);
        assert_eq!(out, [0, 0xf653_0000]);
    }

    #[test]
    fn a_debug_node_for_an_unknown_cpu_is_skipped() {
        // A debug node whose `cpu` phandle names no known CPU (3) writes
        // nothing — fail closed, never a wrong-CPU attribution.
        let blob = tree_with_debug_nodes(&[(3, 0xf651_0000)]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let mut out = [0usize; 2];
        debug_component_bases(&fdt, &[0, 1], &mut out);
        assert_eq!(out, [0, 0]);
    }

    #[test]
    fn a_tree_with_no_debug_nodes_installs_nothing() {
        // The QEMU `virt` / stock Pi 4 shape: no debug components described,
        // so discovery leaves every slot at the sentinel (the sampler then
        // reports Unsupported and the buddy detector runs unchanged).
        let blob = tree_with_debug_nodes(&[]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let mut out = [0usize; 2];
        debug_component_bases(&fdt, &[0, 1], &mut out);
        assert_eq!(out, [0, 0]);
    }

    /// Build a minimal tree carrying a `/timer` node with the given
    /// `clock-frequency` (a single big-endian `u32`), used to exercise
    /// the P4 counter-rate override reader.
    fn tree_with_timer_clock(hz: u32) -> std::vec::Vec<u8> {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("timer");
        b.prop_str("compatible", "arm,armv8-timer");
        b.prop_u32("clock-frequency", hz);
        b.end_node();
        b.end_node();
        b.build()
    }

    #[test]
    fn reads_psci_method_hvc_and_smc() {
        let hvc = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 14);
        let fdt = Fdt::new(&hvc).expect("valid fdt");
        assert_eq!(psci_method(&fdt), Some(PsciMethod::Hvc));

        let smc = virt_like_arm(0x4000_0000, 0x2000_0000, "smc", 14);
        let fdt = Fdt::new(&smc).expect("valid fdt");
        assert_eq!(psci_method(&fdt), Some(PsciMethod::Smc));
    }

    #[test]
    fn reads_psci_method_from_a_raspi_shaped_tree() {
        // The Raspberry-Pi-shaped fixture carries a `/psci` node with the
        // `smc` conduit an EL3-firmware platform uses (`armstub8.bin`).
        let blob = raspi_like_arm(0x7e20_1000, 0x7e21_5040);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(psci_method(&fdt), Some(PsciMethod::Smc));
    }

    #[test]
    fn psci_method_absent_on_a_tree_without_the_node() {
        // A tree with no `/psci` node yields `None`, so the bring-up path
        // fails closed rather than assuming a conduit.
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("timer");
        b.prop_str("compatible", "arm,armv8-timer");
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(psci_method(&fdt), None);
    }

    #[test]
    fn unknown_psci_method_is_rejected() {
        assert_eq!(PsciMethod::from_property(b"nonsense\0"), None);
        assert_eq!(PsciMethod::from_property(b"hvc\0"), Some(PsciMethod::Hvc));
        assert_eq!(PsciMethod::from_property(b"smc"), Some(PsciMethod::Smc));
    }

    #[test]
    fn reads_timer_clock_frequency_when_the_node_declares_it() {
        // The Pi 4's 54 MHz crystal is the evocative value here.
        let blob = tree_with_timer_clock(54_000_000);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(timer_clock_frequency(&fdt), Some(54_000_000));
    }

    #[test]
    fn timer_clock_frequency_absent_on_a_tree_without_the_property() {
        // The `virt`-shaped fixture carries `/timer` `interrupts` but no
        // `clock-frequency`, so the override is absent and the boot path
        // falls back to `CNTFRQ_EL0`.
        let blob = virt_like_arm(0x4000_0000, 0x2000_0000, "hvc", 30);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(timer_clock_frequency(&fdt), None);
    }

    #[test]
    fn effective_timer_hz_prefers_a_nonzero_tree_override() {
        // A present, non-zero tree value wins over the register reading.
        assert_eq!(effective_timer_hz(Some(54_000_000), 62_500_000), 54_000_000);
    }

    #[test]
    fn effective_timer_hz_falls_back_to_the_register() {
        // Absent override → the `CNTFRQ_EL0` value is used.
        assert_eq!(effective_timer_hz(None, 62_500_000), 62_500_000);
        // A zero override is treated as absent (never a 0 Hz timer).
        assert_eq!(effective_timer_hz(Some(0), 62_500_000), 62_500_000);
    }

    /// Build a minimal tree with one device node carrying an `interrupts`
    /// triple `<type number flags>`, then hand that node to `f`.
    fn with_device_interrupts(triple: &[u8], f: impl FnOnce(&super::Node<'_>)) {
        let mut b = DtbBuilder::new();
        b.begin_node("");
        b.prop_u32("#address-cells", 2);
        b.prop_u32("#size-cells", 2);
        b.begin_node("virtio_mmio@a000000");
        b.prop_str("compatible", "virtio,mmio");
        if !triple.is_empty() {
            b.prop("interrupts", triple);
        }
        b.end_node();
        b.end_node();
        let blob = b.build();
        let fdt = Fdt::new(&blob).expect("valid fdt");
        let node = fdt
            .nodes()
            .filter_map(Result::ok)
            .find(|n| n.name() == b"virtio_mmio@a000000")
            .expect("device node");
        f(&node);
    }

    /// Encode a GIC `<type number flags>` interrupt triple as three
    /// big-endian cells, the way a device tree stores it.
    fn interrupts_triple(kind: u32, number: u32, flags: u32) -> std::vec::Vec<u8> {
        let mut v = std::vec::Vec::new();
        v.extend_from_slice(&kind.to_be_bytes());
        v.extend_from_slice(&number.to_be_bytes());
        v.extend_from_slice(&flags.to_be_bytes());
        v
    }

    #[test]
    fn spi_cells_offset_by_the_shared_min_spi_base() {
        // An SPI `number` maps to INTID `number + MIN_SPI_INTID`. SPI 2 is
        // the `virt` board's PL031 RTC (INTID 34), the device the IRQ
        // vertical arms — proving the decoder agrees with the routing.
        assert_eq!(gic_intid_from_cells(GIC_TYPE_SPI, 0), Some(MIN_SPI_INTID));
        assert_eq!(
            gic_intid_from_cells(GIC_TYPE_SPI, 2),
            Some(MIN_SPI_INTID + 2)
        );
    }

    #[test]
    fn ppi_cells_offset_by_the_ppi_base() {
        // A PPI `number` maps to INTID `number + 16`; the generic-timer
        // PPI on `virt` is PPI 14 → INTID 30.
        assert_eq!(gic_intid_from_cells(GIC_TYPE_PPI, 0), Some(16));
        assert_eq!(gic_intid_from_cells(GIC_TYPE_PPI, 14), Some(30));
    }

    #[test]
    fn unknown_interrupt_type_is_refused() {
        // A type cell that is neither SPI (0) nor PPI (1) — e.g. a
        // GICv3-only extended-SPI binding — yields no INTID (fail closed).
        assert_eq!(gic_intid_from_cells(2, 5), None);
        assert_eq!(gic_intid_from_cells(0xFFFF_FFFF, 0), None);
    }

    #[test]
    fn spi_number_above_the_controller_ceiling_is_refused() {
        // An SPI `number` whose INTID would exceed `MAX_INTID` is rejected
        // rather than silently routed to an unaddressable line.
        let over = MAX_INTID - MIN_SPI_INTID + 1;
        assert_eq!(gic_intid_from_cells(GIC_TYPE_SPI, over), None);
        // The exact ceiling is accepted.
        assert_eq!(
            gic_intid_from_cells(GIC_TYPE_SPI, MAX_INTID - MIN_SPI_INTID),
            Some(MAX_INTID)
        );
    }

    #[test]
    fn cell_offset_addition_cannot_overflow() {
        // A hostile `number` near `u32::MAX` must not wrap the addition
        // (a validation bound, fail closed).
        assert_eq!(gic_intid_from_cells(GIC_TYPE_SPI, u32::MAX), None);
        assert_eq!(gic_intid_from_cells(GIC_TYPE_PPI, u32::MAX), None);
    }

    #[test]
    fn device_node_spi_is_decoded_from_its_interrupts_property() {
        // SPI 2, level-high (flags 4) — the `virt` RTC specifier shape.
        let triple = interrupts_triple(GIC_TYPE_SPI, 2, 4);
        with_device_interrupts(&triple, |node| {
            assert_eq!(gic_device_intid(node), Some(MIN_SPI_INTID + 2));
        });
    }

    #[test]
    fn device_node_without_interrupts_yields_none() {
        with_device_interrupts(&[], |node| {
            assert_eq!(gic_device_intid(node), None);
        });
    }

    #[test]
    fn device_node_with_a_truncated_interrupts_property_yields_none() {
        // Only the first cell present: the decode needs two cells, so a
        // short property is refused, never read past its end.
        let mut short = std::vec::Vec::new();
        short.extend_from_slice(&GIC_TYPE_SPI.to_be_bytes());
        with_device_interrupts(&short, |node| {
            assert_eq!(gic_device_intid(node), None);
        });
    }
}
