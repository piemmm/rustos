//! riscv64 device-tree access.
//!
//! The flattened-device-tree parser itself is architecture-neutral and
//! lives once in [`rustos_fdt`] (no duplication); this
//! module re-exports it so the riscv64 boot path and the QEMU integration
//! tests keep naming `rustos_arch_riscv64::fdt::Fdt`. The riscv64-specific
//! normalisation of the tree into [`rustos_abi::hwtree`] nodes lives in
//! [`crate::platform`].
//!
//! It also carries the riscv64-specific decode of a device's **PLIC**
//! interrupt line from its device-tree `interrupts` cell — the riscv64
//! analogue of the aarch64 port's `gic_device_intid`. The QEMU `virt`
//! board declares `#interrupt-cells = <1>` on the PLIC, so a device names
//! its PLIC source number directly in a single `interrupts` cell (there is
//! no GIC-style `<type number flags>` triple). The bootstrap-floor
//! virtio-MMIO discovery walks
//! ([`rustos_kernel::hwdiscovery`](../../rustos_kernel/hwdiscovery/index.html))
//! feed this decode as their per-slot `slot_irq` resolver, so an emitted
//! device node carries the PLIC line its interrupt-driven user-space driver
//! parks on — a discovered value, never a board constant.

pub use rustos_fdt::{Fdt, FdtError};

/// PLIC interrupt source `0` is the reserved "no interrupt" sentinel: a
/// device whose `interrupts` cell is `0` routes to no line. The RISC-V
/// PLIC numbers real sources from `1`.
pub const PLIC_SOURCE_NONE: u32 = 0;

/// Is `source` a routable PLIC interrupt line for a controller with
/// `ndev` sources?
///
/// A source is routable iff it is neither the [`PLIC_SOURCE_NONE`]
/// sentinel nor above the controller's discovered source count `ndev`
/// (PLIC sources are numbered `1..=ndev`). Everything else is refused so
/// a device is never bound to a line the controller cannot raise (fail
/// closed).
#[must_use]
pub fn plic_source_in_range(source: u32, ndev: u32) -> bool {
    source != PLIC_SOURCE_NONE && source <= ndev
}

/// The PLIC's `riscv,ndev` source count, read from the first PLIC node the
/// tree describes (`riscv,plic0` / `sifive,plic-1.0.0`).
///
/// [`None`] when the tree describes no PLIC node or its node carries no
/// readable `riscv,ndev` — the caller then falls back to the nonzero-only
/// check ([`plic_source_in_range`] is skipped), leaving the controller's
/// own arm-time range check as the backstop.
#[must_use]
pub fn plic_ndev(fdt: &Fdt<'_>) -> Option<u32> {
    for node in fdt.nodes() {
        let node = node.ok()?;
        if node.is_compatible("riscv,plic0") || node.is_compatible("sifive,plic-1.0.0") {
            return node.property("riscv,ndev")?.read_be_u32(0).ok();
        }
    }
    None
}

/// The physical base of the PLIC register block, read from the first PLIC
/// node the tree describes (`riscv,plic0` / `sifive,plic-1.0.0`).
///
/// The `install_irq_dispatch` PLIC path maps the controller's registers at
/// this discovered address — a value read from the firmware tree, never a
/// board constant. [`None`] when the tree describes no PLIC node or its node
/// carries no readable `reg` base (the caller then wires no external-IRQ
/// dispatch and interrupt-driven bring-up fails closed).
#[must_use]
pub fn plic_base(fdt: &Fdt<'_>) -> Option<u64> {
    for node in fdt.nodes() {
        let node = node.ok()?;
        if node.is_compatible("riscv,plic0") || node.is_compatible("sifive,plic-1.0.0") {
            return node.property("reg")?.read_be_u64(0).ok();
        }
    }
    None
}

/// Decode the PLIC interrupt source of the `virtio,mmio` node whose `reg`
/// base equals `slot_base`.
///
/// This is the riscv64 counterpart of the aarch64 port's `device_spi` /
/// `gic_device_intid`: it finds the `virtio,mmio` slot at `slot_base`,
/// reads the single `interrupts` cell (the QEMU `virt` PLIC uses
/// `#interrupt-cells = <1>`, so the cell *is* the PLIC source number —
/// never a raw byte poke: discovery uses the enumerable source only), and
/// validates it against the PLIC's discovered [`plic_ndev`] source count.
///
/// Returns [`None`] — never a guessed line — when no `virtio,mmio` node
/// sits at `slot_base`, the matched node has no readable `interrupts`
/// cell, the cell is the [`PLIC_SOURCE_NONE`] sentinel, or (when the PLIC
/// source count is known) the source is out of range. The discovery walk's
/// `slot_irq` resolver then leaves that slot undiscovered rather than
/// emitting a device node without the interrupt line its driver parks on
/// (fail closed).
#[must_use]
pub fn plic_device_source(fdt: &Fdt<'_>, slot_base: u64) -> Option<u32> {
    let ndev = plic_ndev(fdt);
    for node in fdt.nodes() {
        let node = node.ok()?;
        if !node.is_compatible("virtio,mmio") {
            continue;
        }
        let reg = node.property("reg")?;
        if reg.read_be_u64(0).ok()? != slot_base {
            continue;
        }
        let source = node.property("interrupts")?.read_be_u32(0).ok()?;
        return match ndev {
            Some(ndev) => plic_source_in_range(source, ndev).then_some(source),
            // No PLIC source count discovered: accept any non-sentinel
            // source and defer the upper-bound check to the controller's
            // own arm-time range guard.
            None => (source != PLIC_SOURCE_NONE).then_some(source),
        };
    }
    None
}

