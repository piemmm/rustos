//! wasm32 early-boot platform discovery.
//!
//! The browser sandbox has no firmware hardware table; its "hardware" is
//! the JavaScript host ("host-environment capability
//! query"). This module implements the Arch HAL
//! [`PlatformDiscovery`](rustos_arch_api::PlatformDiscovery) slice by
//! querying the host for the facts that *do* map onto the
//! hardware tree — the number of logical processors (the Web Worker pool)
//! and whether a display surface exists — and emitting them as
//! [`rustos_abi::hwtree`] nodes.
//!
//! The query itself (`HostCapabilities::query`) is a host call, so it is
//! gated to the wasm target; the normalisation into nodes is pure and
//! host-testable from a [`HostCapabilities`](crate::platform::HostCapabilities)
//! built directly.

use rustos_abi::{HwDeviceClass, HwNode, HW_NODE_ROOT};
use rustos_arch_api::{DiscoveryError, HwNodeSink, PlatformDiscovery};

/// Upper bound on the number of CPU nodes discovery emits.
///
/// Bounds the synthetic tree so a host advertising an implausibly large
/// `hardwareConcurrency` cannot make discovery unbounded (deterministic, bounded work).
const MAX_DISCOVERED_CPUS: u32 = 64;

/// The host-environment capabilities that map onto the hardware tree.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct HostCapabilities {
    /// Logical processors the host advertises (the Web Worker pool size).
    /// Always at least `1` (the boot worker).
    pub logical_processors: u32,
    /// Whether the host exposes a display surface (a canvas).
    pub has_display: bool,
}

impl HostCapabilities {
    /// Build a capability set, clamping the processor count to at least
    /// one and at most an internal bound (64) on the synthetic CPU count.
    #[must_use]
    pub fn new(logical_processors: u32, has_display: bool) -> Self {
        Self {
            logical_processors: logical_processors.clamp(1, MAX_DISCOVERED_CPUS),
            has_display,
        }
    }

    /// Query the JavaScript host for its capabilities.
    #[cfg(target_arch = "wasm32")]
    #[must_use]
    pub fn query() -> Self {
        Self::new(
            crate::bindings::host_logical_processors(),
            crate::bindings::host_has_display(),
        )
    }
}

/// Builds the hardware tree from a host-capability query.
pub struct HostCapabilityDiscovery {
    caps: HostCapabilities,
}

impl HostCapabilityDiscovery {
    /// Wrap a capability set.
    #[must_use]
    pub fn new(caps: HostCapabilities) -> Self {
        Self { caps }
    }
}

impl PlatformDiscovery for HostCapabilityDiscovery {
    fn discover(&self, sink: &mut dyn HwNodeSink) -> Result<(), DiscoveryError> {
        // Root first so every later node's parent is already emitted.
        sink.emit(HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Root))?;
        let mut next_id: u32 = 1;

        // One CPU node per advertised logical processor (the worker pool).
        for _ in 0..self.caps.logical_processors {
            sink.emit(HwNode::new(next_id, 0, HwDeviceClass::Cpu))?;
            next_id += 1;
        }

        if self.caps.has_display {
            sink.emit(HwNode::new(next_id, 0, HwDeviceClass::Display))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{HostCapabilities, HostCapabilityDiscovery, MAX_DISCOVERED_CPUS};
    use rustos_abi::HwDeviceClass;
    use rustos_abi::HwNode;
    use rustos_arch_api::platform::{conformance, DiscoveryError, HwNodeSink, PlatformDiscovery};

    #[test]
    fn capabilities_clamp_processor_count() {
        assert_eq!(HostCapabilities::new(0, false).logical_processors, 1);
        assert_eq!(
            HostCapabilities::new(10_000, false).logical_processors,
            MAX_DISCOVERED_CPUS
        );
        assert_eq!(HostCapabilities::new(4, true).logical_processors, 4);
    }

    #[test]
    fn passes_platform_discovery_conformance() {
        let disco = HostCapabilityDiscovery::new(HostCapabilities::new(4, true));
        conformance::run(&disco);
        // A single-worker, headless host is also conformant.
        let headless = HostCapabilityDiscovery::new(HostCapabilities::new(1, false));
        conformance::run(&headless);
    }

    #[derive(Default)]
    struct CountingSink {
        cpus: usize,
        displays: usize,
        total: usize,
    }

    impl HwNodeSink for CountingSink {
        fn emit(&mut self, node: HwNode) -> Result<(), DiscoveryError> {
            self.total += 1;
            match node.class() {
                Some(HwDeviceClass::Cpu) => self.cpus += 1,
                Some(HwDeviceClass::Display) => self.displays += 1,
                _ => {}
            }
            Ok(())
        }
    }

    #[test]
    fn emits_one_cpu_per_processor_and_optional_display() {
        let disco = HostCapabilityDiscovery::new(HostCapabilities::new(4, true));
        let mut sink = CountingSink::default();
        disco.discover(&mut sink).expect("discovery succeeds");
        assert_eq!(sink.cpus, 4);
        assert_eq!(sink.displays, 1);
        assert_eq!(sink.total, 6, "root + 4 cpus + display");

        let headless = HostCapabilityDiscovery::new(HostCapabilities::new(2, false));
        let mut sink = CountingSink::default();
        headless.discover(&mut sink).expect("discovery succeeds");
        assert_eq!(sink.cpus, 2);
        assert_eq!(sink.displays, 0);
        assert_eq!(sink.total, 3, "root + 2 cpus");
    }
}
