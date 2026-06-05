//! §17.2 acceptance test: the Arch HAL conformance harness runs.
//!
//! This is the canonical location `plans/WIRING.md` (Stage W0) names —
//! the arch-agnostic suite parameterised over the HAL traits, mirroring
//! `kernel/sched/api/tests/conformance.rs`. The api crate cannot name a
//! concrete port (that would invert the §17.4 layering: ports depend on
//! `kernel/arch/api`, never the reverse — and a port is host-buildable
//! only on its own host), so this integration test drives the harness
//! over a faithful in-test double to prove the suite itself runs. The
//! *real* per-port coverage lives in each `kernel/arch/<target>` crate's
//! own `conformance` host test, which instantiates the same
//! [`rustos_arch_api::conformance`] suite over its real `*Arch`,
//! `SideChannel`, and `MemoryTags` handles.

use core::sync::atomic::{AtomicU64, Ordering};

use rustos_abi::{HwDeviceClass, HwNode, HW_NODE_ROOT};
use rustos_arch_api::conformance;
use rustos_arch_api::memtag::{MemoryTagging, Tagging, TaggingProfile, TAG_COUNT};
use rustos_arch_api::platform::{DiscoveryError, HwNodeSink, PlatformDiscovery};
use rustos_arch_api::sidechannel::{Mitigation, MitigationProfile, SideChannelMitigation};
use rustos_arch_api::{CpuId, SchedulerArch};

#[derive(Default)]
struct DoubleArch {
    ticks: AtomicU64,
}

impl SchedulerArch for DoubleArch {
    fn current_cpu(&self) -> CpuId {
        0
    }

    fn ticks_now(&self) -> u64 {
        self.ticks.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn send_ipi(&self, _target: CpuId) {}
}

struct DoubleSideChannel;

impl SideChannelMitigation for DoubleSideChannel {
    fn profile(&self) -> MitigationProfile {
        MitigationProfile {
            address_space_isolation: Mitigation::Applied,
            syscall_entry_barrier: Mitigation::Applied,
            syscall_exit_barrier: Mitigation::Applied,
            context_switch_buffer_flush: Mitigation::Applied,
            context_switch_indirect_branch_barrier: Mitigation::Applied,
        }
    }
    fn syscall_entry_barrier(&self) {}
    fn syscall_exit_barrier(&self) {}
    fn context_switch_barrier(&self) {}
}

struct DoubleDiscovery;

impl PlatformDiscovery for DoubleDiscovery {
    fn discover(&self, sink: &mut dyn HwNodeSink) -> Result<(), DiscoveryError> {
        sink.emit(HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Root))?;
        sink.emit(HwNode::new(1, 0, HwDeviceClass::Cpu))?;
        Ok(())
    }
}

struct DoubleMemTags;

impl MemoryTagging for DoubleMemTags {
    fn profile(&self) -> TaggingProfile {
        TaggingProfile {
            tag_storage: Tagging::Supported,
            tag_check_faults: Tagging::Supported,
        }
    }
    fn granule_bytes(&self) -> usize {
        16
    }
    fn tag_count(&self) -> u8 {
        TAG_COUNT
    }
}

#[test]
fn arch_hal_conformance_suite_runs() {
    let arch = DoubleArch::default();
    conformance::run_all(&arch, &DoubleSideChannel, &DoubleMemTags, &DoubleDiscovery);
}