#[cfg(test)]
pub(crate) mod tests {
    //! Re-export of the shared DTB test fixtures so the `platform`
    //! discovery tests and the conformance handle drive the same builder
    //! as the parser's own tests. Enabled by the
    //! `rustos-fdt/test-fixtures` feature this crate turns on in its
    //! `[dev-dependencies]`.
    pub(crate) use rustos_fdt::fixture::{virt_like, virt_like_with_virtio};

    use super::{
        plic_base, plic_device_source, plic_ndev, plic_source_in_range, Fdt, PLIC_SOURCE_NONE,
    };

    /// A `virt`-shaped tree with the given virtio-MMIO slots and PLIC
    /// source count, ready for the resolver assertions.
    fn tree_with(ndev: u32, slots: &[(u64, u32)]) -> std::vec::Vec<u8> {
        virt_like_with_virtio(0x8000_0000, 0x1000_0000, 10_000_000, ndev, slots)
    }

    #[test]
    fn resolves_a_slots_declared_plic_source() {
        // The `virtio,mmio` slot at 0x1000_1000 declares PLIC source 1, so
        // the resolver returns exactly that discovered line.
        let blob = tree_with(96, &[(0x1000_1000, 1), (0x1000_2000, 2)]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(plic_device_source(&fdt, 0x1000_1000), Some(1));
        assert_eq!(plic_device_source(&fdt, 0x1000_2000), Some(2));
    }

    #[test]
    fn an_unknown_slot_base_resolves_to_none() {
        // No `virtio,mmio` node sits at the queried base, so there is no
        // line to return.
        let blob = tree_with(96, &[(0x1000_1000, 1)]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(plic_device_source(&fdt, 0xDEAD_0000), None);
    }

    #[test]
    fn the_no_source_sentinel_is_rejected() {
        // A slot whose `interrupts` cell is the PLIC "no interrupt"
        // sentinel routes to no line, so it is refused rather than bound to
        // source 0 (fail closed).
        let blob = tree_with(96, &[(0x1000_1000, PLIC_SOURCE_NONE)]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(plic_device_source(&fdt, 0x1000_1000), None);
    }

    #[test]
    fn a_source_above_ndev_is_rejected() {
        // A source the controller cannot raise (above its discovered
        // `riscv,ndev` count) is refused, never guessed.
        let blob = tree_with(4, &[(0x1000_1000, 5)]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(plic_device_source(&fdt, 0x1000_1000), None);
        // The exact-boundary source is accepted.
        let blob = tree_with(5, &[(0x1000_1000, 5)]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(plic_device_source(&fdt, 0x1000_1000), Some(5));
    }

    #[test]
    fn plic_ndev_reads_the_controller_source_count() {
        let blob = tree_with(53, &[(0x1000_1000, 1)]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(plic_ndev(&fdt), Some(53));
    }

    #[test]
    fn plic_base_reads_the_controller_register_base() {
        // The `virt`-shaped fixture places the PLIC at `plic@c000000`, so the
        // resolver reads exactly that discovered register base.
        let blob = tree_with(53, &[(0x1000_1000, 1)]);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(plic_base(&fdt), Some(0x0c00_0000));
    }

    #[test]
    fn plic_base_on_a_tree_without_a_plic_is_none() {
        // A bare board (no PLIC node) yields no base, so the caller wires no
        // external-IRQ dispatch rather than guessing an address (fail closed).
        let blob = virt_like(0x8000_0000, 0x1000_0000, 10_000_000);
        let fdt = Fdt::new(&blob).expect("valid fdt");
        assert_eq!(plic_base(&fdt), None);
    }

    #[test]
    fn source_range_boundaries() {
        // 0 is the sentinel (out), 1..=ndev in, ndev+1 out.
        assert!(!plic_source_in_range(0, 8));
        assert!(plic_source_in_range(1, 8));
        assert!(plic_source_in_range(8, 8));
        assert!(!plic_source_in_range(9, 8));
    }
}
