//! Boot-display publication (`plans/DISPLAY.md` D7d).
//!
//! When an architecture port's boot path brings a framebuffer boot
//! console up, the machine has exactly one platform-programmed linear
//! scan-out surface — and until now it existed only as console state, so
//! the user-space display service (`drivers/display/framebuffer`) had no
//! display node to autoload against. This module is the arch-neutral
//! publication step: it turns the port's *discovered* scan-out facts
//! (base, geometry, pixel format — never a board constant) into a
//! [`HwDeviceClass::Display`] hardware-tree node carrying the
//! geometry-carrying [`HwResource::framebuffer`] grant request and the
//! canonical [`SIMPLE_FRAMEBUFFER_COMPATIBLE`] match key the display
//! service's `BIND_KEYS` binds.
//!
//! The node rides the same boot-discovered tree the block and input
//! probes buffer, so the pre-unlock autoload matches it exactly like any
//! other discovered device: the signed store scan finds the display
//! bundle, `lib/devmatch` resolves the node to it, and the spawn path
//! mints exactly one `CAP_MMIO_MAP`-gated surface grant. Seat identity is
//! untouched: the boot display *is* the boot seat's display
//! (`SEAT_PRIMARY` always exists), so no seat is minted here — only a
//! runtime `hw_emit_node` display publication creates a further seat
//! (`plans/DISPLAY.md` D6).

use rustos_abi::driver::display::{DisplayFormat, DisplayMode};
use rustos_abi::hwtree::{FramebufferMemory, HwResource};
use rustos_abi::{
    HwDeviceClass, HwMatchKey, HwNode, HW_NODE_ROOT_ID, SIMPLE_FRAMEBUFFER_COMPATIBLE,
};
use rustos_arch_api::{DiscoveryError, HwNodeSink};

/// Node id of the boot display in the discovered tree.
///
/// One entry in the shared, disjoint-by-construction node-id map
/// ([`crate::hwtree_node_ids`]), so it can never collide with a probe base
/// or another boot step's node. There is at most one boot display, so this
/// is a single id, not a base.
pub use crate::hwtree_node_ids::BOOT_DISPLAY_NODE_ID;

/// The boot console's scan-out surface, as the architecture port
/// discovered it — plain values, so this module never names an
/// arch-specific type (platform specifics stay in the port).
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct BootScanout {
    /// CPU-physical base of the surface.
    pub base: u64,
    /// Surface width in pixels.
    pub width_px: u32,
    /// Surface height in pixels.
    pub height_px: u32,
    /// Distance in bytes between the start of consecutive scanlines.
    pub stride_bytes: u32,
    /// Pixel encoding the platform programmed the surface with.
    pub format: DisplayFormat,
    /// CPU mapping policy required by the discovered surface backing.
    pub memory: FramebufferMemory,
}

