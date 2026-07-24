//! TAIRiX microkernel core (Stage 2.6 of `PLAN.md`).
//!
//! `kernel/core` ships the **architecture-neutral kernel entry point,
//! init sequencing, and panic policy**. Architecture crates
//! (`kernel/arch/*`, Stage 3) build a [`BootInfo`] from whatever
//! protocol the platform exposes (multiboot2, UEFI, DTB,
//! `wasm-bindgen`, …) and hand it to [`kernel_main`]; everything else
//! in this crate exists to support that hand-off.
//!
//! # Module map
//!
//! | Module          | Role                                                   |
//! | --------------- | ------------------------------------------------------ |
//! | [`aspace`]      | Per-task address-space registry for the copy path.     |
//! | [`audit`]       | Stable audit-event IDs in the `4_000..5_000` range.    |
//! | [`bootinfo`]    | [`BootInfo`] hand-off type + [`KernelArch`] trait.     |
//! | [`init`]        | [`kernel_main`] and the documented init order.         |
//! | [`mod@panic`]   | [`handle_panic`] and [`PanicContext`].                 |
//!
//! # Init order
//!
//! [`kernel_main`] runs the following phases in this exact order:
//!
//! 1. **log** — install the global level filter.
//! 2. **mem** — build the physical [`tairix_kernel_mem::FrameAllocator`].
//! 3. **sec** — verify the bootstrap [`tairix_kernel_sec::IdentityTable`].
//! 4. **sched** — construct the SMP [`crate::sched::Scheduler`] (the
//!    build-time-selected scheduler policy).
//! 5. **ipc** — `kernel/ipc` has no global state at this stage; the
//!    phase event still fires so external log consumers see a uniform
//!    boot timeline.
//!
//! Stage 2.7 will extend the chain with a syscall-registration phase
//! and replace the trailing [`KernelArch::halt`] with the scheduler
//! dispatch loop.
//!
//! # Panic policy
//!
//! The arch port owns the `#[panic_handler]` attribute and delegates
//! to [`handle_panic`] (see the example in [`mod@panic`] module docs).
//! The handler logs one [`audit::AuditEvent::Panic`] record carrying
//! the failing CPU id, file, line, and column, then calls
//! [`KernelArch::halt`]. It **never** silently resets / Stage 2 deliverables.
//!
//! # No global mutable static
//!
//! Per, *"No global mutable static beyond the per-CPU
//! bootstrap area"*. `kernel/core` declares **zero** global mutable
//! statics: every subsystem the kernel constructs lives on
//! [`kernel_main`]'s stack inside `KernelState`, which never escapes
//! the entry function. The per-CPU bootstrap area itself lives in the
//! arch crates (`kernel/arch/*`, Stage 3) and is reviewed there.
//!
//! # Documentation
//!
//! Architectural detail (entry contract, init order, panic policy,
//! `BootInfo` schema) is in `docs/src/architecture/kernel.md`. Public
//! items here carry rustdoc.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

// Host tests need `std` for `Box::leak`, `catch_unwind`, and
// `String::from_utf8`. The crate itself remains `no_std` for
// production builds (no hacks).
#[cfg(test)]
extern crate std;

pub mod appspawn;
pub mod aspace;
pub mod audit;
pub mod blockwait;
pub mod boot_id;
pub mod bootinfo;
pub mod cache_control;
pub mod callreg;
pub mod console;
mod cpu_state;
pub mod crash;
pub mod devres;
pub mod dispatch_slot;
pub mod driver_store;
pub mod filemap;
pub mod fs;
pub mod fswatch;
pub mod groups;
pub mod hwtree;
pub mod init;
pub mod introspect;
pub mod introspect_source;
pub mod kheap;
pub mod kthread;
pub mod kthread_irq;
pub mod launch_cache;
pub mod live_producer;
pub mod loadavg;
pub mod memmap;
pub mod memstats;
pub mod memtest;
pub mod panic;
pub mod pipe;
pub mod preempt;
pub mod proc_id;
pub mod procsignal;
pub mod procwait;
pub mod random;
pub mod resource;
pub mod rlimit;
pub mod seat;
pub mod sharedreg;
pub mod sleeplock;
// The single scheduler selection point. Internal:
// the concrete policy must not leak to crates that should depend on the
// `tairix_kernel_sched_api` contract instead.
pub(crate) mod sched;
pub mod smp;
pub mod spawn;
pub mod spawn_services;
pub mod syscalls;
pub mod syscfg;
pub mod useradmin;
pub mod users;
pub mod waitq;
pub mod waitset;
pub mod wallclock;
pub mod watchdog;

