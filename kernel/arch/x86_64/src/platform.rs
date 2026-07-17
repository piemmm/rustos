//! x86_64 early-boot platform discovery.
//!
//! Implements the Arch HAL
//! [`PlatformDiscovery`](tairix_arch_api::PlatformDiscovery) slice by
//! normalising the
//! ACPI MADT (Multiple APIC Description Table) the firmware exposes into
//! [`tairix_abi::hwtree`] nodes. This is a tracked *move* of the facts the
//! [`crate::acpi`] parser already extracts for SMP bring-up behind the
//! common HAL trait, not a new parser: the boot path
//! used to enumerate the Local APIC ids directly; it now also surfaces
//! them — and the I/O APICs — as a [`tairix_abi::HwNode`] tree.
//!
//! The MADT is pure byte-slice input ([`crate::acpi::Madt::parse`]), so
//! discovery is host-testable without a bare-metal target. Locating the
//! MADT from the RSDP/(R|X)SDT is the firmware-hand-off step the boot path
//! performs before constructing
//! [`AcpiDiscovery`](crate::platform::AcpiDiscovery); this module takes the
//! already-located table bytes.

use crate::acpi::{Madt, MadtEntry};
use tairix_abi::{HwDeviceClass, HwNode, HwResource, HW_NODE_ROOT, HW_NODE_ROOT_ID};
use tairix_arch_api::{DiscoveryError, HwNodeSink, PlatformDiscovery};

/// The MMIO window size of an I/O APIC register block (one 4 KiB page:
/// the index/data register pair lives in the first few bytes, but the
/// whole page is reserved to the controller).
const IOAPIC_WINDOW_LEN: u64 = 0x1000;

/// The `enabled` bit of a Local APIC entry's flags (ACPI 6.5 §5.2.12.2):
/// a processor whose Local APIC is not enabled (and not
/// online-capable) is not brought up.
const LAPIC_FLAG_ENABLED: u32 = 1 << 0;

/// Builds the hardware tree from a located ACPI MADT.
pub struct AcpiDiscovery<'a> {
    madt: &'a [u8],
}

impl<'a> AcpiDiscovery<'a> {
    /// Wrap the bytes of an already-located MADT table.
    #[must_use]
    pub fn new(madt: &'a [u8]) -> Self {
        Self { madt }
    }
}

impl PlatformDiscovery for AcpiDiscovery<'_> {
    fn discover(&self, sink: &mut dyn HwNodeSink) -> Result<(), DiscoveryError> {
        // Root first so every later node's parent is already emitted. Its
        // id is the shared [`HW_NODE_ROOT_ID`]; its parent is the
        // `HW_NODE_ROOT` sentinel (so it alone is `is_root`).
        sink.emit(HwNode::new(
            HW_NODE_ROOT_ID,
            HW_NODE_ROOT,
            HwDeviceClass::Root,
        ))?;
        let madt = Madt::parse(self.madt).map_err(|_| DiscoveryError::MalformedSource)?;

        let mut next_id: u32 = 1;
        for entry in madt.entries() {
            match entry {
                MadtEntry::LocalApic { flags, .. } if flags & LAPIC_FLAG_ENABLED != 0 => {
                    sink.emit(HwNode::new(next_id, HW_NODE_ROOT_ID, HwDeviceClass::Cpu))?;
                    next_id += 1;
                }
                MadtEntry::IoApic { address, .. } => {
                    let mut node =
                        HwNode::new(next_id, HW_NODE_ROOT_ID, HwDeviceClass::InterruptController);
                    node.push_resource(HwResource::mmio(u64::from(address), IOAPIC_WINDOW_LEN))
                        .map_err(|_| DiscoveryError::MalformedSource)?;
                    sink.emit(node)?;
                    next_id += 1;
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::AcpiDiscovery;
    use crate::acpi::tests::build_madt;
    use std::vec::Vec;
    use tairix_abi::{HwDeviceClass, HwNode};
    use tairix_arch_api::platform::{conformance, DiscoveryError, HwNodeSink, PlatformDiscovery};

    /// One enabled Local APIC (a CPU) and one I/O APIC.
    fn sample_madt() -> Vec<u8> {
        // LocalApic: type=0, len=8, uid=0, apic_id=0, flags=1 (enabled).
        let lapic = [0u8, 8, 0, 0, 1, 0, 0, 0];
        // IoApic: type=1, len=12, id=2, reserved=0, addr=0xFEC00000, gsi=0.
        let ioapic = [1u8, 12, 2, 0, 0x00, 0x00, 0xC0, 0xFE, 0, 0, 0, 0];
        let mut entries = Vec::new();
        entries.extend_from_slice(&lapic);
        entries.extend_from_slice(&ioapic);
        build_madt(0xFEE0_0000, 0x1, &entries)
    }

    #[test]
    fn passes_platform_discovery_conformance() {
        let madt = sample_madt();
        let disco = AcpiDiscovery::new(&madt);
        conformance::run(&disco);
    }

    #[derive(Default)]
    struct CountingSink {
        cpus: usize,
        intctrls: usize,
        ioapic_base: u64,
        total: usize,
    }

    impl HwNodeSink for CountingSink {
        fn emit(&mut self, node: HwNode) -> Result<(), DiscoveryError> {
            self.total += 1;
            match node.class() {
                Some(HwDeviceClass::Cpu) => self.cpus += 1,
                Some(HwDeviceClass::InterruptController) => {
                    self.intctrls += 1;
                    if let Some(res) = node.resources().first() {
                        self.ioapic_base = res.base();
                    }
                }
                _ => {}
            }
            Ok(())
        }
    }

    #[test]
    fn emits_cpu_and_ioapic_from_madt() {
        let madt = sample_madt();
        let disco = AcpiDiscovery::new(&madt);
        let mut sink = CountingSink::default();
        disco.discover(&mut sink).expect("discovery succeeds");
        assert_eq!(sink.total, 3, "root + cpu + ioapic");
        assert_eq!(sink.cpus, 1);
        assert_eq!(sink.intctrls, 1);
        assert_eq!(sink.ioapic_base, 0xFEC0_0000);
    }

    #[test]
    fn disabled_cpu_is_skipped() {
        // A Local APIC with flags=0 (neither enabled nor online-capable)
        // is not brought up, so it is not emitted as a CPU node.
        let lapic_off = [0u8, 8, 0, 0, 0, 0, 0, 0];
        let madt = build_madt(0xFEE0_0000, 0x0, &lapic_off);
        let disco = AcpiDiscovery::new(&madt);
        let mut sink = CountingSink::default();
        disco.discover(&mut sink).expect("discovery succeeds");
        assert_eq!(sink.cpus, 0, "a disabled processor is not a CPU node");
        assert_eq!(sink.total, 1, "root only");
    }

    #[test]
    fn malformed_madt_fails_closed() {
        let mut madt = sample_madt();
        madt[0] = b'X'; // corrupt the signature
        let disco = AcpiDiscovery::new(&madt);
        let mut sink = CountingSink::default();
        assert_eq!(
            disco.discover(&mut sink),
            Err(DiscoveryError::MalformedSource)
        );
    }
}
