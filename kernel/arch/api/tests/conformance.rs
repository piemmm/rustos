//! acceptance test: the Arch HAL conformance harness runs.
//!
//! This is the canonical location `plans/WIRING.md` (Stage W0) names —
//! the arch-agnostic suite parameterised over the HAL traits, mirroring
//! `kernel/sched/api/tests/conformance.rs`. The api crate cannot name a
//! concrete port (that would invert the layering: ports depend on
//! `kernel/arch/api`, never the reverse — and a port is host-buildable
//! only on its own host), so this integration test drives the harness
//! over a faithful in-test double to prove the suite itself runs. The
//! *real* per-port coverage lives in each `kernel/arch/<target>` crate's
//! own `conformance` host test, which instantiates the same
//! [`tairix_arch_api::conformance`] suite over its real `*Arch`,
//! `SideChannel`, and `MemoryTags` handles.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use tairix_abi::{HwDeviceClass, HwNode, HW_NODE_ROOT};
use tairix_arch_api::conformance;
use tairix_arch_api::context::{
    self, ContextSwitch, KernelStackRegion, PrepareError, TaskContext, TaskEntry,
};
use tairix_arch_api::cpufeatures::{CpuFeatures, FeatureProfile, FeatureSupport};
use tairix_arch_api::memtag::{MemoryTagging, Tagging, TaggingProfile, TAG_COUNT};
use tairix_arch_api::percpu;
use tairix_arch_api::platform::{DiscoveryError, HwNodeSink, PlatformDiscovery};
use tairix_arch_api::sidechannel::{Mitigation, MitigationProfile, SideChannelMitigation};
use tairix_arch_api::timer::{self, TickFn, Timer};
use tairix_arch_api::{CoreType, CpuFeatureSet};
use tairix_arch_api::{CpuId, PerCpu, SchedulerArch};
use tairix_sync::FnCell;

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

    fn quantum_ticks(&self) -> u64 {
        1
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

#[derive(Default)]
struct DoublePerCpu {
    base: AtomicUsize,
}

impl PerCpu for DoublePerCpu {
    fn read_self_base(&self) -> usize {
        self.base.load(Ordering::Relaxed)
    }
    unsafe fn write_self_base(&self, base: usize) {
        self.base.store(base, Ordering::Relaxed);
    }
}

struct DoubleCpuFeatures;

impl CpuFeatures for DoubleCpuFeatures {
    fn detect(&self, _cpu: CpuId) -> CpuFeatureSet {
        CpuFeatureSet::EMPTY
    }
    fn core_type(&self, _cpu: CpuId) -> CoreType {
        CoreType::UNKNOWN
    }
    fn profile(&self) -> FeatureProfile {
        FeatureProfile {
            isa_features: FeatureSupport::Supported,
            core_identity: FeatureSupport::Supported,
        }
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
    conformance::run_all(
        &arch,
        &DoubleSideChannel,
        &DoubleMemTags,
        &DoubleDiscovery,
        &DoublePerCpu::default(),
        &DoubleCpuFeatures,
    );
}

#[test]
fn per_cpu_isolation_vertical_runs_over_two_handles() {
    percpu::conformance::run_isolation(&DoublePerCpu::default(), &DoublePerCpu::default());
}

#[derive(Default)]
struct DoubleTimer {
    callback: FnCell<TickFn>,
}

impl Timer for DoubleTimer {
    fn set_tick_callback(&self, callback: TickFn) {
        self.callback.install(callback);
    }
    fn tick_callback(&self) -> Option<TickFn> {
        self.callback.load()
    }
    fn dispatch_tick(&self, cpu: CpuId) -> bool {
        match self.tick_callback() {
            Some(cb) => {
                cb(cpu);
                true
            }
            None => false,
        }
    }
    fn arm_oneshot(&self, _ticks_from_now: u64) {}
    fn disarm(&self) {}
}

#[test]
fn timer_vertical_runs_over_a_handle() {
    timer::conformance::run_all(&DoubleTimer::default());
}

/// A faithful in-test [`ContextSwitch`] double: it honours the
/// fail-closed `prepare` contract and seeds a plausible frame. `switch`
/// is the bare-metal-only operation and is never exercised on the host,
/// so its body is empty (the suite calls only `prepare`).
struct DoubleContextSwitch;

/// A frame size below the conformance stack but above the too-small probe.
const DOUBLE_FRAME_BYTES: usize = 64;

impl ContextSwitch for DoubleContextSwitch {
    fn prepare(
        &self,
        ctx: &mut TaskContext,
        stack: KernelStackRegion,
        _entry: TaskEntry,
        _arg: usize,
    ) -> Result<(), PrepareError> {
        let frame = stack.seed_frame(DOUBLE_FRAME_BYTES)?;
        ctx.stack_pointer = frame.addr().get() as u64;
        Ok(())
    }

    unsafe fn switch(&self, _prev: *mut TaskContext, _next: *mut TaskContext) {}
}

#[test]
fn context_switch_vertical_runs_over_a_handle() {
    context::conformance::run_all(&DoubleContextSwitch);
    let dynamic: &dyn ContextSwitch = &DoubleContextSwitch;
    context::conformance::run_all(dynamic);
}