#[cfg(any(test, feature = "test-arch"))]
pub mod test_arch;
// The unit-test binary's counting global allocator: per-measurement net
// live-byte balances for the host-side leak soaks (`plans/APPS.md` I3).
// `cfg(test)` only — never a production or integration-test allocator.
#[cfg(test)]
pub(crate) mod test_alloc;
// Shared host-test fixtures for the on-disk bundle spawn path (an
// in-memory filesystem + the signed-bundle composer), used by both the
// `appspawn` unit tests and the `spawn` syscall-handler tests so the fake
// volume is defined once.
#[cfg(test)]
pub(crate) mod test_bundle;
// Shared host-test fixture for the reclaimable-cache pressure gauge (a
// controllable free-memory source plus per-band readings), used by the
// filesystem-cache, launch-cache, and cross-cache integration suites so
// the gauge scaffolding is defined once.
#[cfg(test)]
pub(crate) mod test_pressure;
#[cfg(any(test, feature = "test-arch"))]
pub mod test_sink;

// Cross-cache integration tests for the reclaimable-memory services
// (`plans/SMARTRAM.md` SMART10): one shared pressure gauge driving the
// filesystem cache and the launch cache together, the combined `ramzip`
// handoff arithmetic, the thrash scenario, and the work-avoided
// benchmark evidence.
#[cfg(test)]
mod reclaim_integration_tests;