/// Emit the boot display node for `scanout` into `sink`.
///
/// The node carries the [`SIMPLE_FRAMEBUFFER_COMPATIBLE`] match key and a
/// single [`HwResource::framebuffer`] capability-grant request sized and
/// validated from the discovered mode — the matched display service is
/// granted exactly the surface window, nothing more (no ambient
/// authority). A degenerate mode (zero extent, an under-sized stride, an
/// overflowing length) is skipped fail-closed: a node the kernel cannot
/// mint a correct, bounded grant for is left unpublished rather than
/// half-described, and the boot proceeds with the display driverless.
///
/// # Errors
///
/// Propagates the sink's [`DiscoveryError`] verbatim (a full bounded sink);
/// the caller leaves the display unpublished on any error (fail closed).
pub fn observe_boot_display(
    scanout: &BootScanout,
    sink: &mut dyn HwNodeSink,
) -> Result<(), DiscoveryError> {
    let mode = DisplayMode {
        width_px: scanout.width_px,
        height_px: scanout.height_px,
        stride_bytes: scanout.stride_bytes,
        format: scanout.format,
    };
    let Ok(resource) = HwResource::framebuffer(scanout.base, &mode, scanout.memory) else {
        return Ok(());
    };
    let Ok(key) = HwMatchKey::compatible(SIMPLE_FRAMEBUFFER_COMPATIBLE) else {
        return Ok(());
    };
    let mut node = HwNode::new(
        BOOT_DISPLAY_NODE_ID,
        HW_NODE_ROOT_ID,
        HwDeviceClass::Display,
    );
    if node.push_match_key(key).is_err() || node.push_resource(resource).is_err() {
        return Ok(());
    }
    sink.emit(node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use rustos_abi::HwResourceKind;

    /// A sink buffering every emitted node, mirroring the boot path's
    /// `boot_hwtree::CollectingHwNodeSink`.
    #[derive(Default)]
    struct CollectingSink {
        nodes: Vec<HwNode>,
    }

    impl HwNodeSink for CollectingSink {
        fn emit(&mut self, node: HwNode) -> Result<(), DiscoveryError> {
            self.nodes.push(node);
            Ok(())
        }
    }

    /// The QEMU `virt` ramfb boot console's discovered shape.
    fn ramfb_scanout() -> BootScanout {
        BootScanout {
            base: 0x4200_0000,
            width_px: 1024,
            height_px: 768,
            stride_bytes: 1024 * 4,
            format: DisplayFormat::Bgra8888,
            memory: FramebufferMemory::WriteBack,
        }
    }

    #[test]
    fn publishes_a_display_node_with_the_framebuffer_grant_and_match_key() {
        let mut sink = CollectingSink::default();
        observe_boot_display(&ramfb_scanout(), &mut sink).expect("sink never fills");
        assert_eq!(sink.nodes.len(), 1);
        let node = &sink.nodes[0];
        assert_eq!(node.id(), BOOT_DISPLAY_NODE_ID);
        assert_eq!(node.parent(), HW_NODE_ROOT_ID);
        assert!(!node.is_root(), "a device node, never skipped as the root");
        assert_eq!(node.class(), Some(HwDeviceClass::Display));
        // The match key is the shared canonical model name, so the display
        // service's BIND_KEYS resolves it.
        assert!(node
            .match_keys()
            .iter()
            .any(|k| k.compatible_bytes() == SIMPLE_FRAMEBUFFER_COMPATIBLE));
        // Exactly one resource: the geometry-carrying, MMIO_MAP-gated
        // surface window, decoding back to the discovered mode.
        assert_eq!(node.resources().len(), 1);
        let resource = node.resources()[0];
        assert_eq!(resource.kind(), Some(HwResourceKind::Framebuffer));
        assert_eq!(resource.base(), 0x4200_0000);
        let mode = resource.framebuffer_mode().expect("mode decodes");
        assert_eq!(mode.width_px, 1024);
        assert_eq!(mode.height_px, 768);
        assert_eq!(mode.stride_bytes, 4096);
        assert_eq!(mode.format, DisplayFormat::Bgra8888);
        assert_eq!(
            resource.framebuffer_memory(),
            Ok(FramebufferMemory::WriteBack)
        );
    }

    #[test]
    fn a_degenerate_mode_publishes_nothing() {
        // Zero extents and an under-sized stride each fail the
        // `HwResource::framebuffer` validation; the node is skipped rather
        // than half-described (fail closed).
        for scanout in [
            BootScanout {
                width_px: 0,
                ..ramfb_scanout()
            },
            BootScanout {
                height_px: 0,
                ..ramfb_scanout()
            },
            BootScanout {
                stride_bytes: 1024 * 4 - 1,
                ..ramfb_scanout()
            },
        ] {
            let mut sink = CollectingSink::default();
            observe_boot_display(&scanout, &mut sink).expect("skip is not an error");
            assert!(sink.nodes.is_empty(), "degenerate mode must not publish");
        }
    }
}
