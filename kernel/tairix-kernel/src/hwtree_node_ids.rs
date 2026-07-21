//! Reserved synthetic hardware-tree node-id address space.
//!
//! Several boot-path steps mint [`HwNode`] ids that are *not* the firmware
//! discovery ids (`platform::FdtDiscovery` numbers those from `1` upward).
//! Each such step — every bootstrap-floor virtio-MMIO class probe
//! ([`crate::hwdiscovery`]) and the boot-display publication shim
//! ([`crate::boot_display`]) — numbers its emitted nodes from a distinct
//! high base so their ids stay obviously disjoint: two nodes discovered on
//! the same bus must never share an id, or one would silently overwrite the
//! other in the leaked hardware tree (a display world with a NIC hits
//! exactly this).
//!
//! Those bases used to be hand-picked literals kept apart only by prose,
//! and a base once collided with the boot-display id in production. This
//! module makes the whole scheme correct *by construction*: every base is
//! `origin + index * stride`, so distinct indices cannot alias, and a
//! compile-time guard proves a single probe walk (one id per enumerated bus
//! slot, at most [`MAX_SLOTS`]) can never run off the end of its region
//! into the next. Adding a region is claiming the next index here — never a
//! fresh literal elsewhere.
//!
//! [`HwNode`]: tairix_abi::HwNode

use tairix_kernel_virtio::MAX_SLOTS;

/// First id of the reserved synthetic node-id space. Chosen far above the
/// firmware discovery ids (which start at `1`) so a synthetic id is always
/// recognisable as such.
pub const HW_NODE_PROBE_ORIGIN: u32 = 0x8000_0000;

/// Width of each reserved node-id region. A probe walk emits one id per
/// enumerated bus slot, incrementing from its region base, so the region
/// must be wider than the most slots a bus can present ([`MAX_SLOTS`]); the
/// region spacing keeps consecutive regions from ever overlapping.
pub const HW_NODE_PROBE_REGION_STRIDE: u32 = 0x0001_0000;

/// The base id of the reserved node-id region with the given `index`.
///
/// Regions are [`HW_NODE_PROBE_REGION_STRIDE`]-spaced from
/// [`HW_NODE_PROBE_ORIGIN`], so distinct indices yield disjoint,
/// non-overlapping ranges by construction. The named bases below are the
/// claimed indices; a new boot step claims the next free one through this.
#[must_use]
pub const fn region(index: u32) -> u32 {
    HW_NODE_PROBE_ORIGIN + index * HW_NODE_PROBE_REGION_STRIDE
}

/// First synthetic id for a probed virtio-MMIO **block** child node
/// ([`crate::hwdiscovery::observe_virtio_mmio_block_devices`]). One id per
/// enumerated block slot, so distinct disks stay distinct.
pub const VIRTIO_BLOCK_PROBE_NODE_BASE_ID: u32 = region(0);

/// First synthetic id for a probed virtio-MMIO **input** child node
/// ([`crate::hwdiscovery::observe_virtio_mmio_input_devices`]). One id per
/// enumerated input slot, so distinct devices stay distinct.
pub const VIRTIO_INPUT_PROBE_NODE_BASE_ID: u32 = region(1);

/// The id of the published boot-display node ([`crate::boot_display`]).
/// A single node (there is one boot framebuffer), so it needs only the one
/// id at its region base.
pub const BOOT_DISPLAY_NODE_ID: u32 = region(2);

/// First synthetic id for a probed virtio-MMIO **network** child node
/// ([`crate::hwdiscovery::observe_virtio_mmio_network_devices`]). One id
/// per enumerated network slot, so distinct NICs stay distinct.
pub const VIRTIO_NET_PROBE_NODE_BASE_ID: u32 = region(3);

/// First synthetic id for a probed virtio-**PCI** network child node
/// ([`crate::hwdiscovery::observe_virtio_pci_network_devices`]). One id
/// per enumerated virtio-net PCI function; a distinct region from the
/// MMIO network base so a machine that probed both buses (it will not,
/// but the map is disjoint by construction regardless) keeps every NIC
/// node id unambiguous.
pub const VIRTIO_PCI_NET_PROBE_NODE_BASE_ID: u32 = region(4);

/// First synthetic id for a probed virtio-**PCI** block child node
/// ([`crate::hwdiscovery::observe_virtio_pci_block_devices`]). One id per
/// enumerated virtio-blk PCI function; a distinct region from the MMIO
/// block base so a port that probes the PCI storage bus (x86_64) keeps
/// every disk node id unambiguous and never aliases an MMIO-probed disk.
pub const VIRTIO_PCI_BLOCK_PROBE_NODE_BASE_ID: u32 = region(5);

// A single probe walk emits at most one id per enumerated bus slot
// (`bus.enumerate` fills at most `MAX_SLOTS`; an overfull bus fails closed),
// so the highest id a walk can reach in its region is
// `base + (MAX_SLOTS - 1)`. Requiring `MAX_SLOTS <= stride` proves that id
// never crosses into the next region — the guard that makes the derived
// bases sufficient, not merely disjoint.
const _: () = assert!(
    MAX_SLOTS <= HW_NODE_PROBE_REGION_STRIDE as usize,
    "a probe walk must not overrun its node-id region into the next"
);

#[cfg(test)]
mod tests {
    use super::*;

    /// Every reserved base, in region order, for the disjointness sweep.
    const BASES: [u32; 6] = [
        VIRTIO_BLOCK_PROBE_NODE_BASE_ID,
        VIRTIO_INPUT_PROBE_NODE_BASE_ID,
        BOOT_DISPLAY_NODE_ID,
        VIRTIO_NET_PROBE_NODE_BASE_ID,
        VIRTIO_PCI_NET_PROBE_NODE_BASE_ID,
        VIRTIO_PCI_BLOCK_PROBE_NODE_BASE_ID,
    ];

    #[test]
    fn every_reserved_region_is_disjoint_from_every_other() {
        // The whole point of the shared map: no two regions may overlap, so
        // no node one step mints can alias a node another step minted. A
        // walk occupies `[base, base + stride)`; two regions are disjoint
        // iff their bases differ by at least a stride.
        for (i, &a) in BASES.iter().enumerate() {
            for &b in &BASES[i + 1..] {
                assert!(
                    a.abs_diff(b) >= HW_NODE_PROBE_REGION_STRIDE,
                    "regions at {a:#x} and {b:#x} overlap"
                );
            }
        }
    }

    #[test]
    fn regions_are_stride_spaced_from_the_origin() {
        assert_eq!(BASES[0], HW_NODE_PROBE_ORIGIN);
        for pair in BASES.windows(2) {
            assert_eq!(pair[1] - pair[0], HW_NODE_PROBE_REGION_STRIDE);
        }
    }

    #[test]
    fn a_full_walk_stays_within_its_region() {
        // The compile-time guard restated as a runtime check: the last id a
        // maximal walk mints is below the next region's base.
        let last = VIRTIO_BLOCK_PROBE_NODE_BASE_ID as usize + (MAX_SLOTS - 1);
        assert!(last < VIRTIO_INPUT_PROBE_NODE_BASE_ID as usize);
    }
}