pub use appspawn::{
    app_error_errno, bundle_run_path, AnchorVerifier, AppStore, BundleRunPath, FsBundleStore,
};
pub use aspace::{AddressSpaceRegistry, AspaceError, StackSpan};
pub use audit::AuditEvent;
pub use blockwait::{FallbackPark, IrqParkWaiter};
pub use bootinfo::{BootInfo, BootInfoError, IrqRouting, KernelArch, MAX_COMMAND_LINE_BYTES};
pub use cache_control::{CacheClass, CacheControl, CacheMode, CACHE_CONTROL};
pub use console::{
    BlockingConsoleRead, ConsoleDevice, ConsoleInput, ConsoleInputQueue, ConsoleRead, ConsoleWrite,
    NullConsole, NullConsoleInput, NullConsoleRead, SecretFeedback, CONSOLE_INPUT_QUEUE_CAPACITY,
    NO_CONSOLES, NULL_CONSOLE, NULL_CONSOLE_INPUT, NULL_CONSOLE_READ,
};
pub use cpu_state::{install as initialize_cpu_state, CpuStateInitError};
pub use devres::{
    dma_constraint, mappable_subwindow, translate_device_addr, DmaAllocFacility, DmaCarve,
    DmaConstraint, MmioMapFacility, MsiAllocFacility, NullDmaAllocFacility, NullMmioMapFacility,
    NullMsiAllocFacility, NullSharedMemFacility, SharedMemFacility, NULL_DMA_ALLOC_FACILITY,
    NULL_MMIO_MAP_FACILITY, NULL_MSI_ALLOC_FACILITY, NULL_SHARED_MEM_FACILITY,
};
pub use dispatch_slot::{
    AlreadyInstalledError, DispatchCallbackSlot, DispatchHook, DispatchOutcome, RescheduleAction,
    UserFaultOutcome,
};
pub use driver_store::{
    enumerate_driver_store, DriverImageError, DriverImageReader, DRIVER_STORE_PATH,
    MAX_DRIVER_IMAGE_LEN, MAX_STORE_DEPTH, MAX_STORE_DRIVERS, SYSTEM_VOLUME_STORE_PATH,
};
pub use filemap::{FileMap, NullFileMap, NULL_FILE_MAP};
pub use fs::{
    Access, AclEntry, AclWho, CachedFs, Credentials, FilesystemAlreadyInstalled, FilesystemService,
    IdentityAlreadyInstalled, LateFilesystem, LateIdentity, Metadata, Mode, MountPoint, MountTable,
    MountedFilesystemService, NullFilesystemService, Path, Vfs, VfsError, VolumeForest,
    VolumePublishError, VolumeService, NULL_FILESYSTEM, NULL_VOLUME_FOREST, NULL_VOLUME_SERVICE,
};
pub use groups::{
    build_identity_table, load_groups_db, system_identity_table, GroupsLoadError, GROUPS_DB_PATH,
};
pub use hwtree::{HwTreeSource, NullHwTreeSource, NULL_HW_TREE};
pub use init::{kernel_main, InitError, KernelInitSpawner, Phase};
pub use introspect::{IntrospectSource, NullIntrospectSource, NULL_INTROSPECT};
pub use introspect_source::KernelIntrospectSource;
pub use kthread::{
    reschedule_current, spawn_kthread, spawn_kthread_with_stack, spawn_kthread_with_stack_parked,
    spawn_user_kthread, spawn_user_kthread_with_stack, spawn_user_kthread_with_stack_live,
    with_current_live_space, BoxStack, KernelServiceBody, KernelStack, YieldHandle, Yielder,
    YielderHandle, KTHREAD_STACK_BYTES,
};
pub use kthread_irq::{CooperativeYield, KthreadIrqWaiter};
pub use launch_cache::LaunchCache;
pub use live_producer::{LiveDmaAlloc, LiveMemMap, LiveMmioMap, LiveSharedMem};
pub use memmap::{MemMap, NullMemMap, NULL_MEM_MAP};
pub use panic::{handle_panic, panic_dump, PanicContext};
pub use pipe::{Pipe, PipeEnd, PipeRole, PIPE_CAPACITY};
pub use preempt::{
    note_preempt_tick, preempt_current, preemption_count, take_preempt_pending,
    total_preemption_count,
};
pub use proc_id::{mint_proc_id, mint_proc_id_bootstrap};
pub use procsignal::{
    clear_intake, drain_pending_foreground, foreground_signal_installed,
    install_deferred_kill_lander, install_foreground_signal, intake_disable, intake_enable,
    intake_enabled, intake_ready, intake_take, land_running_kill, queue_foreground_signal,
    task_is_stopped, DeferredKillLander, DeferredKillLanderAlreadyInstalled, DeferredTeardown,
    ForegroundSignal, ForegroundSignalAlreadyInstalled, KernelProcessSignal, NullProcessSignal,
    ProcessSignal, NULL_PROCESS_SIGNAL,
};
pub use procwait::{
    KernelProcessWait, NullProcessWait, ProcessTable, ProcessWait, Reap, WaitedChild,
    NULL_PROCESS_WAIT,
};
pub use random::{reserve_errno, BootReserve, NullEntropy, RandomReserve};
pub use rlimit::{authorize_set, LimitSet, DEFAULT_STACK_LIMIT_BYTES};
pub use seat::{
    seat_errno, PresentGate, SeatRegistry, KEYBOARD_CHANNEL_CAPACITY, NULL_SEAT_REGISTRY,
};
pub use sleeplock::{SleepGuard, SleepLock};
pub use smp::{run_secondary, SecondaryExit};
pub use spawn::{
    admit_errno, refuse_build, spawn_and_enter, spawn_caller_errno, spawn_image, AdmitError,
    ArchImageBuilder, BuiltImage, EmbeddedProgram, ImageBuildCtx, InitSpawn, InitSpawnCtx,
    NullArchImageBuilder, ProgramRegistry, SpawnCallerError, SpawnRequest, EMPTY_PROGRAM_REGISTRY,
    NULL_ARCH_IMAGE_BUILDER,
};
pub use spawn_services::{
    install_spawn_services, installed_spawn_services, ArchSpawnRuntime, SpawnRuntime,
    SpawnServices, SpawnServicesAlreadyInstalled,
};
pub use syscalls::{KernelDispatchHook, KernelSpawnCtx, KernelSyscallHandlers, LoadPlan};
pub use syscfg::load_and_apply_system_config;
pub use useradmin::{
    LateUsersAdmin, NullUsersAdmin, UserAdminBacking, UserAdminEngine, UsersAdmin,
    UsersAdminAlreadyInstalled, NULL_USERS_ADMIN,
};
pub use users::{
    load_users_db, load_users_db_source, HeldUsersDbSource, LateUsersDb, NullUsersDbSource,
    UsersDbAlreadyInstalled, UsersDbSource, UsersDbText, UsersLoadError, NULL_USERS_DB,
    USERS_DB_PATH,
};
pub use waitq::{
    call_wake, call_wake_task, console_deregister, console_wake, drain_pending_wakes, hw_tree_wake,
    install_wait_arch, irq_wake, nearest_timed_deadline, procwait_wake, rearm_timed_wakeup,
    seat_input_wake, serve_wake, serve_wake_task, timed_wake_sweep, wait_now_ns,
    WaitArchAlreadyInstalled, WaitQueue, WaitQueueArch, CALL_WAITQ, CONSOLE_WAITQ, HW_TREE_WAITQ,
    IRQ_WAITQ, NO_DEADLINE, PROCWAIT_WAITQ, SEAT_INPUT_WAITQ, SERVE_WAITQ,
};
pub use wallclock::{KernelWallClock, NullWallClock, WallClockSource, NULL_WALL_CLOCK};
pub use watchdog::{
    check_stall, install_recovery as install_watchdog_recovery,
    install_report_sink as install_watchdog_sink, note_progress, on_watchdog_tick,
    set_activity as set_watchdog_activity, WatchdogActivity, DEFAULT_HARD_LOCKUP_THRESHOLD_NS,
    DEFAULT_SOFT_LOCKUP_THRESHOLD_NS,
};
