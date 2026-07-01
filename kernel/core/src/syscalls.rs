//! Production [`SyscallHandlers`] wiring for `kernel/core`.
//!
//! Stage 2.7 follow-up (f3) of `PLAN.md`. The dispatcher in
//! `kernel/syscall` performs the checks (identify caller, check
//! capability, validate arguments, audit) and then forwards the call
//! through the [`SyscallHandlers`] trait. This module ships the one
//! concrete implementation that the production kernel uses; tests of
//! the dispatcher continue to substitute their own mocks.
//!
//! # Surface
//!
//! [`KernelSyscallHandlers<'a, A>`] borrows three pieces of kernel
//! state for the lifetime of one syscall:
//!
//! * `&'a Scheduler<A>` — for `yield_now` and `exit` (and, in the
//!   future, anything else that needs a `TaskId → Task` lookup).
//! * `&'a RwLock<CapTable>` — for `cap_query` (read) and `cap_revoke`
//!   (write). Wrapping `CapTable` in `kernel/sync::RwLock` is the
//!   minimum interior mutability the dispatcher requires; the choice
//!   mirrors `Scheduler::tasks`'s reader-preferring lock and lets
//!   `kernel_main`'s `KernelState` compose the two registries under a
//!   single lock-ordering policy.
//! * `&'a A` — the arch port, for `clock_get` via
//!   [`KernelArch::monotonic_ns`].
//! * `&'a RwLock<PortRegistry>` — the named-port registry, for
//!   `ipc_send` / `ipc_recv` endpoint resolution. Wrapped in the same
//!   reader-preferring lock as `CapTable` so the IPC hot path takes
//!   only a shared lock.
//! * `&'a RwLock<AddressSpaceRegistry>` — the per-task address-space
//!   registry, so a handler can resolve `caller.task_id` to the user
//!   [`AddressSpace`](rustos_kernel_mem::AddressSpace) +
//!   [`PhysMap`] pair the
//!   [`rustos_kernel_mem::uaccess`] copy path walks
//!   ([`KernelSyscallHandlers::with_caller_aspace`], increment C of
//!   `PLAN.md` Stage 7). Reaching it here keeps the copy bridge inside
//!   `kernel/core` so the decoupled dispatcher (`kernel/syscall`)
//!   never gains a `kernel/mem` dependency.
//!   Wrapped in the same reader-preferring lock as the other two.
//!
//! `ipc_send` / `ipc_recv` resolve the destination endpoint against
//! that live registry: an endpoint that is not currently bound fails
//! closed with `NotFound` (a real lookup miss; the dispatcher's
//! standard pipeline audits it).
//!
//! Every consumer of `PLAN.md` Stage 7's "User-memory copy path" is now
//! **fully wired**: `ipc_send` (increment D.1), `ipc_recv` (D.2),
//! `cap_delegate` (D.3), and `random_get` (D.4) each move their bytes
//! through the validated [`rustos_kernel_mem::copy_in`] /
//! [`rustos_kernel_mem::copy_out`] boundary
//! ([`KernelSyscallHandlers::with_caller_aspace`] → `copy_fault_errno`)
//! and then run the backing subsystem ([`rustos_kernel_ipc::Port::send`]
//! / `recv_with`, the `CapTable` delegate path, and the kernel random
//! output reserve [`crate::random::RandomReserve`]). A faulting user
//! pointer — or a caller with no registered address space — fails closed
//! with [`Errno::BadAddress`] (the RustOS `EFAULT`), never an oracle
//! that distinguishes the cause.
//!
//! `random_get` draws CSPRNG output from the reserve composed into
//! `KernelState` and copies it out. Before the reserve
//! is seeded it is **not ready**, so the draw fails closed with
//! [`Errno::EntropyNotReady`] rather than returning weak bytes — the
//! reserve is seeded once the platform-RNG entropy seam lands.
//! The only remaining lookup-miss deferral is an unbound IPC endpoint:
//!
//! | Syscall               | Condition        | Errno      | Reason |
//! |-----------------------|------------------|------------|--------|
//! | `ipc_send`/`ipc_recv` | endpoint unbound | `NotFound` | No port is bound to the endpoint in the [`PortRegistry`]; a real lookup miss the dispatcher's standard pipeline audits. |
//!
//! # No ambient authority
//!
//! Nothing in this module reads or writes a global; every input is
//! threaded through [`KernelSyscallHandlers::new`]. `cap_query`,
//! `cap_revoke`, and `clock_get` all consult the caller's already-
//! validated [`CallerContext`] — there is no `uid == 0` shortcut.

use crate::sched::{CpuId, SchedError, Scheduler, SchedulerArch};
use rustos_abi::hwtree::{HwResource, HwResourceKind};
use rustos_abi::input::KeyInput;
use rustos_abi::{
    decode_log_record, BootId, CapabilityId, DescriptorTable, DirEntry, Errno, FileStat, IrqHandle,
    LimitKind, MapFlags, OpenFlags, ProcId, RandomFlags, ResourceLimit, StreamMode, SyscallNumber,
    Time64, WaitSetOp, WaitSourceKind, WallClockReading, WallTimeState, BOOT_ID_LEN,
    CONSOLE_INHERIT, FS_IO_MAX, FS_NAME_MAX, FS_PATH_MAX, LOG_FIELDS_MAX, LOG_RECORD_MAX,
    RANDOM_REQUEST_MAX_BYTES,
};
use rustos_caps::CapabilitySet;
use rustos_kernel_ipc::{
    CallEndpoint, CallEndpointLimits, CallTicket, EndpointId, PortRegistry, RecvCall, ReplyOutcome,
};
use rustos_kernel_irq::{
    block_until_ready, IrqController, IrqTable, IrqWaitAbort, IrqWaiter, WaitOutcome,
};
use rustos_kernel_mem::{
    copy_in, copy_out, FrameAllocator, PhysMap, UaccessError, UserAddressSpace, VirtAddr, PAGE_SIZE,
};
use rustos_kernel_sched_api::Priority;
use rustos_kernel_sec::{CapTable, TaskCapabilities, TaskId as SecTaskId, UserId};
use rustos_kernel_syscall::{CallerContext, Dispatcher, RawArgs, SyscallHandlers, SyscallResult};
use rustos_log::{Event, EventId, Field, Level, Sink};
use rustos_sync::RwLock;
use rustos_util::fmt::format_hex_u64;
use zeroize::Zeroize;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::aspace::AddressSpaceRegistry;
use crate::audit::AuditEvent;
use crate::bootinfo::KernelArch;
use crate::console::{ConsoleDevice, NO_CONSOLES};
use crate::devres::{
    dma_constraint, mappable_subwindow, translate_device_addr, DmaAllocFacility, MmioMapFacility,
    MsiAllocFacility, SharedMemFacility, NULL_DMA_ALLOC_FACILITY, NULL_MMIO_MAP_FACILITY,
    NULL_MSI_ALLOC_FACILITY, NULL_SHARED_MEM_FACILITY,
};
use crate::dispatch_slot::{DispatchHook, DispatchOutcome, RescheduleAction};
use crate::fs::{FilesystemService, NULL_FILESYSTEM};
use crate::hwtree::{HwTreeSource, NULL_HW_TREE};
use crate::input_focus::{InputFocus, NULL_INPUT_FOCUS};
use crate::memmap::{MemMap, NULL_MEM_MAP};
use crate::procwait::{ProcessWait, NULL_PROCESS_WAIT};
use crate::random::{reserve_errno, RandomReserve};
use crate::rlimit::{authorize_set, LimitSet};
use crate::spawn::{
    AdmitError, ProcessSpawn, ProgramRegistry, SpawnCtx, EMPTY_PROGRAM_REGISTRY, NULL_PROCESS_SPAWN,
};
use crate::users::{UsersDbSource, NULL_USERS_DB};
use crate::wallclock::{WallClockSource, NULL_WALL_CLOCK};

/// A no-op diagnostic [`Sink`] — the fail-closed default for the
/// `log_emit` handler's `log_sink` until the boot path installs the real
/// arch diagnostic sink.
///
/// Dropping a record rather than touching an uninstalled sink keeps a
/// pre-install `log_emit` harmless; the dispatcher's capability check still
/// runs, so the inert build never widens authority.
struct NullLogSink;

impl Sink for NullLogSink {
    fn write_event(&self, _event: &Event<'_>) {}
}

/// The shared no-op diagnostic sink the `log_emit` handler holds until the
/// boot path installs the real one through
/// [`KernelSyscallHandlers::with_log_sink`].
static NULL_LOG_SINK: NullLogSink = NullLogSink;

/// Production [`SyscallHandlers`] implementation.
///
/// Construct once at boot, after `KernelState` has assembled the
/// scheduler, the capability table, the arch handle, and the audit
/// sink. The struct holds borrows only, never owns anything; it is
/// designed to live on the stack of a syscall trampoline or inside
/// `KernelState` and be re-used for every syscall on every CPU.
pub struct KernelSyscallHandlers<'a, A>
where
    A: KernelArch + 'static,
{
    sched: &'a Scheduler<A>,
    caps: &'a RwLock<CapTable>,
    arch: &'a A,
    audit: &'a (dyn Sink + Sync),
    /// The kernel's **diagnostic** log sink — the same sink the kernel emits
    /// its own boot/runtime records through (`kernel/arch/*` routes it to the
    /// serial UART on a debug build, the video console on release). The
    /// `log_emit` handler emits a capability-gated, validated user-space
    /// record through this sink, attributed to the calling task. It is **never** the hash-chained security audit sink
    /// ([`Self::audit`]), which stays kernel-only, so user space can neither
    /// forge nor truncate an audit entry. Defaults to the no-op
    /// `NULL_LOG_SINK` so that until the boot path installs the real sink
    /// through [`Self::with_log_sink`] a `log_emit` is silently dropped
    /// rather than touching an uninstalled sink.
    log_sink: &'a (dyn Sink + Sync),
    irq: &'a IrqTable,
    /// Controller-mask seam consumed by [`IrqTable::fire`] from the
    /// architecture port's trap path. Held here so the per-trap
    /// firing code can reach it from inside the dispatch hook; the
    /// `irq_bind` / `irq_wait` syscall handlers themselves do not
    /// dereference it (mask happens on `fire`, not on `wait`).
    irq_controller: &'a (dyn IrqController + Sync),
    /// Named-port registry consulted by `ipc_send` / `ipc_recv` to
    /// resolve the endpoint carried in the syscall to a live, kernel-
    /// owned [`rustos_kernel_ipc::Port`]. Borrowed under the same
    /// reader-preferring lock `KernelState` wraps it in; the handlers
    /// take only a read guard (no global mutable
    /// static; the registry owns no lock of its own).
    ipc: &'a RwLock<PortRegistry>,
    /// Per-task address-space registry consulted to resolve the
    /// caller's [`rustos_kernel_sec::TaskId`] to the user
    /// [`AddressSpace`](rustos_kernel_mem::AddressSpace) and the
    /// [`PhysMap`] backing it — the pair the
    /// [`rustos_kernel_mem::uaccess`] copy path walks. Borrowed under the same reader-preferring lock as `caps`
    /// / `ipc`; [`Self::with_caller_aspace`] takes only a read guard.
    /// Threading it here (increment C, `PLAN.md` Stage 7) lets a
    /// handler reach the caller's mappings without coupling the
    /// decoupled dispatcher (`kernel/syscall`) to `kernel/mem`; increment D wires the deferred `ipc_send` /
    /// `ipc_recv` / `cap_delegate` / `random_get` copies through it.
    aspaces: &'a RwLock<AddressSpaceRegistry>,
    /// The kernel's single cryptographic random output reserve
    /// ([`crate::random::RandomReserve`]), consulted by
    /// `random_get` to draw CSPRNG output before copying it into the
    /// caller's buffer. Held type-erased behind a `Box` so the handler
    /// is not generic over the reserve's entropy source — the concrete
    /// platform-RNG seam is installed by re-seeding the boxed
    /// reserve, never by changing this borrow's type. Wrapped in the
    /// same reader-preferring [`RwLock`] as `caps` / `ipc` / `aspaces`
    /// so the kernel composes every per-task registry under one
    /// lock-ordering policy; drawing takes the write guard because the
    /// reserve mutates its buffer as it serves (the
    /// reserve owns no lock of its own).
    rng: &'a RwLock<Box<dyn RandomReserve + Send + Sync>>,
    /// The installed system console list `stream_write` / `stream_read`
    /// resolve a descriptor's console index against. Defaults to the empty [`NO_CONSOLES`] (every
    /// console-backed access fails closed with
    /// [`Errno::NotImplemented`]); the boot path installs the discovered
    /// list — index 0 the primary console (the detected display, else
    /// the first UART), further entries the independent secondary
    /// consoles (`plans/PI.md` P11) — through [`Self::with_consoles`].
    /// Held as a `'static` borrow because the installed consoles live
    /// for the lifetime of the running kernel.
    consoles: &'static [ConsoleDevice],
    /// The kernel's live physical-frame allocator, the source of the
    /// frames a spawned process's pages are mapped to (`plans/SPAWN.md`
    /// SP3). [`None`] until the boot path threads it through
    /// [`Self::with_frames`]; while it is `None` the `spawn` syscall fails
    /// closed with [`Errno::NotImplemented`] (the spawn
    /// subsystem is not wired). Borrowed for the handler's lifetime,
    /// exactly like the other registries.
    frames: Option<&'a FrameAllocator>,
    /// The kernel's live frame allocator as a `'static` borrow, handed to
    /// the spawn producer so it can build a child's **page tables** out of
    /// reclaimable RAM rather than a fixed-size `.bss` pool (the spawn capacity scales with discovered RAM and grows on
    /// demand). It is the same allocator as [`Self::frames`]; the distinct
    /// `'static`-typed field exists because a port's `AddressSpace` retains
    /// its page-table frame source for the child's lifetime. [`None`] until
    /// the boot path threads it through [`Self::with_page_table_frames`];
    /// while it is `None` the producer fails closed. Held
    /// `'static` because the kernel allocator lives for the running kernel's
    /// lifetime, exactly like the other `'static` boot-installed seams.
    page_table_frames: Option<&'static FrameAllocator>,
    /// The embedded-program registry the `spawn` syscall resolves a path
    /// against (`plans/SPAWN.md` SP3). Defaults to the shared empty
    /// registry, so a `spawn` of any path fails closed with
    /// [`Errno::NotFound`] until the boot path installs a populated one
    /// through [`Self::with_spawn`]. Held as a `'static` borrow because
    /// the registry's program bytes live for the lifetime of the running
    /// kernel.
    programs: &'static ProgramRegistry,
    /// The architecture-specific spawn producer the `spawn` syscall drives
    /// to build a child's isolated address space and admit it
    /// (`plans/SPAWN.md` SP3). Defaults to [`NULL_PROCESS_SPAWN`] (fail
    /// closed with [`Errno::NotImplemented`]); the boot
    /// path installs the concrete producer through [`Self::with_spawn`].
    /// Held as a `'static` borrow, exactly like the console device.
    spawn_service: &'static (dyn ProcessSpawn + 'static),
    /// The architecture-specific anonymous-memory producer the `mem_map` /
    /// `mem_unmap` syscalls drive to map and unmap fresh `RW` regions in the
    /// caller's own live address space (`plans/SPAWN.md` SP5). Defaults to
    /// [`NULL_MEM_MAP`] (fail closed with [`Errno::NotImplemented`]); the boot path installs the concrete `kernel/mem`
    /// producer through [`Self::with_mem_map`] once `SP5b` lands. Held as a
    /// `'static` borrow, exactly like the console device and spawn producer.
    mem_map: &'static (dyn MemMap + 'static),
    /// The scheduler-side process-wait producer the `wait` syscall drives
    /// to block the caller until one of its children exits, reap it, and
    /// report its exit code (`plans/SPAWN.md` SP6). Defaults to
    /// [`NULL_PROCESS_WAIT`] (fail closed with [`Errno::NotImplemented`]); the boot path installs the concrete producer
    /// through [`Self::with_process_wait`] once `SP6b` lands. Held as a
    /// `'static` borrow, exactly like the console device and spawn producer.
    process_wait: &'static (dyn ProcessWait + 'static),
    /// The kernel-held user database the `users_db_read` syscall serves
    /// (`plans/PI.md` P11). Defaults to [`NULL_USERS_DB`] (fail closed
    /// with [`Errno::NotImplemented`]); the boot path
    /// that mounts the root volume and loads the database installs the
    /// real holder through [`Self::with_users_db`]. Held as a `'static`
    /// borrow, exactly like the console device.
    users_db: &'static (dyn UsersDbSource + 'static),
    /// The kernel input-focus arbiter the `key_inject` / `display_acquire`
    /// / `display_release` / `keyboard_read` syscalls drive (`plans/PI.md` P11 — input follows the surface
    /// owner). Defaults to [`NULL_INPUT_FOCUS`], whose text sink is the
    /// fail-closed [`crate::console::NULL_CONSOLE_INPUT`], so a build with
    /// no arbiter wired refuses to route a key edge rather than leaking it
    /// to a device; the boot path installs the
    /// real arbiter — its text sink pointed at the console that owns the
    /// directly attached keyboard — through [`Self::with_input_focus`].
    /// Held as a `'static` borrow because the arbiter lives for the
    /// lifetime of the running kernel, exactly like the console device.
    input_focus: &'static InputFocus,
    /// The architecture MMIO-map producer the `mmio_map` syscall drives to
    /// map a granted device window into the caller's own live address space
    /// (`plans/PI.md` P10 chunk 5d-0). Defaults to
    /// [`NULL_MMIO_MAP_FACILITY`] (fail closed with [`Errno::NotImplemented`]); the boot path installs the concrete `kernel/mem`
    /// producer through [`Self::with_mmio_map_facility`]. Held as a `'static`
    /// borrow, exactly like the console device and the `mem_map` producer.
    mmio_map_facility: &'static (dyn MmioMapFacility + 'static),
    /// The architecture DMA-alloc producer the `dma_alloc` syscall drives to
    /// carve a coherent DMA buffer into the caller's own live address space
    /// (`plans/PI.md` P10 chunk 5d-0). Defaults to
    /// [`NULL_DMA_ALLOC_FACILITY`] (fail closed with [`Errno::NotImplemented`]); the boot path installs the concrete `kernel/mem`
    /// producer through [`Self::with_dma_alloc_facility`]. Held as a `'static`
    /// borrow, exactly like the MMIO-map producer.
    dma_alloc_facility: &'static (dyn DmaAllocFacility + 'static),
    /// The architecture MSI-alloc producer the `msi_alloc` syscall drives to
    /// mint an MSI vector and report its doorbell (`plans/PI.md` U-MSI).
    /// Defaults to [`NULL_MSI_ALLOC_FACILITY`] (fail closed with
    /// [`Errno::NotImplemented`] on a platform with no MSI controller); the
    /// boot path installs the concrete arch producer through
    /// [`Self::with_msi_alloc_facility`]. Held as a `'static` borrow, exactly
    /// like the MMIO-map and DMA-alloc producers.
    msi_alloc_facility: &'static (dyn MsiAllocFacility + 'static),
    /// The kernel-held discovered hardware tree the `hw_tree_read` /
    /// `hw_tree_wait` syscalls serve (
    /// Design D). Defaults to [`NULL_HW_TREE`] (fail closed with
    /// [`Errno::NotImplemented`]); the boot path installs
    /// the real store through [`Self::with_hw_tree`] once the inventory is
    /// seeded. Held as a `'static` borrow, exactly like the users database.
    hw_tree: &'static (dyn HwTreeSource + 'static),
    /// The shared-memory producer the `shm_create` / `shm_map` / `shm_unmap`
    /// syscalls drive to allocate, zero, map, and free cross-process
    /// shared-memory regions in the caller's own live address space
    /// (`plans/USB.md`). Defaults to [`NULL_SHARED_MEM_FACILITY`] (fail closed
    /// with [`Errno::NotImplemented`]); the boot path installs the concrete
    /// `kernel/mem`-backed producer through [`Self::with_shared_mem_facility`].
    /// Held as a `'static` borrow, exactly like the MMIO-map producer.
    shared_mem_facility: &'static (dyn SharedMemFacility + 'static),
    /// The kernel filesystem service the `fs_*` syscalls route through
    /// (`PREREQUISITES.md` P-A). Defaults to [`NULL_FILESYSTEM`] (every
    /// operation fails closed with [`Errno::NotImplemented`]); the boot path
    /// that owns the mounted volume installs the real disk-backed service
    /// through [`Self::with_filesystem`]. Held as a `'static` borrow, exactly
    /// like the users database, because the mounted filesystem lives for the
    /// lifetime of the running kernel.
    filesystem: &'static (dyn FilesystemService + 'static),
    /// The kernel wall clock the `wall_time_get` / `wall_time_set` syscalls
    /// read and drive (`PREREQUISITES.md` P-D). Defaults to the fail-closed
    /// [`NULL_WALL_CLOCK`] (reads report `Unset`, a set returns
    /// `NotImplemented`); the boot path installs the production
    /// [`crate::wallclock::KernelWallClock`] through [`Self::with_wall_clock`].
    /// Held `'static` because the leaked clock lives for the running kernel's
    /// lifetime, exactly like the other boot-installed seams.
    wall_clock: &'static (dyn WallClockSource + 'static),
    /// The per-boot identifier the `boot_id_get` syscall reports
    /// (`PREREQUISITES.md` P-E). Defaults to [`BootId::UNSET`]; the boot path
    /// mints the real value from the seeded CSPRNG reserve and installs it
    /// through [`Self::with_boot_id`]. While unset (no boot path ran, or the
    /// reserve could not be seeded in time) `boot_id_get` fails closed with
    /// [`Errno::EntropyNotReady`] rather than report the all-zero sentinel as
    /// a real id. A plain value, not a borrow: it is 16 immutable bytes minted
    /// once, not a live seam.
    boot_id: BootId,
}

impl<'a, A> KernelSyscallHandlers<'a, A>
where
    A: KernelArch + 'static,
{
    // The `'a` lifetime appears in the constructor parameters; it is
    // not elidable there even though `clippy::elidable_lifetime_names`
    // would suggest it could be. The methods that follow take `&self`
    // only, so we re-name the borrow as `'_` in their signatures.

    /// Build a new handler set bound to the supplied kernel state.
    ///
    /// All borrows must outlive the dispatcher instance that wraps
    /// this handler. In the production kernel `KernelState` owns the
    /// targets and keeps them alive for the lifetime of the kernel
    /// (no global mutable static).
    #[must_use]
    // Each argument is a *distinct* piece of kernel state the handler
    // borrows explicitly — there is no global mutable static and no
    // ambient authority to reach them through,
    // so they are threaded one-by-one exactly as `BootInfo::new`
    // mirrors its fields. Bundling them behind a wrapper purely to
    // satisfy the arg-count lint would be the one-use wrapper type
    // the charter forbids; the explicit list is the clearer shape.
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        sched: &'a Scheduler<A>,
        caps: &'a RwLock<CapTable>,
        arch: &'a A,
        audit: &'a (dyn Sink + Sync),
        irq: &'a IrqTable,
        irq_controller: &'a (dyn IrqController + Sync),
        ipc: &'a RwLock<PortRegistry>,
        aspaces: &'a RwLock<AddressSpaceRegistry>,
        rng: &'a RwLock<Box<dyn RandomReserve + Send + Sync>>,
    ) -> Self {
        Self {
            sched,
            caps,
            arch,
            audit,
            // Diagnostic log sink unwired until the boot path installs the
            // arch serial/console sink: until then a
            // `log_emit` is silently dropped through the no-op `NULL_LOG_SINK`
            // rather than touching an uninstalled sink.
            log_sink: &NULL_LOG_SINK,
            irq,
            irq_controller,
            ipc,
            aspaces,
            rng,
            // Fail closed until the boot path installs the discovered
            // console list: an early
            // `stream_write` / `stream_read` returns `NotImplemented`
            // rather than touching a device that does not exist.
            consoles: &NO_CONSOLES,
            // Spawn subsystem unwired until the boot path threads a frame
            // allocator + populated registry + producer (`plans/SPAWN.md`
            // SP3): `spawn` fails closed (`NotImplemented` / `NotFound`).
            frames: None,
            page_table_frames: None,
            programs: &EMPTY_PROGRAM_REGISTRY,
            spawn_service: &NULL_PROCESS_SPAWN,
            // Anonymous-memory subsystem unwired until the boot path installs
            // the `kernel/mem` live-mapping producer (`plans/SPAWN.md` SP5b):
            // `mem_map` / `mem_unmap` fail closed with `NotImplemented`.
            mem_map: &NULL_MEM_MAP,
            // Process-wait subsystem unwired until the boot path installs the
            // scheduler-side producer (`plans/SPAWN.md` SP6b): `wait` fails
            // closed with `NotImplemented`.
            process_wait: &NULL_PROCESS_WAIT,
            // Users-database service unwired until a boot path that mounted
            // the root volume installs the loaded holder (`plans/PI.md`
            // P11): `users_db_read` fails closed with `NotImplemented`.
            users_db: &NULL_USERS_DB,
            // Input-focus arbiter unwired until the boot path installs the
            // real one whose text sink owns the keyboard console
            // (`plans/PI.md` P11): `key_inject` / `keyboard_read` fail
            // closed (`NotImplemented` / no input) through the shared
            // `NULL_INPUT_FOCUS`.
            input_focus: &NULL_INPUT_FOCUS,
            // The MMIO-map facility is unwired until the boot path installs
            // the `kernel/mem` map producer (`plans/PI.md` P10 chunk 5d-0):
            // `mmio_map` fails closed (`NotFound` for an ungranted handle,
            // resolved against the per-task grant table in `aspaces`;
            // `NotImplemented` with no map facility) — never mapping an
            // ungranted or arbitrary region.
            mmio_map_facility: &NULL_MMIO_MAP_FACILITY,
            // The DMA-alloc facility is unwired until the boot path installs
            // the `kernel/mem` carve producer (`plans/PI.md` P10 chunk 5d-0):
            // `dma_alloc` fails closed (`NotFound` for an ungranted handle,
            // resolved against the per-task grant table in `aspaces`;
            // `NotImplemented` with no DMA facility) — never carving against
            // an ungranted constraint.
            dma_alloc_facility: &NULL_DMA_ALLOC_FACILITY,
            // The MSI-alloc facility is unwired until the boot path installs
            // the arch producer: `msi_alloc` fails closed with
            // `NotImplemented` (a platform with no MSI controller) — never
            // fabricating a vector.
            msi_alloc_facility: &NULL_MSI_ALLOC_FACILITY,
            // Hardware-tree store unwired until the boot path seeds the
            // discovered inventory and installs the holder: `hw_tree_read` / `hw_tree_wait` fail closed with
            // `NotImplemented`.
            hw_tree: &NULL_HW_TREE,
            // The shared-memory facility is unwired until the boot path
            // installs the `kernel/mem`-backed producer: `shm_create` /
            // `shm_map` / `shm_unmap` fail closed with `NotImplemented`,
            // never fabricating a region.
            shared_mem_facility: &NULL_SHARED_MEM_FACILITY,
            // The filesystem service is unwired until the boot path that owns
            // the mounted volume installs the disk-backed producer
            // (`PREREQUISITES.md` P-A): every `fs_*` syscall fails closed with
            // `NotImplemented`, never fabricating a handle or a read.
            filesystem: &NULL_FILESYSTEM,
            // The wall clock is unwired until the boot path installs the
            // production `KernelWallClock` (`PREREQUISITES.md` P-D):
            // `wall_time_get` reports `Unset` and `wall_time_set` fails closed
            // with `NotImplemented` through `NULL_WALL_CLOCK`.
            wall_clock: &NULL_WALL_CLOCK,
            // No per-boot id until the boot path mints one from the seeded
            // CSPRNG reserve and installs it (`PREREQUISITES.md` P-E):
            // `boot_id_get` fails closed with `EntropyNotReady` until then.
            boot_id: BootId::UNSET,
        }
    }

    /// Install the kernel filesystem service the `fs_*` syscalls route
    /// through, consuming and returning `self`.
    ///
    /// Called once by the boot path that owns the mounted volume
    /// (`PREREQUISITES.md` P-A). Until this is called the handler holds the
    /// fail-closed [`NULL_FILESYSTEM`] and every `fs_*` syscall returns
    /// [`Errno::NotImplemented`]. The service must be `'static` because the
    /// boot path leaks it alongside `KernelState`, which lives for the
    /// lifetime of the running kernel (no global mutable static; the install
    /// is a one-shot move).
    #[must_use]
    pub const fn with_filesystem(
        mut self,
        filesystem: &'static (dyn FilesystemService + 'static),
    ) -> Self {
        self.filesystem = filesystem;
        self
    }

    /// Install the kernel wall clock the `wall_time_get` / `wall_time_set`
    /// syscalls read and drive, consuming and returning `self`
    /// (`PREREQUISITES.md` P-D).
    ///
    /// Called once by the boot path with the leaked production
    /// [`crate::wallclock::KernelWallClock`]. Until then the handler holds the
    /// fail-closed [`NULL_WALL_CLOCK`]: `wall_time_get` reports an `Unset`
    /// epoch reading and `wall_time_set` returns [`Errno::NotImplemented`].
    /// The clock must be `'static` because the boot path leaks it alongside
    /// `KernelState` (no global mutable static; the install is a one-shot
    /// move).
    #[must_use]
    pub const fn with_wall_clock(
        mut self,
        wall_clock: &'static (dyn WallClockSource + 'static),
    ) -> Self {
        self.wall_clock = wall_clock;
        self
    }

    /// Install the per-boot identifier the `boot_id_get` syscall reports,
    /// consuming and returning `self` (`PREREQUISITES.md` P-E).
    ///
    /// Called once by the boot path with the [`BootId`] it minted from the
    /// seeded CSPRNG reserve. Until then — and on a port whose entropy source
    /// could not seed the reserve, where the mint yields [`BootId::UNSET`] —
    /// the handler reports `EntropyNotReady` rather than the all-zero
    /// sentinel. The value is copied in (16 immutable bytes), not borrowed.
    #[must_use]
    pub const fn with_boot_id(mut self, boot_id: BootId) -> Self {
        self.boot_id = boot_id;
        self
    }

    /// Install the discovered system console list `stream_write` /
    /// `stream_read` resolve descriptors against, consuming and
    /// returning `self`.
    ///
    /// Called once by the boot path after it has selected the console
    /// devices from the normalised hardware tree (`plans/PI.md` P6 /
    /// P11): index 0 is the primary console (the
    /// detected display when present, else the first discovered UART),
    /// and each further entry is an independent console with its own
    /// session context (the UART beside an active video console). Until
    /// this is called the handler holds the empty [`NO_CONSOLES`] and
    /// every console-backed stream access fails closed with
    /// [`Errno::NotImplemented`]. The list must be `'static`: the boot
    /// path leaks it alongside `KernelState`, which lives for the
    /// lifetime of the running kernel (no global
    /// mutable static; the install is a one-shot move).
    #[must_use]
    pub const fn with_consoles(mut self, consoles: &'static [ConsoleDevice]) -> Self {
        self.consoles = consoles;
        self
    }

    /// Install the kernel's diagnostic log sink the `log_emit` syscall emits
    /// user-space records through, consuming and returning `self`.
    ///
    /// Called once by the boot path with the same arch diagnostic sink the
    /// kernel routes its own records through (the serial UART on a debug
    /// build, the video console on release). Until this is called the handler
    /// holds the no-op `NULL_LOG_SINK` and a `log_emit` is silently dropped. The sink must be `'static` because the boot path
    /// leaks it alongside `KernelState`, which lives for the lifetime of the
    /// running kernel. This is the **diagnostic** sink only; the security
    /// audit sink stays the kernel-owned `audit` borrow user space can never
    /// reach.
    #[must_use]
    pub const fn with_log_sink(mut self, log_sink: &'a (dyn Sink + Sync)) -> Self {
        self.log_sink = log_sink;
        self
    }

    /// Install the kernel input-focus arbiter the keyboard syscalls drive,
    /// consuming and returning `self`.
    ///
    /// Called once by the boot path after it has built the arbiter with its
    /// text sink pointed at the console that owns the directly attached
    /// keyboard (on the Pi, the video console's input queue; `plans/PI.md`
    /// P11). Until this is called the handler holds [`NULL_INPUT_FOCUS`]
    /// and every `key_inject` in the default text focus fails closed,
    /// `keyboard_read` returns no input, and `display_acquire` /
    /// `display_release` toggle an arbiter no driver feeds. The arbiter must be `'static`: the boot path leaks it
    /// alongside `KernelState`, which lives for the lifetime of the running
    /// kernel (no global mutable static; the install is
    /// a one-shot move).
    #[must_use]
    pub const fn with_input_focus(mut self, input_focus: &'static InputFocus) -> Self {
        self.input_focus = input_focus;
        self
    }

    /// Install the live frame allocator the `spawn` syscall draws the
    /// child image's frames from, consuming and returning `self`
    /// (`plans/SPAWN.md` SP3).
    ///
    /// Until this is called the handler holds [`None`] and `spawn` fails
    /// closed with [`Errno::NotImplemented`] — the spawn subsystem is not
    /// wired. The allocator is the leaked `KernelState`'s, which lives for
    /// the lifetime of the running kernel.
    #[must_use]
    pub const fn with_frames(mut self, frames: &'a FrameAllocator) -> Self {
        self.frames = Some(frames);
        self
    }

    /// Install the live frame allocator as a `'static` borrow the spawn
    /// producer builds a child's **page tables** out of, consuming and
    /// returning `self`.
    ///
    /// This is the same allocator as [`Self::with_frames`]; the distinct
    /// `'static`-typed seam exists because a port's `AddressSpace` retains
    /// its page-table frame source for the child's lifetime, so the source
    /// must be `'static` (the producer caches a single
    /// [`rustos_kernel_mem::FrameTableSource`] over it). Until this is
    /// called the handler holds [`None`] and the producer fails closed, so a build can never over-spawn. The allocator
    /// is the leaked `KernelState`'s, which lives for the lifetime of the
    /// running kernel.
    #[must_use]
    pub const fn with_page_table_frames(mut self, frames: &'static FrameAllocator) -> Self {
        self.page_table_frames = Some(frames);
        self
    }

    /// Install the embedded-program registry and the architecture spawn
    /// producer the `spawn` syscall drives, consuming and returning `self`
    /// (`plans/SPAWN.md` SP3).
    ///
    /// Until this is called the handler holds the empty registry and
    /// [`NULL_PROCESS_SPAWN`], so `spawn` fails closed
    /// ([`Errno::NotFound`] / [`Errno::NotImplemented`]). Both must be
    /// `'static`: the program bytes and the producer live for the lifetime
    /// of the running kernel, exactly like the console device.
    #[must_use]
    pub const fn with_spawn(
        mut self,
        programs: &'static ProgramRegistry,
        spawn_service: &'static (dyn ProcessSpawn + 'static),
    ) -> Self {
        self.programs = programs;
        self.spawn_service = spawn_service;
        self
    }

    /// Install the users-database holder the `users_db_read` syscall
    /// serves, consuming and returning `self` (`plans/PI.md` P11).
    ///
    /// Called once by a boot path that mounted the root volume and ran
    /// the audited [`crate::load_users_db`] read. Until this is called
    /// the handler holds [`NULL_USERS_DB`] and `users_db_read` fails
    /// closed with [`Errno::NotImplemented`]. The
    /// holder must be `'static`: it lives for the lifetime of the
    /// running kernel, exactly like the console device.
    #[must_use]
    pub const fn with_users_db(mut self, users_db: &'static (dyn UsersDbSource + 'static)) -> Self {
        self.users_db = users_db;
        self
    }

    /// Install the discovered hardware-tree store the `hw_tree_read` /
    /// `hw_tree_wait` syscalls serve, consuming and returning `self`.
    ///
    /// Called once by the boot path after it seeds the inventory. Until
    /// this is called the handler holds [`NULL_HW_TREE`] and both syscalls
    /// fail closed with [`Errno::NotImplemented`]. The
    /// store must be `'static`: it lives for the lifetime of the running
    /// kernel, exactly like the users database.
    #[must_use]
    pub const fn with_hw_tree(mut self, hw_tree: &'static (dyn HwTreeSource + 'static)) -> Self {
        self.hw_tree = hw_tree;
        self
    }

    /// Install the architecture anonymous-memory producer the `mem_map` /
    /// `mem_unmap` syscalls drive, consuming and returning `self`
    /// (`plans/SPAWN.md` SP5).
    ///
    /// Until this is called the handler holds [`NULL_MEM_MAP`], so both
    /// syscalls fail closed with [`Errno::NotImplemented`]. The producer must be `'static`: it lives for the lifetime of
    /// the running kernel, exactly like the console device.
    #[must_use]
    pub const fn with_mem_map(mut self, mem_map: &'static (dyn MemMap + 'static)) -> Self {
        self.mem_map = mem_map;
        self
    }

    /// Install the architecture MMIO-map producer the `mmio_map` syscall
    /// drives, consuming and returning `self` (`plans/PI.md` P10 chunk
    /// 5d-0).
    ///
    /// Until this is called the handler holds [`NULL_MMIO_MAP_FACILITY`],
    /// so `mmio_map` fails closed with [`Errno::NotImplemented`]. The producer must be `'static`: it lives for the
    /// lifetime of the running kernel, exactly like the `mem_map` producer.
    #[must_use]
    pub const fn with_mmio_map_facility(
        mut self,
        mmio_map_facility: &'static (dyn MmioMapFacility + 'static),
    ) -> Self {
        self.mmio_map_facility = mmio_map_facility;
        self
    }

    /// Install the architecture DMA-alloc producer the `dma_alloc` syscall
    /// drives, consuming and returning `self` (`plans/PI.md` P10 chunk
    /// 5d-0).
    ///
    /// Until this is called the handler holds [`NULL_DMA_ALLOC_FACILITY`],
    /// so `dma_alloc` fails closed with [`Errno::NotImplemented`]. The producer must be `'static`: it lives for the
    /// lifetime of the running kernel, exactly like the MMIO-map producer.
    #[must_use]
    pub const fn with_dma_alloc_facility(
        mut self,
        dma_alloc_facility: &'static (dyn DmaAllocFacility + 'static),
    ) -> Self {
        self.dma_alloc_facility = dma_alloc_facility;
        self
    }

    /// Install the architecture MSI-alloc producer the `msi_alloc` syscall
    /// drives, consuming and returning `self` (`plans/PI.md` U-MSI).
    ///
    /// Until this is called the handler holds [`NULL_MSI_ALLOC_FACILITY`],
    /// so `msi_alloc` fails closed with [`Errno::NotImplemented`] (a
    /// platform with no MSI controller). The producer must be `'static`: it
    /// lives for the lifetime of the running kernel, exactly like the
    /// MMIO-map and DMA-alloc producers.
    #[must_use]
    pub const fn with_msi_alloc_facility(
        mut self,
        msi_alloc_facility: &'static (dyn MsiAllocFacility + 'static),
    ) -> Self {
        self.msi_alloc_facility = msi_alloc_facility;
        self
    }

    /// Install the shared-memory producer the `shm_create` / `shm_map` /
    /// `shm_unmap` syscalls drive, consuming and returning `self`
    /// (`plans/USB.md`). Also publishes the producer to the shared-region
    /// registry so the exit / driver-unload reclaim paths can free a
    /// region's frames without it being threaded through their wiring.
    ///
    /// Until this is called the handler holds [`NULL_SHARED_MEM_FACILITY`],
    /// so `shm_*` fail closed with [`Errno::NotImplemented`]. The producer
    /// must be `'static`: it lives for the lifetime of the running kernel,
    /// exactly like the MMIO-map producer.
    #[must_use]
    pub fn with_shared_mem_facility(
        mut self,
        shared_mem_facility: &'static (dyn SharedMemFacility + 'static),
    ) -> Self {
        self.shared_mem_facility = shared_mem_facility;
        self
    }

    /// Install the scheduler-side process-wait producer the `wait` syscall
    /// drives, consuming and returning `self` (`plans/SPAWN.md` SP6).
    ///
    /// Until this is called the handler holds [`NULL_PROCESS_WAIT`], so
    /// `wait` fails closed with [`Errno::NotImplemented`]. The producer must be `'static`: it lives for the lifetime of
    /// the running kernel, exactly like the console device, the spawn
    /// producer, and the anonymous-memory producer.
    #[must_use]
    pub const fn with_process_wait(
        mut self,
        process_wait: &'static (dyn ProcessWait + 'static),
    ) -> Self {
        self.process_wait = process_wait;
        self
    }

    /// Borrow the [`IrqTable`] this handler set wires `irq_bind` /
    /// `irq_wait` against.
    ///
    /// The kernel-binary trap path obtains the table this way so it
    /// can call [`IrqTable::fire`] from a trap dispatcher without
    /// having to re-borrow `KernelState`. The `irq_controller`
    /// argument [`IrqTable::fire`] requires is exposed through
    /// [`Self::irq_controller`].
    #[must_use]
    pub fn irq_table(&self) -> &IrqTable {
        self.irq
    }

    /// Borrow the [`IrqController`] this handler set wires
    /// [`IrqTable::fire`] against.
    #[must_use]
    pub fn irq_controller(&self) -> &(dyn IrqController + Sync) {
        self.irq_controller
    }

    /// Resolve the caller's user address space and the physical map
    /// backing it, then run `f` with the borrowed pair while the
    /// per-task registry's read guard is held.
    ///
    /// This is the bridge increment C (`PLAN.md` Stage 7) adds so a
    /// syscall handler can reach the bytes of the calling task's user
    /// memory: the registry maps `caller.task_id` to the
    /// `(&dyn UserAddressSpace, &dyn PhysMap)` pair the
    /// [`rustos_kernel_mem::uaccess`] copy path walks. The closure shape keeps the read guard alive for exactly
    /// the span the borrowed references are used and never hands a
    /// caller's mappings out past it; the registry exposes only
    /// `translate`, so the copy path can read but never mutate them.
    ///
    /// Returns `None` (fail closed) when no address
    /// space is registered for the caller — e.g. a kernel task that
    /// never had user mappings, or a `CallerContext` whose task has
    /// already exited and been withdrawn. A handler maps that `None`
    /// to its own stable [`Errno`]; increment D consumes this to drive
    /// `copy_in` / `copy_out` for the deferred `ipc_send` / `ipc_recv`
    /// / `cap_delegate` / `random_get` payloads.
    pub fn with_caller_aspace<R>(
        &self,
        caller: &CallerContext<'_>,
        f: impl FnOnce(&dyn UserAddressSpace, &dyn PhysMap) -> R,
    ) -> Option<R> {
        let registry = self.aspaces.read();
        let (space, physmap) = registry.resolve(caller.task_id)?;
        Some(f(space, physmap))
    }

    /// Re-freeze the calling task's live address space into the registry
    /// after a syscall has mutated it (`mem_map` / `mem_unmap` / `mmio_map`
    /// / `dma_alloc`).
    ///
    /// The registry holds a `Send + Sync` [`rustos_kernel_mem::FrozenAddressSpace`]
    /// snapshot, not the live `!Sync` space; a snapshot frozen at spawn
    /// describes only the spawn-time image and stack. Once a task grows its
    /// own heap, frees part of it, or a driver maps a granted window, that
    /// snapshot is stale and [`with_caller_aspace`](Self::with_caller_aspace)'s
    /// `copy_in` / `copy_out` walk would miss (or still expose) those pages
    /// — the defect that made `spawn`'s heap-allocated path argument fault
    /// with [`Errno::BadAddress`]. The mutating handler runs on the CPU the
    /// caller is switched in on, so the caller's live space is the one
    /// published for [`SchedulerArch::current_cpu`]; re-freeze it and publish
    /// the fresh snapshot so the very next copy reflects the current
    /// mappings. A caller with no published live space (a
    /// kernel task, or a task spawned without a retained space) or no
    /// registered snapshot is a no-op — there is nothing to refresh and the
    /// mutation could not have touched a live space either.
    fn refreeze_caller_aspace(&self, caller: &CallerContext<'_>) {
        let cpu = SchedulerArch::current_cpu(self.arch);
        if let Some(frozen) = crate::kthread::with_current_live_space(cpu, |live| live.freeze()) {
            self.aspaces
                .write()
                .reregister_space(caller.task_id, Box::new(frozen));
        }
    }

    /// Copy a filesystem path of `len` bytes from the caller's address space
    /// at `ptr` and validate it as a UTF-8 string, for the `fs_open` /
    /// `fs_mkdir` / `fs_unlink` handlers.
    ///
    /// Validates every input before use: an empty path or one longer than
    /// [`FS_PATH_MAX`] is refused with [`Errno::LengthOutOfRange`] before any
    /// allocation; a copy fault — or a caller with no registered address
    /// space — fails closed with [`Errno::BadAddress`], never leaking which;
    /// non-UTF-8 bytes are [`Errno::OutOfRange`]. The path's structural
    /// validity (absolute, no `.`/`..`, component bounds) is the secured
    /// VFS's to judge under the caller's credentials, not this copy step's.
    fn copy_path_in(
        &self,
        caller: &CallerContext<'_>,
        ptr: u64,
        len: usize,
    ) -> Result<String, Errno> {
        if len == 0 || len > FS_PATH_MAX {
            return Err(Errno::LengthOutOfRange);
        }
        let mut buf = vec![0u8; len];
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_in(space, physmap, VirtAddr::new(ptr), &mut buf)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }
        match core::str::from_utf8(&buf) {
            Ok(path) => Ok(String::from(path)),
            Err(_) => Err(Errno::OutOfRange),
        }
    }
}

/// Collapse every [`UaccessError`] onto the single stable
/// [`Errno::BadAddress`] (the RustOS `EFAULT`).
///
/// A syscall that copies through the kernel's `copy_from_user` /
/// `copy_to_user` boundary returns one code for *every* faulting-pointer
/// reason — null, unmapped, kernel-only, wrong permission, off the direct
/// map — so a malicious caller cannot use the distinction as an oracle to
/// probe the kernel's memory layout. The handler maps
/// the absence of any registered address space (a kernel task, or one
/// withdrawn on `exit`) onto the same code at the call site.
fn copy_fault_errno(_err: UaccessError) -> Errno {
    Errno::BadAddress
}

/// Size of the kernel staging buffer `random_get` draws CSPRNG output
/// into before copying it into the caller's buffer.
///
/// `random_get` cannot hand the reserve the caller's user pointer (the
/// reserve fills a kernel slice; user memory is reached only through the
/// validated [`copy_out`] boundary), so it draws into this fixed stack
/// buffer and copies it out one chunk at a time. A fixed size avoids a
/// per-call heap allocation whose failure path could OOM; the buffer is wiped after use (zeroed on
/// consumption). 256 bytes serves the common small request in a single
/// iteration while keeping the on-stack cost trivial; a larger request
/// simply loops.
const RANDOM_STAGE_CHUNK: usize = 256;

/// Upper bound, in bytes, on a single `stream_write` call.
///
/// `stream_write` stages the caller's bytes in one kernel-owned buffer
/// before handing them to the device, so an unbounded `len` would let a
/// caller force an arbitrarily large kernel allocation whose failure
/// path could OOM (deterministic OOM behaviour). The
/// call therefore writes at most this many bytes and returns the count;
/// a caller with more to say loops, exactly as POSIX `write` allows. A
/// banner line is far smaller than this, so the common path never
/// iterates.
const CONSOLE_WRITE_MAX: usize = 4096;

/// Upper bound, in bytes, on a single `stream_read` call.
///
/// `stream_read` reads into one kernel-owned staging buffer before
/// copying it out to the caller, so an unbounded `len` would let a caller
/// force an arbitrarily large kernel allocation whose failure path could
/// OOM (deterministic OOM behaviour). The call therefore
/// reads at most this many bytes and returns the count; a caller wanting
/// more loops, exactly as POSIX `read` allows. A line of console input is
/// far smaller than this, so the common path never iterates.
const CONSOLE_READ_MAX: usize = 4096;

/// Upper bound, in bytes, on the program path a single `spawn` call may
/// pass.
///
/// `spawn` stages the caller's path in one kernel-owned buffer before
/// looking it up in the registry, so an unbounded `path_len` would let a
/// caller force an arbitrarily large kernel allocation whose failure path
/// could OOM (deterministic OOM behaviour). An absolute
/// program path (`/Apps/<Name>.app/Run`) is far shorter
/// than this; a longer request is refused with [`Errno::NotFound`] (it
/// cannot name any registered program) rather than allocated.
const SPAWN_PATH_MAX: usize = 1024;

impl<A> SyscallHandlers for KernelSyscallHandlers<'_, A>
where
    A: KernelArch + 'static,
{
    fn yield_now(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        // SP2b (`plans/SPAWN.md` SP2): a `yield` from a resumable user
        // kthread is driven by the reschedule path, not here. The
        // dispatch hook recognises the `yield` syscall number and returns
        // `DispatchOutcome::Reschedule { action: Yield, .. }`; the
        // bin-crate callback then suspends the caller back to the
        // scheduler, which re-enqueues it from the `TaskAction::Yield` the
        // kthread reports when it switches back. Driving
        // `Scheduler::yield_current` here as well would double-handle the
        // re-enqueue — and, fatally, mutate scheduler state re-entrantly
        // from inside the in-flight `step` the kthread is running under
        // (the SP2a note's reconciliation). The handler is therefore inert
        // and always reports success; the value is encoded only on the
        // (degenerate) path where no user kthread is published.
        Ok(0)
    }

    fn exit(&self, caller: &CallerContext<'_>, code: i32) -> SyscallResult {
        // Hand the exit code to the scheduler-side process-wait producer so a
        // parent blocked in `wait` can reap this task and read its terminal
        // status back (`plans/SPAWN.md` SP6). The producer keeps the code only
        // for a task it tracks as a child (a process spawned through `spawn`);
        // PID 1 and kernel threads it does not track are ignored, and the
        // default `NULL_PROCESS_WAIT` is an inert no-op — so this is not the
        // interface creep the charter forbids: the one consumer (`wait`)
        // exists. The dispatcher's `SyscallInvoked` audit record (the `EXIT`
        // spec sets `audit = true`) still carries the code for the log.
        self.process_wait.record_exit(caller.task_id, code);
        //
        // Order matters:
        //
        //   1. Release every IRQ binding the exiting task held
        //      (`docs/src/security/irq.md` — the kernel unmasks no
        //      lines on exit; a freshly created task that wants the
        //      same line must re-issue `irq_bind`).
        //   2. Drop the capability record so a concurrent
        //      `cap_query` racing this `exit` cannot observe a task
        //      that the scheduler still believes exists but whose
        //      caps have vanished.
        //
        // The scheduler reap is step 3, driven by the reschedule path
        // below rather than from this handler. Each step is idempotent;
        // the call ordering matters for
        // the *security* observer (no caller can hold an audited
        // capability bit after the IRQ subsystem has released the
        // task's bindings).
        //
        // SP2b (`plans/SPAWN.md` SP2): the scheduler reap itself is driven
        // by the reschedule path, not here — the dispatch hook returns
        // `DispatchOutcome::Reschedule { action: Exit, .. }` and the
        // bin-crate callback suspends the caller, after which the kthread
        // reports `TaskAction::Exit` and the scheduler reaps it. Calling
        // `Scheduler::exit` here as well would mutate scheduler state
        // re-entrantly from inside the in-flight `step` the exiting
        // kthread runs under. This handler keeps only the security-state
        // cleanup the reschedule path does not perform (IRQ bindings,
        // capability record); both are idempotent and ordered so no caller
        // can hold an audited capability bit after the IRQ subsystem has
        // released the task's bindings.
        let task = caller.task_id;
        let _ = self.irq.release_for(task);
        // Tear down every synchronous call endpoint this task served before
        // dropping its capability record: a user-space service that exits
        // (cleanly, by fault, or killed) must not leave callers blocked in
        // `ipc_call` forever — destroying its endpoints cancels their
        // in-flight calls, and waking `CALL_WAITQ` re-runs each parked
        // caller's poll so it abandons fail-closed.
        // The owner key is the *security* task id (`caller.caps.task()`), the
        // same id `CallEndpoint::create` recorded as the owner.
        if crate::callreg::unregister_owned_by(caller.caps.task().0, self.audit) > 0 {
            crate::waitq::call_wake();
        }
        // Release every shared-memory mapping this task held, dropping each
        // reference and zeroing + freeing any region whose last reference this
        // releases (zero-on-free). Done here, while the exiting task is still
        // the current one, so the registry can scrub a freed region's frames
        // through its own live space before the scheduler reap drops it. A
        // task that mapped none reclaims nothing (idempotent).
        crate::sharedreg::reclaim_task(self.shared_mem_facility, task);
        // Drop every wait-set this task owned. A wait-set holds no resource of
        // its own (its members only *name* endpoints and IRQ lines, reclaimed
        // above), so dropping the sets is the whole reclamation; idempotent.
        crate::waitset::release_owned_by(task.0);
        let _ = self.caps.write().remove(task);
        Ok(0)
    }

    fn ipc_send(
        &self,
        caller: &CallerContext<'_>,
        endpoint: u64,
        ptr: u64,
        len: usize,
    ) -> SyscallResult {
        // resolve the destination endpoint against the live
        // named-port registry before touching the caller's buffer. An
        // endpoint that is not currently bound fails closed with
        // `NotFound` — a real lookup miss; the dispatcher's standard
        // pipeline audits the rejection at this boundary
        // (`PortRegistry::lookup` deliberately does not).
        let ipc = self.ipc.read();
        let Some(port) = ipc.lookup(EndpointId(endpoint)) else {
            return Err(Errno::NotFound);
        };

        // Bound the copy *before* allocating: refuse a payload larger
        // than the port advertises (itself capped at
        // `IPC_MESSAGE_MAX_PAYLOAD_LEN` at `Port::create`). This makes a
        // malicious `len` cheap to reject and keeps the kernel from
        // staging an oversized buffer the port would reject anyway. The
        // same `MessageTooLarge` code `Port::send` would return.
        if len as u64 > u64::from(port.max_payload()) {
            return Err(Errno::MessageTooLarge);
        }

        // Copy the payload in from the caller's address space through the
        // validated `copy_from_user` boundary. The
        // bytes are staged in a kernel-owned buffer; `Port::send` then
        // takes its own kernel copy, so the sender cannot mutate the
        // message after it is accepted. `with_caller_aspace` yields
        // `None` when the caller has no registered address space (a
        // kernel task, or one already withdrawn on `exit`) — fail closed
        // with the same `BadAddress` an actual fault produces, never
        // leaking which case occurred.
        let mut payload = alloc::vec![0u8; len];
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_in(space, physmap, VirtAddr::new(ptr), &mut payload)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }

        // Enqueue. `Port::send` performs the per-send capability check
        // against the caller's effective set and
        // re-checks the payload size, returning a stable `Errno` for
        // every refusal.
        port.send(caller.caps, &payload, self.audit).map(|()| 0)
    }

    fn ipc_recv(
        &self,
        caller: &CallerContext<'_>,
        endpoint: u64,
        ptr: u64,
        len: usize,
    ) -> SyscallResult {
        // resolve the destination endpoint against the live
        // named-port registry. An endpoint that is not currently bound
        // fails closed with `NotFound`; the dispatcher's standard
        // pipeline audits the rejection.
        let ipc = self.ipc.read();
        let Some(port) = ipc.lookup(EndpointId(endpoint)) else {
            return Err(Errno::NotFound);
        };

        // Peek-then-commit (D.2): the message is dequeued only once it
        // has been copied into the caller's buffer through the validated
        // `copy_to_user` boundary. Resolving the caller's address space
        // first nests the mailbox lock *inside* the address-space read
        // guard, so the spinlock is held only for the bounded copy.
        //
        // `with_caller_aspace` yields `None` when the caller has no
        // registered address space (a kernel task, or one withdrawn on
        // `exit`); `recv_with` yields `Some(None)` when the mailbox is
        // momentarily empty. The two are kept distinct so an empty
        // mailbox is the retryable `WouldBlock`, never confused with a
        // faulting pointer.
        let copied = self.with_caller_aspace(caller, |space, physmap| {
            port.recv_with(|msg| -> Result<usize, Errno> {
                let payload = msg.payload.as_slice();
                // Refuse to truncate: a buffer smaller than the message
                // fails closed and — because `recv_with` only commits on
                // `Ok` — leaves the message queued for a retry with a
                // larger buffer.
                if payload.len() > len {
                    return Err(Errno::BufferTooSmall);
                }
                // Every `UaccessError` collapses onto the single
                // `BadAddress` so a faulting pointer cannot be used as a
                // memory-layout oracle. A fault leaves the
                // message queued.
                copy_out(space, physmap, VirtAddr::new(ptr), payload)
                    .map(|()| payload.len())
                    .map_err(copy_fault_errno)
            })
        });

        match copied {
            // No registered address space — fail closed with the same
            // `BadAddress` a fault produces, never leaking the case.
            None => Err(Errno::BadAddress),
            // Bound but empty: a live endpoint with nothing to deliver
            // is retryable, not an error in the endpoint itself.
            Some(None) => Err(Errno::WouldBlock),
            // Delivered: return the number of payload bytes copied.
            Some(Some(Ok(n))) => Ok(n as u64),
            // Copy-out fault or undersized buffer: the message stays
            // queued (`recv_with` did not commit).
            Some(Some(Err(err))) => Err(err),
        }
    }

    fn cap_query(&self, caller: &CallerContext<'_>, cap: CapabilityId) -> SyscallResult {
        // The caller's effective caps are already in `CallerContext`.
        // Going through the CapTable would re-validate the same set
        // and add a lock acquisition for no extra information; the
        // dispatcher guarantees `caller.caps` is the authoritative
        // record `KernelState` registered for `caller.task_id`.
        Ok(u64::from(caller.caps.has(cap)))
    }

    fn cap_delegate(&self, caller: &CallerContext<'_>, target: u64, set_ptr: u64) -> SyscallResult {
        // `set_ptr` names a fixed-size `CapabilitySet` (its 256-bit bitmap
        // as four little-endian `u64` words, `CapabilitySet::WIRE_LEN`
        // bytes) in the caller's address space. Copy it in through the
        // validated `copy_from_user` boundary before
        // touching the capability table. A caller with no registered
        // address space (a kernel task, or one withdrawn on `exit`) and
        // any copy fault both collapse onto `BadAddress`, never leaking
        // which case occurred.
        let mut buf = [0u8; CapabilitySet::WIRE_LEN];
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_in(space, physmap, VirtAddr::new(set_ptr), &mut buf)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }

        // Every 32-byte pattern is a representable set, and `buf` is
        // exactly `WIRE_LEN`, so decoding cannot fail. Run the `CapTable`
        // delegate path: `delegate` replaces the target's effective set
        // with the requested subset, rejecting a *widening* request with
        // `DelegationWiden` and auditing the decision. An unknown
        // target task is the stable `NotFound`, not a kernel bug — the
        // same condition `cap_revoke` surfaces.
        let requested = CapabilitySet::from_le_bytes(&buf)?;
        let mut guard = self.caps.write();
        match guard.caps_for_mut(SecTaskId(target)) {
            Some(record) => record.delegate(&requested, self.audit).map(|()| 0),
            None => Err(Errno::NotFound),
        }
    }

    fn cap_revoke(
        &self,
        _caller: &CallerContext<'_>,
        target: u64,
        cap: CapabilityId,
    ) -> SyscallResult {
        // The target task is named by raw `TaskId`. `caps_for_mut`
        // returns `None` for an unknown task; that is a stable
        // condition (`Errno::NotFound`) rather than a kernel bug.
        let mut guard = self.caps.write();
        let entry = guard.caps_for_mut(SecTaskId(target));
        match entry {
            Some(record) => {
                // `revoke` is idempotent — it returns `false` if the
                // capability was not held, but the audit record it
                // emits via the underlying `TaskCapabilities::revoke`
                // is the security-relevant signal (the *attempt* is
                // the event). The boolean is intentionally discarded.
                let _ = record.revoke(cap, self.audit);
                Ok(0)
            }
            None => Err(Errno::NotFound),
        }
    }

    fn clock_get(&self, caller: &CallerContext<'_>) -> SyscallResult {
        // `monotonic_ns` is documented as monotonically non-decreasing
        // per CPU; the dispatcher invokes us on the issuing CPU's
        // process context (step 1), so reading from
        // `self.arch.current_cpu()` is the natural source. We do not
        // accept a caller-supplied CPU id — there is no syscall
        // argument for one, and a kernel-trusted lookup is the only
        // sanctioned source.
        let cpu = crate::sched::SchedulerArch::current_cpu(self.arch);
        let ns = self.arch.monotonic_ns(cpu);
        // A full-resolution timer is a side-channel primitive. Only a principal explicitly trusted with
        // `CAP_TIME_HIRES` reads the raw nanosecond value; every other
        // caller — including the parser sandboxes and untrusted
        // apps — sees the reading floored to
        // `COARSE_CLOCK_GRANULARITY_NS` (security by default). Coarsening is value-only: the `clock_get`
        // ABI signature is unchanged, and `coarsen_clock_ns` preserves
        // the per-CPU monotonic-non-decreasing contract the `irq_wait`
        // timeout loop relies on.
        if caller.caps.has(CapabilityId::TIME_HIRES) {
            Ok(ns)
        } else {
            Ok(rustos_abi::coarsen_clock_ns(ns))
        }
    }

    fn wall_time_get(&self, caller: &CallerContext<'_>, out: u64, out_cap: usize) -> SyscallResult {
        // The caller's buffer must hold a whole reading; a short buffer fails
        // closed rather than truncating it. Unprivileged, like `clock_get`:
        // the dispatcher attaches no capability gate.
        if out_cap < WallClockReading::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        // Read the monotonic clock on the issuing CPU — the same source
        // `clock_get` uses — and project the stored wall instant forward by
        // the elapsed monotonic time. Ordering never depends on this value.
        let cpu = SchedulerArch::current_cpu(self.arch);
        let monotonic_ns = self.arch.monotonic_ns(cpu);
        let bytes = self.wall_clock.read(monotonic_ns).to_le_bytes();
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(out), &bytes)
        }) {
            Some(Ok(())) => Ok(bytes.len() as u64),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        }
    }

    fn wall_time_set(
        &self,
        caller: &CallerContext<'_>,
        time: u64,
        time_len: usize,
        state: u32,
    ) -> SyscallResult {
        // The dispatcher has already checked `CAP_TIME_SET`. Validate the
        // provenance state first: a value that is not a defined variant, or
        // the non-settable `Unset`, is rejected before any state is touched
        // (`WallTimeState::from_u8` rejects the former, the clock the latter).
        let state = u8::try_from(state)
            .ok()
            .map(WallTimeState::from_u8)
            .ok_or(Errno::OutOfRange)??;
        // The instant must be a whole `Time64`; a short buffer fails closed.
        if time_len < Time64::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let mut buf = [0u8; Time64::WIRE_LEN];
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_in(space, physmap, VirtAddr::new(time), &mut buf)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }
        // Reject a non-canonical instant (e.g. nanos >= 1e9) before setting.
        let wall = Time64::from_bytes(&buf)?;
        let cpu = SchedulerArch::current_cpu(self.arch);
        let monotonic_ns = self.arch.monotonic_ns(cpu);
        self.wall_clock.set(wall, monotonic_ns, state).map(|()| 0)
    }

    fn boot_id_get(&self, caller: &CallerContext<'_>, out: u64, out_cap: usize) -> SyscallResult {
        // The caller's buffer must hold the whole id; a short buffer fails
        // closed rather than truncating it. Unprivileged, like `clock_get`:
        // the dispatcher attaches no capability gate (the boot id is a public
        // per-boot nonce, not a secret).
        if out_cap < BOOT_ID_LEN {
            return Err(Errno::BufferTooSmall);
        }
        // Fail closed when no real id was minted: a port whose CSPRNG reserve
        // could not be seeded leaves the boot id `UNSET`, and we must never
        // hand the all-zero sentinel to a caller as if it were a real id.
        if self.boot_id.is_unset() {
            return Err(Errno::EntropyNotReady);
        }
        let bytes = self.boot_id.to_le_bytes();
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(out), &bytes)
        }) {
            Some(Ok(())) => Ok(bytes.len() as u64),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        }
    }

    fn irq_bind(&self, caller: &CallerContext<'_>, line: u32) -> SyscallResult {
        // Capability gate has already been enforced by the
        // dispatcher (the syscall spec carries the `CAP_IRQ_BIND`
        // requirement and the dispatcher's per-call check rejects
        // any caller without it before reaching this handler —
        // `kernel/syscall::Dispatcher::dispatch`). We re-bind the
        // table key against `caller.task_id` (kernel-trusted, not
        // caller-supplied) so the resulting [`IrqHandle`] is
        // unforgeable in the strong sense: it can only be waited on
        // by the task that bound it.
        match self.irq.bind(line, caller.task_id) {
            Ok(out) => Ok(out.handle.as_u64()),
            Err(e) => Err(e.to_errno()),
        }
    }

    fn irq_wait(
        &self,
        caller: &CallerContext<'_>,
        handle: IrqHandle,
        timeout_ns: u64,
    ) -> SyscallResult {
        // The poll-and-park loop itself lives in
        // `rustos_kernel_irq::block_until_ready` so the in-kernel
        // `KernelVirtioHost::notify_wait` path can drive the same
        // implementation without a second copy.
        // This handler supplies the scheduler + arch seam through
        // `SyscallIrqWaiter` and translates the terminal outcome to
        // the documented stable `Errno`.
        //
        // The caller *parks* off the run queue between polls (no busy yield): it is woken by `crate::waitq::irq_wake`
        // the instant the device-IRQ dispatch path runs `IrqTable::fire`,
        // or, with a finite timeout, by the architecture one-shot's
        // per-tick sweep (`crate::waitq::timed_wake_sweep`), and re-checks
        // its bound line after every wake. This mirrors `hw_tree_wait`
        // exactly (one park discipline).
        let cpu = SchedulerArch::current_cpu(self.arch);
        let task = caller.task_id.0;
        let waiter = SyscallIrqWaiter {
            sched: self.sched,
            arch: self.arch,
            // The CPU is captured once: `monotonic_ns` is documented
            // as non-decreasing per CPU, and the handler never
            // migrates mid-wait, so every clock read inside the loop
            // observes the same monotone source
            // (`docs/src/security/irq.md`).
            cpu,
            task: caller.task_id,
            // Re-arm the bound line through the wired controller before each
            // park so an interrupt-driven user-space driver (which holds no
            // controller access) is routed + unmasked on the kernel's behalf.
            // The line is resolved once, owner-checked: a forged/foreign
            // handle yields `None` and re-arms nothing.
            irq_controller: self.irq_controller,
            line: self.irq.line_for(handle, caller.task_id),
        };

        // Register *before* the first poll so a fire arriving in the
        // window between the poll and the park is not lost: the
        // dispatch-path `irq_wake` then `unpark`s this task and the
        // scheduler's wake-pending token converts a concurrent park
        // commit into a re-ready. The deadline is one
        // saturating add from the clock so a `u64::MAX` timeout stays
        // `NO_DEADLINE` (explicit wake only) rather than wrapping to a
        // tiny value (fail closed). `block_until_ready`
        // recomputes the identical deadline from its own first reading.
        let deadline_ns = self.arch.monotonic_ns(cpu).saturating_add(timeout_ns);
        crate::waitq::IRQ_WAITQ.register(task, deadline_ns);
        let outcome = block_until_ready(self.irq, handle, caller.task_id, timeout_ns, &waiter);
        // Leave the wait set and re-point the one-shot at whatever deadline
        // any *remaining* waiter needs (or clear it if none) so a finished
        // wait never leaves a stale arming behind.
        crate::waitq::IRQ_WAITQ.deregister(task);
        self.arch
            .set_wakeup(crate::waitq::IRQ_WAITQ.earliest_deadline());

        match outcome {
            WaitOutcome::Ready => Ok(0),
            WaitOutcome::TimedOut => Err(Errno::TimedOut),
            // A forged / released handle and a vanished task both map
            // to `Errno::NotFound`: `NoSuchTask` cannot happen here
            // (`CallerContext` is built from the live scheduler
            // current-task slot) but is mapped for symmetry with
            // the park seam.
            WaitOutcome::NotFound | WaitOutcome::Aborted(IrqWaitAbort::TaskVanished) => {
                Err(Errno::NotFound)
            }
            // Any other scheduler error fails closed to
            // `Errno::OutOfRange`.
            WaitOutcome::Aborted(IrqWaitAbort::SchedulerError) => Err(Errno::OutOfRange),
        }
    }

    fn random_get(
        &self,
        caller: &CallerContext<'_>,
        buf: u64,
        len: usize,
        flags: RandomFlags,
    ) -> SyscallResult {
        // Bound the work one call may request (a caller
        // needing more issues further requests).
        if len > RANDOM_REQUEST_MAX_BYTES {
            return Err(Errno::LengthOutOfRange);
        }
        // A zero-length request draws nothing and writes nothing; it
        // succeeds without consulting the reserve or the caller's buffer.
        if len == 0 {
            return Ok(0);
        }

        // A non-blocking caller must never wait for the RNG to be seeded; it fails closed with `EntropyNotReady`
        // instead. The flag has no effect once the reserve is seeded —
        // generation never waits for fresh entropy.
        let non_blocking = flags.is_non_blocking();

        // Resolve the caller's address space once and stream the bytes
        // out under its read guard. The reserve produces CSPRNG output
        // into a fixed kernel staging buffer — never a per-call heap
        // allocation whose failure path could OOM — and
        // each chunk is copied into the caller's buffer through the
        // validated `copy_out` boundary, then the staging is wiped
        // (zeroed on consumption). The whole draw runs
        // under the reserve's write guard so one request observes a
        // contiguous stream from a single generator.
        let outcome = self.with_caller_aspace(caller, |space, physmap| {
            let mut reserve = self.rng.write();
            let mut staging = [0u8; RANDOM_STAGE_CHUNK];
            let mut offset = 0usize;
            while offset < len {
                let take = core::cmp::min(staging.len(), len - offset);
                let chunk = &mut staging[..take];
                if let Err(e) = reserve.draw(chunk, non_blocking) {
                    staging.zeroize();
                    return Err(reserve_errno(e));
                }
                // The destination is the caller pointer advanced by the
                // bytes already delivered. A pointer that overflows the
                // address space fails closed as `BadAddress` rather than
                // wrapping.
                let Some(addr) = buf.checked_add(offset as u64) else {
                    staging.zeroize();
                    return Err(Errno::BadAddress);
                };
                if let Err(e) = copy_out(space, physmap, VirtAddr::new(addr), chunk) {
                    staging.zeroize();
                    return Err(copy_fault_errno(e));
                }
                offset += take;
            }
            // Wipe the staging buffer once the request is fully served
            // (the kernel copy of the output never
            // lingers).
            staging.zeroize();
            Ok(offset as u64)
        });
        // A caller with no registered address space (a kernel task, or
        // one withdrawn on `exit`) collapses onto the same fail-closed
        // `BadAddress` every copy-path handler returns.
        outcome.unwrap_or(Err(Errno::BadAddress))
    }

    fn stream_write(
        &self,
        caller: &CallerContext<'_>,
        fd: u32,
        buf: u64,
        len: usize,
    ) -> SyscallResult {
        // Resolve `fd` against the caller's per-process descriptor table
        // *before* touching any state: the inherited
        // descriptor, not an ambient device, is the authority. An
        // `fd` that is not a writable inherited stream fails closed with
        // `NotFound` (its stream backing does not exist for this caller),
        // never leaking whether it was closed, the wrong direction, or
        // out of range. An unregistered caller resolves to the
        // all-`Closed` default and fails here too.
        let streams = self.aspaces.read().streams(caller.task_id);
        if streams.mode(fd) != StreamMode::Write {
            return Err(Errno::NotFound);
        }
        // Resolve the descriptor's console index against the installed
        // console list (the descriptor names its
        // backing; the video console and the UART are separate devices,
        // `plans/PI.md` P11). An index with no installed console —
        // including the empty pre-install list — announces the inert
        // interface rather than silently dropping
        // the bytes.
        let Some(device) = self.consoles.get(usize::from(streams.console(fd))) else {
            return Err(Errno::NotImplemented);
        };
        // The dispatcher already checked `CAP_CONSOLE_WRITE` and that
        // `buf` is non-null (`UserPtr`). A zero-length write touches
        // neither the caller's buffer nor the device.
        if len == 0 {
            return Ok(0);
        }
        // Bound the staging allocation so a hostile `len` cannot force
        // an arbitrarily large kernel buffer. Writing a
        // prefix and reporting the count is valid short-write behaviour;
        // the caller loops for the remainder.
        let take = core::cmp::min(len, CONSOLE_WRITE_MAX);

        // Copy the bytes in from the caller's address space through the
        // validated `copy_from_user` boundary before
        // touching the device. `with_caller_aspace` yields `None` when
        // the caller has no registered address space (a kernel task, or
        // one withdrawn on `exit`) — fail closed with the same
        // `BadAddress` an actual fault produces, never leaking which
        // case occurred.
        let mut payload = alloc::vec![0u8; take];
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_in(space, physmap, VirtAddr::new(buf), &mut payload)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }

        // Hand the copied bytes to the descriptor's console device.
        device.write.write(&payload).map(|n| n as u64)
    }

    fn stream_read(
        &self,
        caller: &CallerContext<'_>,
        fd: u32,
        buf: u64,
        len: usize,
    ) -> SyscallResult {
        // Resolve `fd` against the caller's per-process descriptor table
        // *before* touching any state: an `fd`
        // that is not a readable inherited stream fails closed with
        // `NotFound`, never leaking which case occurred. An unregistered
        // caller resolves to the all-`Closed` default and fails here too.
        let streams = self.aspaces.read().streams(caller.task_id);
        if streams.mode(fd) != StreamMode::Read {
            return Err(Errno::NotFound);
        }
        // Resolve the descriptor's console index against the installed
        // console list (`plans/PI.md` P11 — a login on
        // the UART console reads the UART, a login on the video console
        // reads its own keyboard source, never each other's). A missing
        // console — including the empty pre-install list — announces the
        // inert interface rather than fabricating
        // input.
        let Some(device) = self.consoles.get(usize::from(streams.console(fd))) else {
            return Err(Errno::NotImplemented);
        };
        // The dispatcher already checked `CAP_CONSOLE_READ` and that
        // `buf` is non-null (`UserPtr`). A zero-length read touches
        // neither the device nor the caller's buffer.
        if len == 0 {
            return Ok(0);
        }
        // Bound the staging allocation so a hostile `len` cannot force an
        // arbitrarily large kernel buffer. Reading a
        // prefix and reporting the count is valid short-read behaviour;
        // the caller loops for the remainder.
        let take = core::cmp::min(len, CONSOLE_READ_MAX);

        // Read from the descriptor's console input source into the
        // kernel staging buffer first. A faulting device surfaces its
        // `Errno` here before any user memory is touched.
        let mut payload = alloc::vec![0u8; take];
        let read = device.read.read(&mut payload)?;
        // A correct device never reports more than the buffer it was
        // handed; clamp defensively so a buggy source cannot drive an
        // out-of-bounds copy (validate every input,
        // including from the device side of the seam).
        let read = core::cmp::min(read, take);
        if read == 0 {
            // No input was pending: report a zero-length read without
            // touching the caller's buffer. The production init pipeline
            // wraps the installed device in `BlockingConsoleRead`, which
            // parks the caller until input arrives instead of returning
            // zero; this branch remains for a bare
            // non-blocking device (host tests), whose caller loops.
            return Ok(0);
        }

        // Echo the consumed bytes back to this console's own output when
        // terminal echo is enabled (local echo), so an
        // interactive user sees what they type. The kernel owns the read
        // line discipline, so this needs no separate `CAP_CONSOLE_WRITE`;
        // it is a no-op while a caller has suppressed echo for a password
        // read (`stream_echo`). Best-effort and cosmetic — it never fails
        // the read the caller asked for.
        device.echo_bytes(&payload[..read]);

        // Copy the bytes actually read out to the caller's address space
        // through the validated `copy_to_user` boundary. `with_caller_aspace` yields `None` when the caller has
        // no registered address space (a kernel task, or one withdrawn on
        // `exit`) — fail closed with the same `BadAddress` an actual
        // fault produces, never leaking which case occurred.
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(buf), &payload[..read])
        }) {
            Some(Ok(())) => Ok(read as u64),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        }
    }

    fn spawn(
        &self,
        caller: &CallerContext<'_>,
        path: u64,
        path_len: usize,
        console: u64,
    ) -> SyscallResult {
        // The dispatcher already checked `CAP_PROC_SPAWN` and that `path`
        // is non-null (`UserPtr`). Bound the staged path so a hostile
        // `path_len` cannot force an arbitrarily large kernel allocation; an over-long or empty path cannot name a
        // registered program, so it fails closed with `NotFound`.
        if path_len == 0 || path_len > SPAWN_PATH_MAX {
            return Err(Errno::NotFound);
        }

        // Resolve the child's standard-stream attachment *before*
        // touching any further state: `CONSOLE_INHERIT`
        // copies the caller's own descriptor table (the child stays on
        // its parent's console), while an explicit value must name
        // an installed console index — anything else fails closed with
        // `NotFound`, never attaching the child to a device that does
        // not exist (`plans/PI.md` P11).
        let child_streams = if console == CONSOLE_INHERIT {
            self.aspaces.read().streams(caller.task_id)
        } else {
            let index = match u8::try_from(console) {
                Ok(index) if usize::from(index) < self.consoles.len() => index,
                _ => return Err(Errno::NotFound),
            };
            DescriptorTable::standard_on(index)
        };

        // The spawn subsystem must be fully wired before any state is
        // touched: a build with no frame allocator threaded fails closed
        // with `NotImplemented`, the spawn-equivalent of
        // `stream_write`'s `NULL_CONSOLE`.
        let Some(frames) = self.frames else {
            return Err(Errno::NotImplemented);
        };

        // Copy the path in from the caller's address space through the
        // validated `copy_from_user` boundary before
        // touching the registry. A faulting pointer — or a caller with no
        // registered address space — fails closed with `BadAddress`, never
        // leaking which case occurred.
        let mut path_buf = alloc::vec![0u8; path_len];
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_in(space, physmap, VirtAddr::new(path), &mut path_buf)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }

        // Resolve the path to a registered embedded program. An unknown
        // path fails closed with `NotFound` — there is
        // no prefix or alias resolution.
        let Some(program) = self.programs.lookup(&path_buf) else {
            return Err(Errno::NotFound);
        };

        // Hand the validated `rxe` to the architecture spawn producer,
        // which builds a fresh hardware-isolated address space and admits
        // it as a runnable process through `ctx`, returning the new PID.
        // The default `NULL_PROCESS_SPAWN` fails closed with
        // `NotImplemented`. The producer re-asserts the
        // `CAP_PROC_SPAWN` gate inside `spawn_image` and audits the
        // decision; the child receives only its manifest∩user-grant
        // authority.
        // Record the new child against the spawning caller so a later
        // `wait` from this parent can reap it (`plans/SPAWN.md` SP6). The
        // parent is the kernel-trusted caller identity, never a
        // caller-supplied value.
        let ctx = KernelSpawnCtx::new(
            frames,
            self.page_table_frames,
            self.audit,
            self.sched,
            self.caps,
            self.aspaces,
            self.arch,
            caller.task_id,
            self.process_wait,
            child_streams,
            // A user-driven `spawn` grants the child no device resources:
            // device windows are minted only by the privileged driver-spawn
            // path from the matched node's requests (no
            // ambient authority;).
            &[],
            // …and it is not a node-matched driver load, so the child has no
            // loaded node and may publish no `hw_emit_node` child.
            None,
            // Mint the child's process-instance identity from the kernel's
            // single CSPRNG reserve, attested kernel-side and never
            // influenced by the spawning caller.
            crate::proc_id::mint_proc_id(self.rng),
        );
        self.spawn_service.spawn(program, &ctx)
    }

    fn console_count(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        // The dispatcher already checked `CAP_CONSOLE_WRITE`. The count
        // is the installed list's length — the index space `spawn`'s
        // `console` argument selects from (
        // `plans/PI.md` P11); an empty pre-install list honestly
        // reports zero consoles.
        Ok(self.consoles.len() as u64)
    }

    fn stream_echo(&self, caller: &CallerContext<'_>, fd: u32, enabled: u32) -> SyscallResult {
        // Resolve `fd` against the caller's per-process descriptor table
        // *before* touching any state: echo is a
        // property of an *input* stream's console, so `fd` must be a
        // readable inherited stream — anything else fails closed with
        // `NotFound`, never leaking which case occurred. An unregistered
        // caller resolves to the all-`Closed` default and fails here too.
        let streams = self.aspaces.read().streams(caller.task_id);
        if streams.mode(fd) != StreamMode::Read {
            return Err(Errno::NotFound);
        }
        // Resolve the descriptor's console against the installed list. A
        // missing console — including the empty pre-install list —
        // announces the inert interface rather than
        // pretending the toggle took effect.
        let Some(device) = self.consoles.get(usize::from(streams.console(fd))) else {
            return Err(Errno::NotImplemented);
        };
        // The dispatcher already checked `CAP_CONSOLE_READ`. Any non-zero
        // value enables echo; zero disables it (the ABI contract). The
        // toggle is the program's own terminal control — login disables
        // echo around a password read so the secret is never rendered.
        device.set_echo(enabled != 0);
        Ok(0)
    }

    fn key_inject(&self, caller: &CallerContext<'_>, buf: u64, len: usize) -> SyscallResult {
        // The dispatcher already checked `CAP_INPUT_INJECT` and that `buf`
        // is non-null (`UserPtr`). A record is fixed-width: a `len` that
        // cannot hold one fails closed rather than letting the kernel decode
        // a truncated edge (never act on a partial
        // input).
        if len < KeyInput::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        // Copy exactly one record in from the caller's address space through
        // the validated `copy_from_user` boundary before
        // touching the arbiter. The staging buffer lives on the stack and is
        // wiped on every exit: a key edge can carry a typed character (a
        // password keystroke transits here), so it must not linger
        // (zero memory that held a credential;).
        // `with_caller_aspace` yields `None` when the caller has no
        // registered address space — fail closed with the same `BadAddress`
        // an actual fault produces, never leaking which case occurred.
        let mut record_bytes = [0u8; KeyInput::WIRE_LEN];
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_in(space, physmap, VirtAddr::new(buf), &mut record_bytes)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => {
                record_bytes.zeroize();
                return Err(copy_fault_errno(err));
            }
            None => {
                record_bytes.zeroize();
                return Err(Errno::BadAddress);
            }
        }

        // Decode the record fail-closed: a malformed edge is refused rather
        // than interpreted. The driver no longer
        // chooses the encoding or destination — the arbiter routes the edge
        // to the text console or the desktop keyboard channel by who holds
        // focus (`plans/PI.md` P11).
        let decoded = KeyInput::from_bytes(&record_bytes);
        record_bytes.zeroize();
        let record = decoded?;
        let consumed = self.input_focus.inject(record)?;
        // Witness the first successful delivery exactly once (`plans/PI.md` P11): proof that an (autoloaded)
        // keyboard driver has come up and is routing input through the
        // arbiter. The one-shot latch fires this on the first edge only —
        // never per keystroke — and the record carries no key content,
        // count, or timing, so a typed secret and its cadence never reach
        // the log (no input-content/timing noise;
        // — secret hygiene).
        if self.input_focus.note_first_delivery() {
            crate::audit::emit(
                self.audit,
                rustos_log::Level::Info,
                AuditEvent::InputDelivered,
                &[],
            );
        }
        Ok(consumed as u64)
    }

    fn display_acquire(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        // The dispatcher already checked `CAP_DISPLAY`. Claiming the display
        // switches the arbiter's foreground to the desktop keyboard channel
        // so subsequently injected key edges follow the new surface owner
        // (`plans/PI.md` P11).
        self.input_focus.acquire_display();
        Ok(0)
    }

    fn display_release(&self, _caller: &CallerContext<'_>) -> SyscallResult {
        // The dispatcher already checked `CAP_DISPLAY`. Releasing the
        // display returns the arbiter's foreground to the text console so a
        // login/shell once again receives the keyboard (
        // `plans/PI.md` P11).
        self.input_focus.release_display();
        Ok(0)
    }

    fn keyboard_read(&self, caller: &CallerContext<'_>, buf: u64, len: usize) -> SyscallResult {
        // The dispatcher already checked `CAP_INPUT_READ` and that `buf` is
        // non-null (`UserPtr`). A record is fixed-width: a `len` that cannot
        // hold one fails closed, the kernel never writes a partial record.
        if len < KeyInput::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        // Drain one record into a stack buffer first. `read_key` returns
        // `0` when the channel is momentarily empty (a valid short read the
        // caller loops on) or one whole record's `WIRE_LEN`. The buffer is
        // wiped on every exit (a key edge may carry
        // a typed character).
        let mut record_bytes = [0u8; KeyInput::WIRE_LEN];
        let read = match self.input_focus.read_key(&mut record_bytes) {
            Ok(read) => read,
            Err(err) => {
                record_bytes.zeroize();
                return Err(err);
            }
        };
        if read == 0 {
            record_bytes.zeroize();
            return Ok(0);
        }

        // Copy the record out to the caller's address space through the
        // validated `copy_to_user` boundary. A `None`
        // (unregistered caller) or a fault fails closed with `BadAddress`,
        // never leaking which case occurred.
        let result = match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(buf), &record_bytes[..read])
        }) {
            Some(Ok(())) => Ok(read as u64),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        };
        record_bytes.zeroize();
        result
    }

    fn mem_map(
        &self,
        caller: &CallerContext<'_>,
        len: usize,
        flags: MapFlags,
        addr_hint: u64,
    ) -> SyscallResult {
        // The dispatcher already validated `len` fits in `usize`, that
        // `flags` carries no reserved bit, and that `addr_hint` is a
        // well-formed `u64`. A zero-length mapping is meaningless; reject it
        // before touching any state (validate every
        // input) rather than mapping an empty region.
        if len == 0 {
            return Err(Errno::LengthOutOfRange);
        }
        // Whole pages are what the producer actually backs, so charge — and
        // check the limit against — the page-rounded size, the same figure
        // `mem_unmap` later credits. An overflow on rounding is a request no
        // address space could hold; reject it closed.
        let charged = (len as u64)
            .div_ceil(PAGE_SIZE as u64)
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(Errno::OutOfRange)?;
        // Enforce the caller's `AddressSpaceBytes` ceiling *before* mapping
        // (check capacity before touching state, fail closed): the live
        // total this map would reach must not exceed the task's soft limit —
        // the operative `ulimit -v`-style bound a principal may impose
        // (default `UNLIMITED`, so an unconstrained task is never affected).
        // The limit set is stored against the kernel-trusted caller id, never
        // a caller-supplied value, and a saturated/overflowing projection
        // denies rather than wrapping into a bogus small total. Without this
        // the limit would be settable but silently ignored on the one path
        // that consumes the resource (fail open).
        {
            let aspaces = self.aspaces.read();
            let soft = aspaces
                .limits(caller.task_id)
                .get(LimitKind::AddressSpaceBytes)
                .soft;
            let projected = aspaces
                .mapped_anon_bytes(caller.task_id)
                .checked_add(charged)
                .ok_or(Errno::OutOfRange)?;
            if projected > soft {
                return Err(Errno::OutOfRange);
            }
        }
        // Hand the request to the installed `kernel/mem` producer, which
        // maps the region into the caller's own live address space, zeroes
        // it, and returns its base (`plans/SPAWN.md` SP5b). Until one is
        // installed the default `NULL_MEM_MAP` fails closed with
        // `NotImplemented`, never pretending a region was
        // mapped. A frame exhaustion surfaces as `OutOfMemory` here
        // (deterministic OOM).
        let result = self.mem_map.map(len, flags, addr_hint);
        // The map grew the caller's live space; charge the page-rounded size
        // against the task's address-space accounting and re-freeze the
        // registry snapshot so the next `copy_in`/`copy_out` can see the new
        // region (the copy path must reflect live memory). Only on success:
        // a failed map touched no mappings and charges nothing.
        if result.is_ok() {
            self.aspaces.write().charge_anon(caller.task_id, charged);
            self.refreeze_caller_aspace(caller);
        }
        result
    }

    fn mmio_map(
        &self,
        caller: &CallerContext<'_>,
        handle: u64,
        offset: u64,
        len: usize,
    ) -> SyscallResult {
        // step 2 (capability) was enforced by the dispatcher: the
        // `mmio_map` spec carries `CAP_MMIO_MAP`. Step 3 (validate every
        // input) is here, and it is the security spine of this syscall: we
        // resolve `handle` to a granted resource **for the calling task**
        // (`caller.task_id` is kernel-trusted, never caller-supplied), so a
        // forged or another driver's handle resolves to nothing and is
        // refused (no trusted-caller shortcut; — a
        // driver reaches only the resources its matched node requested). The
        // per-task grant table lives in the address-space registry (minted
        // when a driver is admitted, reclaimed when the task is withdrawn on
        // exit), so the read guard is held only for the lookup.
        let Some(resource) = self.aspaces.read().grant(caller.task_id, handle) else {
            return Err(Errno::NotFound);
        };
        // The grant must name a memory window of this driver's, and the
        // requested `[offset, offset + len)` sub-region must lie wholly
        // inside it; reject any other kind/shape, a zero/overflowing length,
        // or a sub-region that escapes the grant before touching a page
        // table (fail closed rather than mapping the wrong
        // thing; — a driver maps only inside a region it was granted).
        // Mapping a bounded sub-region (not the whole grant) is what lets a
        // driver granted a large outbound bus aperture map just the single
        // BAR it enumerated, instead of the whole 1 GiB window — the latter
        // would exhaust the per-task MMIO virtual window and fail closed with
        // `OutOfMemory`.
        let (phys_base, len) = mappable_subwindow(&resource, offset, len)?;
        // Mechanism: the installed producer maps only that region —
        // caching disabled, never executable — into the caller's own live
        // address space and returns its base virtual address. The default
        // `NULL_MMIO_MAP_FACILITY` fails closed with `NotImplemented`, never pretending a window was mapped; frame
        // exhaustion surfaces as `OutOfMemory` (deterministic OOM).
        let result = self.mmio_map_facility.map_window(phys_base, len);
        // The window grew the caller's live space; re-freeze the registry
        // snapshot so a later copy through the driver's space sees it. Only on success.
        if result.is_ok() {
            self.refreeze_caller_aspace(caller);
        }
        result
    }

    fn dma_alloc(
        &self,
        caller: &CallerContext<'_>,
        handle: u64,
        len: usize,
        device_out: u64,
    ) -> SyscallResult {
        // step 2 (capability) was enforced by the dispatcher: the
        // `dma_alloc` spec carries `CAP_MEM_DMA`. Step 3 (validate every
        // input) is here. Resolve `handle` to a granted resource **for the
        // calling task** (`caller.task_id` is kernel-trusted), so a forged or
        // another driver's handle resolves to nothing and is refused
        // (— a driver reaches only the resources its
        // matched node requested), exactly as `mmio_map`.
        let Some(resource) = self.aspaces.read().grant(caller.task_id, handle) else {
            return Err(Errno::NotFound);
        };
        // The grant must name a DMA constraint; reject any other kind before
        // carving (fail closed).
        let constraint = dma_constraint(&resource)?;
        // A zero-length buffer names nothing; reject it before any carve.
        if len == 0 {
            return Err(Errno::LengthOutOfRange);
        }
        // The buffer must fit the grant's declared maximum extent, when one
        // is declared (`max_len == 0` means no declared maximum). Reject an
        // over-large request before carving.
        if constraint.max_len != 0 && (len as u64) > constraint.max_len {
            return Err(Errno::OutOfRange);
        }
        // Mechanism: the installed producer carves a physically-contiguous,
        // zeroed, coherent block bounded by the grant's `addr_limit` into the
        // caller's own live address space. The default `NULL_DMA_ALLOC_FACILITY`
        // fails closed with `NotImplemented`; frame
        // exhaustion surfaces as `OutOfMemory` (deterministic OOM).
        let carve = self.dma_alloc_facility.alloc(len, constraint.addr_limit)?;
        // Resolve the device-visible base the driver programs into its
        // hardware. For a coherent (untranslated) constraint it is the carved
        // CPU-physical base; for a translating inbound viewport
        // (`dma_translated`, e.g. the Pi 4's `IB MEM 0x0..0x1ffffffff ->
        // 0x4_0000_0000`) it is that base re-based onto the far side of the
        // viewport — checked, never wrapped. The
        // carve already lies below `addr_limit`, so this only re-bases it; a
        // base outside the viewport's CPU window fails closed.
        let device_addr = translate_device_addr(&constraint, carve.device_addr)?;
        // The carve grew the caller's live space; re-freeze the registry
        // snapshot before the copy below so the new DMA window is visible to
        // the copy path.
        self.refreeze_caller_aspace(caller);
        // Hand the device-visible base back through the `device_out` user
        // pointer via the validated `copy_to_user` boundary, exactly as `wait` writes the reaped status — a faulting
        // `device_out` collapses onto the same fail-closed `BadAddress` an
        // actual fault produces. The buffer stays mapped; it is
        // reclaimed when the task's live space is dropped on exit
        // (`LiveSpace::drop`), so a driver that passes a bad pointer self-DoSes
        // a buffer at worst — it never widens authority.
        let device_bytes = device_addr.to_ne_bytes();
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(device_out), &device_bytes)
        }) {
            Some(Ok(())) => Ok(carve.cpu_va),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        }
    }

    fn dma_free(&self, caller: &CallerContext<'_>, handle: u64, cpu_va: u64) -> SyscallResult {
        // step 2 (capability) was enforced by the dispatcher: the `dma_free`
        // spec carries `CAP_MEM_DMA`, symmetric with `dma_alloc`. Step 3
        // (validate every input): resolve `handle` to a granted resource **for
        // the calling task** (`caller.task_id` is kernel-trusted), so a forged
        // or another driver's handle resolves to nothing and is refused, and
        // the grant must name a DMA constraint — exactly as `dma_alloc`. A
        // task may free only against a DMA constraint it still holds.
        let Some(resource) = self.aspaces.read().grant(caller.task_id, handle) else {
            return Err(Errno::NotFound);
        };
        let _constraint = dma_constraint(&resource)?;
        // Mechanism: the installed producer releases the buffer based at
        // `cpu_va` from the caller's own live space, zeroing every backing
        // byte (zero-on-free) before its frames return to the allocator. Only
        // `cpu_va` is taken from the caller; the buffer's extent is the
        // allocator's own authoritative record, so a `cpu_va` that is not the
        // base of a live carve in *this task's* DMA window fails closed
        // (covering a stale, double, or cross-task free) without releasing
        // anything. The default `NULL_DMA_ALLOC_FACILITY` fails closed with
        // `NotImplemented`.
        self.dma_alloc_facility.free(cpu_va)?;
        // The free shrank the caller's live space; re-freeze the registry
        // snapshot so the copy path no longer sees the released DMA window
        // (leaving it in the stale snapshot would let a copy read or write
        // memory the task no longer owns — fail closed). Only on success.
        self.refreeze_caller_aspace(caller);
        Ok(0)
    }

    fn resource_grants(&self, caller: &CallerContext<'_>, buf: u64, len: usize) -> SyscallResult {
        // step 2 (capability): none required — a task reads only its
        // *own* minted grants, which confers no authority over anything else
        // (the handles are useless without the `CAP_MMIO_MAP` / `CAP_MEM_DMA`
        // the driver also holds, and `mmio_map` / `dma_alloc` re-check
        // ownership when they are presented). This is the
        // own-process-observer baseline. Step 3: serialise the grant set of
        // the **calling task** (`caller.task_id` is kernel-trusted, never
        // caller-supplied) from the per-task grant table in the
        // address-space registry; the read guard is held only for the
        // serialisation.
        let bytes = self.aspaces.read().grants_to_le_bytes(caller.task_id);
        // Never deliver a partial grant list: a buffer that cannot hold the
        // whole set fails closed, so the driver re-sizes
        // and retries rather than binding against a truncated table.
        if bytes.len() > len {
            return Err(Errno::BufferTooSmall);
        }
        // A task with no grants is a valid, empty result — not an error
        // (an unbound node is normal); skip the copy.
        if bytes.is_empty() {
            return Ok(0);
        }
        // Copy the records out to the caller's address space through the
        // validated `copy_to_user` boundary. A `None`
        // (unregistered caller) or a fault fails closed with `BadAddress`,
        // never leaking which case occurred.
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(buf), &bytes)
        }) {
            Some(Ok(())) => Ok(bytes.len() as u64),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        }
    }

    fn mem_unmap(&self, caller: &CallerContext<'_>, base: u64, len: usize) -> SyscallResult {
        // The dispatcher already validated `base` and that `len` fits in
        // `usize`. A zero-length range names nothing; reject it before
        // touching any state.
        if len == 0 {
            return Err(Errno::LengthOutOfRange);
        }
        // Hand the range to the installed producer, which zeroes the frames
        // it reclaims and fails closed when `(base, len)`
        // does not name a region the caller mapped. The default
        // `NULL_MEM_MAP` fails closed with `NotImplemented`. Success reports
        // `Ok(0)` — the `Errno`-return ABI shape (`mem_unmap` returns an
        // error code, not a value).
        let result = self.mem_map.unmap(base, len).map(|()| 0);
        // The unmap shrank the caller's live space; credit the page-rounded
        // size back to the task's address-space accounting (the same figure
        // `mem_map` charged) and re-freeze the registry snapshot so the freed
        // pages are dropped from it too — leaving them in the stale snapshot
        // would let the copy path read or write memory the task no longer
        // owns (fail closed, never expose freed memory). Only on success: a
        // failed unmap left the mappings — and the accounting — unchanged.
        // The credit saturates at zero, so a `len` that rounds larger than
        // the live total can never underflow into a bogus huge usage.
        if result.is_ok() {
            let credited = (len as u64)
                .div_ceil(PAGE_SIZE as u64)
                .saturating_mul(PAGE_SIZE as u64);
            self.aspaces.write().credit_anon(caller.task_id, credited);
            self.refreeze_caller_aspace(caller);
        }
        result
    }

    fn wait(&self, caller: &CallerContext<'_>, pid: i32, status: u64) -> SyscallResult {
        // The dispatcher already validated that `pid` is a sign-extended
        // `i32` and that `status` is a non-null `UserPtr`. Hand the request
        // to the installed scheduler-side producer, which validates the
        // parent/child relationship (a process may only reap its own
        // children), blocks the caller until a child
        // is reapable, and reports the reaped child. Until one is installed
        // the default `NULL_PROCESS_WAIT` fails closed with `NotImplemented`, never fabricating a reaped child — the
        // process-wait analogue of `NULL_MEM_MAP` / `NULL_PROCESS_SPAWN`.
        let reaped = self.process_wait.wait(caller.task_id, pid)?;

        // Copy the child's exit code out to the caller's `status` pointer
        // through the validated `copy_to_user` boundary
        // *before* reporting success, so a faulting `status` is the same
        // fail-closed `BadAddress` an actual fault produces and never leaks
        // which case occurred. `with_caller_aspace` yields `None`
        // when the caller has no registered address space; fail closed.
        let status_bytes = reaped.code.to_ne_bytes();
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(status), &status_bytes)
        }) {
            Some(Ok(())) => Ok(u64::from(reaped.pid)),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        }
    }

    fn rlimit_get(&self, caller: &CallerContext<'_>, kind: u32, out: u64) -> SyscallResult {
        // validate `kind` against the closed abi-v1 set before
        // touching any state. An unassigned discriminant fails closed with
        // `OutOfRange` rather than indexing past the limit set.
        let kind = LimitKind::from_u32(kind)?;

        // Read the caller's *own* effective limit (the default policy until
        // one is imposed). Reading one's own limit grants no authority and
        // needs no capability; the dispatcher
        // leaves this call ungated and unaudited.
        let limit = self.aspaces.read().limits(caller.task_id).get(kind);
        let encoded = limit.encode();

        // Copy the encoded limit out to the caller's `out` pointer through
        // the validated `copy_to_user` boundary. A caller
        // with no registered address space (a kernel task, or one withdrawn
        // on `exit`) and any copy fault both collapse onto `BadAddress`,
        // never leaking which case occurred.
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(out), &encoded)
        }) {
            Some(Ok(())) => Ok(0),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        }
    }

    fn rlimit_set(&self, caller: &CallerContext<'_>, kind: u32, value: u64) -> SyscallResult {
        // validate `kind` before touching any state.
        let kind = LimitKind::from_u32(kind)?;

        // Copy the requested limit in from the caller's `value` pointer
        // through the validated `copy_from_user` boundary
        // *before* applying any policy. A caller with no registered address
        // space and any copy fault collapse onto the same fail-closed
        // `BadAddress`.
        let mut buf = [0u8; ResourceLimit::WIRE_LEN];
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_in(space, physmap, VirtAddr::new(value), &mut buf)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }
        // `decode` validates `soft <= hard` and fails closed on a malformed
        // pair, so a hostile buffer never yields a usable limit.
        let requested = ResourceLimit::decode(&buf)?;

        // lowering (or any change that does not raise the hard bound)
        // is free; raising the hard bound above the current ceiling requires
        // `CAP_RLIMIT_RAISE`. `authorize_set` returns `PermissionDenied`
        // otherwise, fail closed. This call is audited per spec, so
        // the dispatcher logs a rejection automatically — no
        // bespoke audit record is needed here.
        let current = self.aspaces.read().limits(caller.task_id).get(kind);
        let can_raise = caller.caps.has(CapabilityId::RLIMIT_RAISE);
        let stored = authorize_set(current, requested, can_raise)?;

        // Commit the authorised limit to the caller's own per-task set. The
        // task is identified by the kernel-trusted `caller.task_id`, never a
        // caller-supplied id, so a process can only set its own
        // limits.
        self.aspaces.write().set_limit(caller.task_id, kind, stored);
        Ok(0)
    }

    fn users_db_read(&self, caller: &CallerContext<'_>, buf: u64, len: usize) -> SyscallResult {
        // The dispatcher already checked `CAP_USERS_READ` and that `buf` is
        // a non-null `UserPtr`. Resolve the held database first: a build
        // with no holder wired fails closed with `NotImplemented`, and a
        // wired holder with no database fails closed with `NotFound`
        // (a system without accounts refuses
        // every login rather than inventing one).
        let text = self.users_db.text()?;

        // The whole text or nothing: a credential database is never
        // truncated to fit an undersized buffer. The
        // format's own 64 KiB maximum bounds the copy, so a conforming
        // caller's `MAX_DB_LEN` buffer always suffices.
        if text.len() > len {
            return Err(Errno::BufferTooSmall);
        }

        // Copy the text out through the validated `copy_to_user` boundary. A faulting pointer — or a caller with no
        // registered address space — fails closed with `BadAddress`, never
        // leaking which case occurred.
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(buf), text)
        }) {
            Some(Ok(())) => Ok(text.len() as u64),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        }
    }

    fn hw_tree_read(&self, caller: &CallerContext<'_>, buf: u64, len: usize) -> SyscallResult {
        // The dispatcher already checked `CAP_SYSINFO_HW` and that `buf` is
        // a non-null `UserPtr`. Take one wire-encoded snapshot — header +
        // nodes, read together so the reported generation matches the
        // nodes. A build with no store wired fails
        // closed with `NotImplemented`.
        let blob = self.hw_tree.snapshot()?;

        // The whole snapshot or nothing: the inventory is never truncated
        // to fit an undersized buffer — the caller grows its buffer and
        // retries.
        if blob.len() > len {
            return Err(Errno::BufferTooSmall);
        }

        // Copy the bytes out through the validated `copy_to_user` boundary. A faulting pointer — or a caller with no
        // registered address space — fails closed with `BadAddress`,
        // never leaking which case occurred.
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(buf), &blob)
        }) {
            Some(Ok(())) => Ok(blob.len() as u64),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        }
    }

    fn hw_tree_wait(
        &self,
        caller: &CallerContext<'_>,
        last_generation: u64,
        timeout_ns: u64,
    ) -> SyscallResult {
        // The dispatcher already checked `CAP_SYSINFO_HW`. Block the caller
        // until the store's generation differs from the one it last
        // observed, or the deadline elapses. The caller *parks* off the run
        // queue (no busy yield); it is woken either by
        // the `HwTreeSource` store on a generation bump
        // (`crate::waitq::hw_tree_wake`) or, with a finite timeout, by the
        // architecture one-shot's per-tick sweep (`crate::waitq::timed_wake_sweep`),
        // and re-checks its condition after every wake.
        //
        // The deadline is one saturating add from the first clock reading,
        // so a `u64::MAX` timeout stays `NO_DEADLINE` (no timed wake, only
        // an explicit one) rather than wrapping to a tiny value (fail closed); `monotonic_ns` is non-decreasing per CPU
        // and the handler does not migrate mid-wait.
        let cpu = SchedulerArch::current_cpu(self.arch);
        let task = caller.task_id.0;

        // Fast paths, checked before registering so the common
        // already-changed / zero-timeout / no-store cases allocate nothing
        // and never touch the wait-queue. A build with
        // no store wired fails closed with `NotImplemented` (`?`).
        let now = self.arch.monotonic_ns(cpu);
        let deadline_ns = now.saturating_add(timeout_ns);
        if self.hw_tree.generation()? != last_generation {
            return Ok(0);
        }
        if deadline_ns != crate::waitq::NO_DEADLINE && now >= deadline_ns {
            return Err(Errno::TimedOut);
        }

        // Must block: register so a waker can find and `unpark` us, then
        // loop check → arm one-shot → park. The wake-pending token in the
        // scheduler closes the check/park race, so a generation bump or
        // timeout arriving in that window is never lost.
        crate::waitq::HW_TREE_WAITQ.register(task, deadline_ns);
        let result = loop {
            let generation = match self.hw_tree.generation() {
                Ok(g) => g,
                Err(err) => break Err(err),
            };
            if generation != last_generation {
                break Ok(0);
            }
            if deadline_ns != crate::waitq::NO_DEADLINE
                && self.arch.monotonic_ns(cpu) >= deadline_ns
            {
                break Err(Errno::TimedOut);
            }
            // Arm the timed-wake one-shot to the nearest pending deadline so
            // a finite timeout fires even on an otherwise-idle CPU
            // (the nearest armed wakeup).
            self.arch
                .set_wakeup(crate::waitq::HW_TREE_WAITQ.earliest_deadline());
            // Park off the run queue until woken. `reschedule_current`
            // returns `false` only when the caller is not a resumable user
            // kthread (host tests with no live dispatch loop); fall back to
            // a cooperative yield then, mirroring the IRQ-wait fail-closed
            // shape, so a degenerate caller never busy-spins.
            if !crate::kthread::reschedule_current(cpu, RescheduleAction::Park) {
                match self.sched.yield_current(task) {
                    Ok(()) | Err(SchedError::InvalidState) => {}
                    Err(SchedError::NoSuchTask) => break Err(Errno::NotFound),
                    Err(_) => break Err(Errno::OutOfRange),
                }
            }
        };
        // Leave the wait set and re-point the one-shot at whatever deadline
        // any *remaining* waiter needs (or clear it if none) so a finished
        // wait never leaves a stale arming behind.
        crate::waitq::HW_TREE_WAITQ.deregister(task);
        self.arch
            .set_wakeup(crate::waitq::HW_TREE_WAITQ.earliest_deadline());
        result
    }

    fn users_db_wait(&self, caller: &CallerContext<'_>, timeout_ns: u64) -> SyscallResult {
        // The dispatcher already checked `CAP_USERS_READ`. Block the caller
        // while the user database is still *pending* — a real holder is
        // being unlocked but has not yet been published or given up on
        // (`UsersDbSource::is_pending`, the `Errno::WouldBlock` signal) — or
        // until the deadline elapses. The caller *parks* off the run queue
        // (no busy yield, the bug this syscall fixes:
        // `login` previously re-read `users_db_read` in a yield loop,
        // flooding the audit log with one ERROR per poll). It is woken by
        // `crate::waitq::users_db_wake` the instant the unlock reaches a
        // terminal outcome (`LateUsersDb::install`/`resolve`) or, with a
        // finite timeout, by the architecture one-shot's per-tick sweep
        // (`crate::waitq::timed_wake_sweep`), and re-checks the pending
        // condition after every wake.
        //
        // The deadline is one saturating add from the first clock reading,
        // so a `u64::MAX` timeout stays `NO_DEADLINE` (no timed wake, only
        // an explicit one) rather than wrapping to a tiny value (fail closed); `monotonic_ns` is non-decreasing per CPU
        // and the handler does not migrate mid-wait.
        let cpu = SchedulerArch::current_cpu(self.arch);
        let task = caller.task_id.0;

        // Fast paths, checked before registering so the common
        // already-resolved / zero-timeout cases allocate nothing and never
        // touch the wait-queue. A build with no
        // users-database service wired is never pending, so the wait returns
        // immediately and the subsequent `users_db_read` fails closed with
        // `NotImplemented`.
        let now = self.arch.monotonic_ns(cpu);
        let deadline_ns = now.saturating_add(timeout_ns);
        if !self.users_db.is_pending() {
            return Ok(0);
        }
        if deadline_ns != crate::waitq::NO_DEADLINE && now >= deadline_ns {
            return Err(Errno::TimedOut);
        }

        // Must block: register so a waker can find and `unpark` us, then
        // loop check → arm one-shot → park. The wake-pending token in the
        // scheduler closes the check/park race, so a terminal unlock outcome
        // or timeout arriving in that window is never lost — mirrors `hw_tree_wait` exactly.
        crate::waitq::USERS_DB_WAITQ.register(task, deadline_ns);
        let result = loop {
            if !self.users_db.is_pending() {
                break Ok(0);
            }
            if deadline_ns != crate::waitq::NO_DEADLINE
                && self.arch.monotonic_ns(cpu) >= deadline_ns
            {
                break Err(Errno::TimedOut);
            }
            // Arm the timed-wake one-shot to the nearest pending deadline so
            // a finite timeout fires even on an otherwise-idle CPU
            // (the nearest armed wakeup).
            self.arch
                .set_wakeup(crate::waitq::USERS_DB_WAITQ.earliest_deadline());
            // Park off the run queue until woken. `reschedule_current`
            // returns `false` only when the caller is not a resumable user
            // kthread (host tests with no live dispatch loop); fall back to
            // a cooperative yield then, mirroring `hw_tree_wait`, so a
            // degenerate caller never busy-spins.
            if !crate::kthread::reschedule_current(cpu, RescheduleAction::Park) {
                match self.sched.yield_current(task) {
                    Ok(()) | Err(SchedError::InvalidState) => {}
                    Err(SchedError::NoSuchTask) => break Err(Errno::NotFound),
                    Err(_) => break Err(Errno::OutOfRange),
                }
            }
        };
        // Leave the wait set and re-point the one-shot at whatever deadline
        // any *remaining* waiter needs (or clear it if none) so a finished
        // wait never leaves a stale arming behind.
        crate::waitq::USERS_DB_WAITQ.deregister(task);
        self.arch
            .set_wakeup(crate::waitq::USERS_DB_WAITQ.earliest_deadline());
        result
    }

    fn ipc_call(
        &self,
        caller: &CallerContext<'_>,
        endpoint: u64,
        request: u64,
        request_len: usize,
        reply: u64,
        reply_cap: usize,
    ) -> SyscallResult {
        // resolve the call endpoint against the kernel call-endpoint
        // registry before touching the caller's buffers. An endpoint that
        // is not bound fails closed with `NotFound`; the dispatcher's
        // standard pipeline audits the rejection at this boundary (the
        // registry lookup, like `PortRegistry::lookup`, does not). A build
        // whose kthread server never registered the endpoint therefore
        // fails closed rather than blocking.
        let Some(ep) = crate::callreg::lookup(EndpointId(endpoint)) else {
            return Err(Errno::NotFound);
        };

        // A grant-restricted endpoint (one that requires `CAP_IPC_ENDPOINT`
        // of its senders) is reachable only by a task the kernel granted the
        // matching per-endpoint resource. This is what keeps two class
        // drivers behind one host controller from reaching each other's
        // transport endpoint even though both hold the class capability: the
        // per-endpoint grant is minted only onto the driver whose matched
        // node carried this endpoint. Checked against the kernel-trusted
        // caller id before any buffer is copied, and fails closed — the
        // endpoint's own `post` still re-checks the class capability.
        if ep
            .required_send_caps()
            .contains(rustos_abi::CapabilityId::IPC_ENDPOINT)
            && !self
                .aspaces
                .read()
                .grant_covers(caller.task_id, &HwResource::endpoint(endpoint))
        {
            return Err(Errno::PermissionDenied);
        }

        // Bound the request copy *before* allocating: refuse a payload
        // larger than the endpoint advertises (itself capped at
        // `IPC_MESSAGE_MAX_PAYLOAD_LEN` at create time). The same
        // `MessageTooLarge` code `CallEndpoint::post` would return, made
        // cheap to reject here.
        if request_len as u64 > u64::from(ep.max_request()) {
            return Err(Errno::MessageTooLarge);
        }

        // Copy the request in from the caller's address space through the
        // validated `copy_from_user` boundary. The bytes
        // are staged in a kernel-owned buffer; `CallEndpoint::post` then
        // takes its own copy, so the caller cannot mutate the request after
        // it is posted. `with_caller_aspace` yields `None` when the caller
        // has no registered address space — fail closed with the same
        // `BadAddress` a fault produces, never leaking which case occurred.
        let mut payload = alloc::vec![0u8; request_len];
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_in(space, physmap, VirtAddr::new(request), &mut payload)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }

        // Post the request. `CallEndpoint::post` performs the per-call
        // capability check against the caller's effective set (no ambient authority) and re-checks the size, returning a
        // stable `Errno` for every refusal and otherwise an opaque ticket
        // correlating this caller with its reply.
        let ticket = ep.post(caller.caps, &payload, self.audit)?;

        // Wake the bound server: an in-kernel IPC-server kthread parks off
        // the run queue on `SERVE_WAITQ` between requests (no busy-yield), so the posted request must unpark it or the
        // call would block until some unrelated wake. The server re-checks
        // its endpoint after every wake; the scheduler's wake-pending token
        // closes the post/park race so a request posted while the server is
        // mid-commit-to-park is never lost.
        crate::waitq::serve_wake();

        // Block until the bound server replies, parking off the run queue
        // (no busy yield). `ipc_call` carries no timeout,
        // so register with `NO_DEADLINE`: the caller is woken only by the
        // server's reply (`crate::waitq::call_wake`) or the endpoint's
        // destruction, and re-checks its ticket after every wake. The
        // wake-pending token in the scheduler closes the check/park race so
        // a reply arriving in that window is never lost.
        //
        // `take_reply` is matched by `claimant`, the *security* task id the
        // request was posted under (`caller.caps.task()`), while the
        // wait-queue and the scheduler park/unpark use the *scheduler* task
        // id (`caller.task_id`), exactly as `hw_tree_wait` does.
        let cpu = SchedulerArch::current_cpu(self.arch);
        let sched_task = caller.task_id.0;
        let claimant = caller.caps.task().0;
        crate::waitq::CALL_WAITQ.register(sched_task, crate::waitq::NO_DEADLINE);
        let outcome = loop {
            match ep.take_reply(claimant, ticket) {
                ReplyOutcome::Ready(bytes) => break Ok(bytes),
                // The endpoint was torn down, or the ticket is no longer
                // ours: abandon the call fail-closed.
                ReplyOutcome::Cancelled | ReplyOutcome::Unknown => break Err(Errno::NotFound),
                ReplyOutcome::Pending => {
                    // Park off the run queue until woken. `reschedule_current`
                    // returns `false` only when the caller is not a resumable
                    // user kthread (host tests with no live dispatch loop);
                    // fall back to a cooperative yield then, mirroring
                    // `hw_tree_wait`, so a degenerate caller never busy-spins.
                    if !crate::kthread::reschedule_current(cpu, RescheduleAction::Park) {
                        match self.sched.yield_current(sched_task) {
                            Ok(()) | Err(SchedError::InvalidState) => {}
                            Err(SchedError::NoSuchTask) => break Err(Errno::NotFound),
                            Err(_) => break Err(Errno::OutOfRange),
                        }
                    }
                }
            }
        };
        crate::waitq::CALL_WAITQ.deregister(sched_task);

        let bytes = outcome?;

        // Refuse to truncate: a reply larger than the caller's buffer fails
        // closed. The reply was already claimed and the
        // ticket retired, so the caller must re-issue the call with a larger
        // buffer — each endpoint's protocol bounds its replies, so a
        // correctly-sized buffer always fits.
        if bytes.len() > reply_cap {
            return Err(Errno::BufferTooSmall);
        }

        // Copy the reply out through the validated `copy_to_user` boundary. A faulting pointer — or a caller with no
        // registered address space — fails closed with `BadAddress`.
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(reply), &bytes)
        }) {
            Some(Ok(())) => Ok(bytes.len() as u64),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        }
    }

    // Six `abi-v1` arguments plus the kernel-trusted caller context — the
    // syscall's own shape, not an accidental parameter pile (justified allow, matching the trait declaration).
    #[allow(clippy::too_many_arguments)]
    fn call_create(
        &self,
        caller: &CallerContext<'_>,
        endpoint: u64,
        send_caps: u64,
        recv_caps: u64,
        max_request: usize,
        max_reply: usize,
        capacity: usize,
    ) -> SyscallResult {
        // Bound the payload caps to the ABI register width before touching
        // anything else (cheap reject; `CallEndpoint::create` re-bounds them
        // against `IPC_MESSAGE_MAX_PAYLOAD_LEN`). `usize` → `u32` is the
        // only narrowing; a request/reply cap beyond `u32` is malformed.
        let Ok(max_request) = u32::try_from(max_request) else {
            return Err(Errno::LengthOutOfRange);
        };
        let Ok(max_reply) = u32::try_from(max_reply) else {
            return Err(Errno::LengthOutOfRange);
        };

        // Copy both `CapabilitySet` wire images in through the validated
        // `copy_from_user` boundary before any state is
        // touched. A caller with no registered address space and any copy
        // fault both collapse onto `BadAddress`, never leaking which case
        // occurred.
        let mut send_buf = [0u8; CapabilitySet::WIRE_LEN];
        let mut recv_buf = [0u8; CapabilitySet::WIRE_LEN];
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_in(space, physmap, VirtAddr::new(send_caps), &mut send_buf)?;
            copy_in(space, physmap, VirtAddr::new(recv_caps), &mut recv_buf)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }
        // Every 32-byte pattern is a representable set; decoding cannot fail.
        let send_set = CapabilitySet::from_le_bytes(&send_buf)?;
        let recv_set = CapabilitySet::from_le_bytes(&recv_buf)?;

        // A grant-restricted endpoint declares its authority by requiring the
        // generic per-endpoint capability `CAP_IPC_ENDPOINT` of its senders:
        // holding the class is necessary but not sufficient — only a task the
        // kernel also grants the matching per-endpoint resource may call it.
        let grant_restricted = send_set.contains(rustos_abi::CapabilityId::IPC_ENDPOINT);
        let endpoint_id = endpoint;

        // Build the endpoint owned by the calling task. `CallEndpoint::create`
        // runs the bind-time authority checks against the caller's effective
        // set *before* the endpoint exists: the
        // creator must hold every `recv` capability it requires, and must
        // hold `CAP_IPC_BIND_PRIVILEGED` to bind a restricted-sender endpoint.
        let endpoint = CallEndpoint::create(
            EndpointId(endpoint_id),
            caller.caps,
            send_set,
            recv_set,
            CallEndpointLimits {
                max_request,
                max_reply,
                capacity,
            },
            self.audit,
        )?;
        // Publish it so the `ipc_call` handler can resolve callers to it. A
        // live id is never silently re-pointed: a clash fails closed with
        // `AlreadyExists` and the freshly built endpoint is dropped.
        crate::callreg::register(alloc::sync::Arc::new(endpoint))?;

        // Mint the creator the per-endpoint grant for a grant-restricted
        // endpoint — mirroring `msi_alloc`'s grant for an allocated IRQ line.
        // The server gains a grant only for an endpoint it itself owns (no
        // ambient authority), so it may forward the endpoint onto a device
        // node it publishes (`hw_emit_node`'s coverage check tests against
        // exactly this grant) and the autoloaded class driver inherits it as
        // its sole reach. Minted only after the endpoint is registered, so a
        // clashing id leaves no orphan grant behind.
        if grant_restricted {
            let _ = self
                .aspaces
                .write()
                .mint_grant(caller.task_id, HwResource::endpoint(endpoint_id));
        }
        Ok(0)
    }

    fn call_recv(
        &self,
        caller: &CallerContext<'_>,
        endpoint: u64,
        buf: u64,
        buf_cap: usize,
        ticket_out: u64,
    ) -> SyscallResult {
        // resolve the endpoint, then gate the *server* against the
        // endpoint's required receive capability and confirm it is the owning
        // task — both before any state is touched. A foreign or
        // insufficiently-capable task is denied (no
        // ambient authority); an unknown endpoint fails closed.
        let Some(ep) = crate::callreg::lookup(EndpointId(endpoint)) else {
            return Err(Errno::NotFound);
        };
        if !ep
            .required_recv_caps()
            .is_subset_of(caller.caps.effective())
            || ep.owner() != caller.caps.task().0
        {
            return Err(Errno::PermissionDenied);
        }

        // Block until a request fits and is dequeued, parking off the run
        // queue between polls (no busy yield). Register on
        // `SERVE_WAITQ` *before* the first poll so a request posted in the
        // register/park window is not lost: the `ipc_call` handler's
        // `serve_wake` unparks this task, and the scheduler's wake-pending
        // token closes the poll/park race (the same interlock `ipc_call` uses
        // on `CALL_WAITQ`).
        let cpu = SchedulerArch::current_cpu(self.arch);
        let sched_task = caller.task_id.0;
        crate::waitq::SERVE_WAITQ.register(sched_task, crate::waitq::NO_DEADLINE);
        let received = loop {
            // The owner was torn down, or the endpoint destroyed: abandon the
            // receive fail-closed.
            if ep.is_closed() {
                break Err(Errno::NotFound);
            }
            match ep.recv_call(buf_cap) {
                RecvCall::Received(call) => break Ok(call),
                // The front request does not fit: leave it queued and report
                // closed. The server resizes and retries.
                RecvCall::TooLarge { .. } => break Err(Errno::BufferTooSmall),
                RecvCall::Empty => {
                    if !crate::kthread::reschedule_current(cpu, RescheduleAction::Park) {
                        match self.sched.yield_current(sched_task) {
                            Ok(()) | Err(SchedError::InvalidState) => {}
                            Err(SchedError::NoSuchTask) => break Err(Errno::NotFound),
                            Err(_) => break Err(Errno::OutOfRange),
                        }
                    }
                }
            }
        };
        crate::waitq::SERVE_WAITQ.deregister(sched_task);

        let call = received?;

        // Deliver the request bytes and the ticket through the validated
        // `copy_to_user` boundary. The call has already been moved into the
        // in-service table, so a faulting server buffer cannot leave a ticket
        // the server never saw blocking its caller forever: reply-cancel the
        // call with an empty reply (releasing the parked caller fail-closed,
        // its protocol decoder rejects the truncated reply) and surface
        // `BadAddress` to the server.
        let ticket_bytes = call.ticket.0.to_le_bytes();
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(buf), &call.request)?;
            copy_out(space, physmap, VirtAddr::new(ticket_out), &ticket_bytes)
        }) {
            Some(Ok(())) => Ok(call.request.len() as u64),
            Some(Err(err)) => {
                let _ = ep.reply(call.ticket, &[], self.audit);
                crate::waitq::call_wake();
                Err(copy_fault_errno(err))
            }
            None => {
                let _ = ep.reply(call.ticket, &[], self.audit);
                crate::waitq::call_wake();
                Err(Errno::BadAddress)
            }
        }
    }

    fn call_reply(
        &self,
        caller: &CallerContext<'_>,
        endpoint: u64,
        ticket: u64,
        reply: u64,
        reply_len: usize,
    ) -> SyscallResult {
        // resolve + gate before touching state, exactly as `call_recv`.
        let Some(ep) = crate::callreg::lookup(EndpointId(endpoint)) else {
            return Err(Errno::NotFound);
        };
        if !ep
            .required_recv_caps()
            .is_subset_of(caller.caps.effective())
            || ep.owner() != caller.caps.task().0
        {
            return Err(Errno::PermissionDenied);
        }

        // Bound the reply copy before allocating: refuse a reply larger than
        // the endpoint advertises (the same `MessageTooLarge` `reply` would
        // return, made cheap to reject here).
        if reply_len as u64 > u64::from(ep.max_reply()) {
            return Err(Errno::MessageTooLarge);
        }
        let mut payload = alloc::vec![0u8; reply_len];
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_in(space, physmap, VirtAddr::new(reply), &mut payload)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }

        // Complete the ticket and wake the caller blocked in `ipc_call` for
        // it. `reply` re-checks the ticket and size and fails closed on an
        // unknown/already-answered ticket.
        ep.reply(CallTicket(ticket), &payload, self.audit)?;
        crate::waitq::call_wake();
        Ok(0)
    }

    fn call_peer_origin(
        &self,
        caller: &CallerContext<'_>,
        endpoint: u64,
        ticket: u64,
        origin: u64,
        origin_cap: usize,
    ) -> SyscallResult {
        // Resolve + gate before touching state, exactly as `call_recv` /
        // `call_reply`: the reader must hold the endpoint's required receive
        // capability and be the owning task. A foreign or insufficiently
        // capable task is denied (no ambient authority); an unknown endpoint
        // fails closed.
        let Some(ep) = crate::callreg::lookup(EndpointId(endpoint)) else {
            return Err(Errno::NotFound);
        };
        if !ep
            .required_recv_caps()
            .is_subset_of(caller.caps.effective())
            || ep.owner() != caller.caps.task().0
        {
            return Err(Errno::PermissionDenied);
        }

        // The reader's buffer must hold a whole origin; a short buffer fails
        // closed rather than truncating the record.
        if origin_cap < rustos_abi::ORIGIN_WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }

        // Look up the kernel-attested origin captured for this in-service
        // ticket. An unknown, still-pending, already-replied, or foreign
        // ticket resolves to nothing and fails closed: a server learns a
        // caller's identity only while it is actively servicing that caller's
        // call. The origin was snapshotted from the caller's own task state at
        // post time, so it cannot be forged on the wire.
        let Some(peer) = ep.peer_origin(CallTicket(ticket)) else {
            return Err(Errno::NotFound);
        };
        let bytes = peer.to_le_bytes();
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(origin), &bytes)
        }) {
            Some(Ok(())) => Ok(bytes.len() as u64),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        }
    }

    fn log_emit(&self, caller: &CallerContext<'_>, record: u64, len: usize) -> SyscallResult {
        // The dispatcher has already checked `CAP_LOG_EMIT` and that
        // `record` is a non-null `UserPtr`. A valid
        // encoded record never exceeds `LOG_RECORD_MAX`, so a larger `len`
        // is malformed and is rejected before copying — a hostile `len`
        // cannot drive a large kernel allocation.
        if len == 0 || len > LOG_RECORD_MAX {
            return Err(Errno::LengthOutOfRange);
        }

        // Copy the encoded record in through the validated boundary before
        // touching the sink. A faulting pointer — or a caller with no
        // registered address space — fails closed with `BadAddress`, never
        // an oracle distinguishing the cause.
        let mut payload = alloc::vec![0u8; len];
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_in(space, physmap, VirtAddr::new(record), &mut payload)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }

        // Fully validate the record (lengths, slice bounds, UTF-8) before
        // building an event from it.
        let decoded = decode_log_record(&payload)?;

        // Attribute the record to the calling task with a kernel-supplied
        // `task` field the caller cannot forge — the trusted origin marker. It is prepended to the caller's own
        // fields, which the decoder bounds to `LOG_FIELDS_MAX`, so the
        // fixed array never overflows.
        let mut fields_buf = [Field {
            key: "",
            value: rustos_log::FieldValue::Null,
        }; LOG_FIELDS_MAX + 1];
        fields_buf[0] = Field {
            key: "task",
            value: rustos_log::FieldValue::UnsignedInt(caller.task_id.0),
        };
        let mut field_count = 1;
        for (key, value) in decoded.fields() {
            fields_buf[field_count] = Field { key, value };
            field_count += 1;
        }

        // The level byte was validated `<= LOG_LEVEL_MAX` by the decoder, so
        // `from_u8` always succeeds; `unwrap_or` keeps the path panic-free.
        let event = Event {
            level: Level::from_u8(decoded.level()).unwrap_or(Level::Info),
            id: EventId(decoded.event_id()),
            message: decoded.message(),
            fields: &fields_buf[..field_count],
        };
        // Emit through the kernel's diagnostic sink only — never the audit
        // sink. Below the active level threshold the
        // record is dropped in O(1).
        rustos_log::log(self.log_sink, &event);
        Ok(0)
    }

    fn hw_emit_node(&self, caller: &CallerContext<'_>, node: u64, len: usize) -> SyscallResult {
        // The dispatcher has already checked `CAP_HW_EMIT` and that `node`
        // is a non-null `UserPtr`. A wire-encoded
        // `HwNode` is exactly `HwNode::WIRE_LEN` bytes, so any other `len`
        // is malformed and is rejected before copying — a hostile `len`
        // cannot drive a large copy, and a short one cannot decode.
        if len != rustos_abi::HwNode::WIRE_LEN {
            return Err(Errno::LengthOutOfRange);
        }

        // Copy the encoded node in through the validated boundary before
        // touching any state. A faulting pointer — or a caller with no
        // registered address space — fails closed with `BadAddress`, never
        // an oracle distinguishing the cause. The buffer
        // is a fixed `WIRE_LEN` stack array (no allocation).
        let mut bytes = [0u8; rustos_abi::HwNode::WIRE_LEN];
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_in(space, physmap, VirtAddr::new(node), &mut bytes)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }

        // Fully decode and validate the node (lengths, discriminants,
        // bounded match-key / resource counts) before touching state.
        let decoded = rustos_abi::HwNode::from_bytes(&bytes)?;

        // Security spine of recursive, user-space hardware discovery
        // (no ambient authority;). Two checks, both
        // against kernel-trusted state keyed by `caller.task_id` (never a
        // caller-supplied value), under one read guard held only for
        // the duration of these checks:
        //
        // 1. The caller must be an autoloaded driver bound to a matched node
        //    — its *own* node, the parent the published child is attached
        //    under. A task with no loaded node (an ordinary process, or a
        //    driver not loaded for a node) may publish nothing: it cannot
        //    name a position in the tree, so it fails closed. This is what
        //    makes the tree topology trustworthy — a driver cannot forge its
        //    parent.
        // 2. Every resource the child requests must be covered by one of the
        //    caller's *own* minted grants, so an autoloaded child driver can
        //    never be granted authority its emitter lacks. One uncovered
        //    resource fails the whole publish closed (never partially apply).
        let parent_id = {
            let aspaces = self.aspaces.read();
            let Some(parent_id) = aspaces.loaded_node(caller.task_id) else {
                return Err(Errno::PermissionDenied);
            };
            for resource in decoded.resources() {
                if !aspaces.grant_covers(caller.task_id, resource) {
                    return Err(Errno::PermissionDenied);
                }
            }
            parent_id
        };

        // Publish under the emitter's own node into the live hardware tree,
        // bumping the generation that wakes the device manager's reactive
        // autoload (the same change channel `hw_tree_wait` observes). The
        // store owns identity: it assigns the published node a fresh,
        // collision-free id and sets its parent to `parent_id`, so an
        // emitter-chosen id can never collide with an existing node
        // (load-bearing, the driver-store load path
        // resolves a matched node by id). A build with no store wired fails
        // closed with `NotImplemented`. Returns the kernel-assigned node id
        // once published, so the emitter can later retract this child by id
        // (a USB host controller removing the interface node on a port-down).
        self.hw_tree.publish(parent_id, decoded).map(u64::from)
    }

    fn hw_remove_node(&self, caller: &CallerContext<'_>, node_id: u64) -> SyscallResult {
        // The dispatcher has already checked `CAP_HW_EMIT` — the same
        // privilege publishing requires, since removing a child is the exact
        // counterpart of emitting one. `node_id` is
        // a plain `u64`, copied in nothing: a `HwNode::id` is a `u32`, so a
        // value outside that range names no node and fails closed at the
        // resolution below — never an out-of-band copy.
        let Ok(node_id) = u32::try_from(node_id) else {
            return Err(Errno::NotFound);
        };

        // Security spine, identical to `hw_emit_node` (no
        // ambient authority): resolve the caller's *own* matched node from
        // kernel-trusted state keyed by `caller.task_id` (never a
        // caller-supplied value). A task with no loaded node (an
        // ordinary process, or a driver not loaded for a node) owns nothing in
        // the tree and may remove nothing — it fails closed. The store then
        // removes `node_id` only when its parent is exactly this node, so a
        // driver can never retire a node it did not itself publish.
        let parent_id = {
            let aspaces = self.aspaces.read();
            let Some(parent_id) = aspaces.loaded_node(caller.task_id) else {
                return Err(Errno::PermissionDenied);
            };
            parent_id
        };

        // Remove the child (and its whole subtree) from the live hardware
        // tree, bumping the generation that wakes the device manager's
        // reactive watch so it unloads the driver bound to the vanished node
        // (the same change channel `hw_tree_wait` observes). The store enforces the ownership check (`node_id`'s parent
        // must be `parent_id`) and fails closed `NotFound` otherwise; a build
        // with no store wired fails closed `NotImplemented`. Returns `Ok(0)` once removed (the `Errno`-return ABI shape).
        self.hw_tree.remove(parent_id, node_id).map(|()| 0)
    }

    fn msi_alloc(&self, caller: &CallerContext<'_>, out: u64, out_len: usize) -> SyscallResult {
        // Step 2 (capability) was enforced by the dispatcher: the `msi_alloc`
        // spec carries `CAP_IRQ_BIND`. Step 3 (validate every input): the out
        // buffer must be able to hold the whole encoded record before
        // anything is allocated, so a short buffer fails closed without
        // consuming a vector.
        if out_len < rustos_abi::MsiAllocation::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        // Mechanism: the installed arch producer mints a free MSI vector,
        // brings the platform's MSI controller up if it is not already, and
        // builds the doorbell. The default `NULL_MSI_ALLOC_FACILITY` fails
        // closed with `NotImplemented` (a platform with no MSI controller);
        // an exhausted vector space surfaces as `OutOfRange`.
        let allocation = self.msi_alloc_facility.allocate()?;
        // Copy the encoded record out through the validated `copy_to_user`
        // boundary *before* minting the grant: a faulting `out` pointer fails
        // closed with `BadAddress` and the caller never learns the line, so
        // leaving the grant unminted keeps a faulting call from widening the
        // caller's authority.
        let bytes = allocation.to_le_bytes();
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(out), &bytes)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }
        // Grant the calling task a device resource for the allocated virtual
        // line, so it may both `irq_bind` it and forward it as an
        // `HwResource::irq` onto a child node it publishes (the `hw_emit_node`
        // grant-coverage check tests against exactly this). Minted against
        // `caller.task_id` (kernel-trusted), exactly like the driver-admission
        // grant path; the handle is unused here — the *line*, not a handle, is
        // what the driver presents to `irq_bind` and forwards.
        let _handle = self.aspaces.write().mint_grant(
            caller.task_id,
            HwResource::irq(u64::from(allocation.line), 1),
        );
        Ok(rustos_abi::MsiAllocation::WIRE_LEN as u64)
    }

    fn shm_create(&self, caller: &CallerContext<'_>, len: usize, id_out: u64) -> SyscallResult {
        // Step 2 (capability) was enforced by the dispatcher: the `shm_create`
        // spec carries `CAP_SHM`. Step 3 (validate every input): a zero-length
        // region names nothing; reject it before touching any state.
        if len == 0 {
            return Err(Errno::LengthOutOfRange);
        }
        let pages = (len as u64).div_ceil(PAGE_SIZE as u64);
        // Allocate + zero + map the region into the caller's own live space
        // through the installed shared-memory facility, and record it
        // (refs = 1) against the caller as owner. A build with no facility
        // wired holds the fail-closed default and returns `NotImplemented`.
        // On any failure no region is recorded and the frames (if any) are
        // returned to the allocator, so a failed create leaks nothing.
        let (base_va, id) =
            crate::sharedreg::create(self.shared_mem_facility, caller.task_id, pages)?;
        // The map grew the caller's live space; re-freeze the registry
        // snapshot so the `id_out` copy (and any later copy) sees current
        // memory, exactly as `mem_map` / `mmio_map` do.
        self.refreeze_caller_aspace(caller);
        // Write the kernel-minted region id out through the validated
        // `copy_to_user` boundary. A faulting `id_out` releases the region we
        // just created (dropping its only reference, which frees and scrubs
        // its frames) and fails closed, so a faulting call leaves no orphan
        // region behind and never widens authority.
        let id_bytes = id.to_le_bytes();
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(id_out), &id_bytes)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => {
                let _ = crate::sharedreg::unmap(self.shared_mem_facility, caller.task_id, base_va);
                self.refreeze_caller_aspace(caller);
                return Err(copy_fault_errno(err));
            }
            None => {
                let _ = crate::sharedreg::unmap(self.shared_mem_facility, caller.task_id, base_va);
                self.refreeze_caller_aspace(caller);
                return Err(Errno::BadAddress);
            }
        }
        // Grant the calling task the per-region resource, so it may forward
        // the region onto a child node it publishes (`hw_emit_node`'s
        // coverage check tests against exactly this) and an autoloaded class
        // driver inherits it as its sole reach. Minted against
        // `caller.task_id` (kernel-trusted), exactly like the `msi_alloc`
        // grant path; the handle is unused here.
        let _handle = self
            .aspaces
            .write()
            .mint_grant(caller.task_id, HwResource::shared(id));
        Ok(base_va)
    }

    fn shm_map(&self, caller: &CallerContext<'_>, handle: u64) -> SyscallResult {
        // Step 2 (capability) was enforced by the dispatcher: the `shm_map`
        // spec carries `CAP_SHM`. Step 3 (validate every input): resolve
        // `handle` to a granted resource **for the calling task**
        // (`caller.task_id` is kernel-trusted), so a forged or another
        // driver's handle resolves to nothing and is refused, exactly as
        // `mmio_map` / `dma_alloc` resolve their grants.
        let Some(resource) = self.aspaces.read().grant(caller.task_id, handle) else {
            return Err(Errno::NotFound);
        };
        // The grant must name a shared region; reject any other kind before
        // mapping (fail closed).
        if resource.kind() != Some(HwResourceKind::Shared) {
            return Err(Errno::OutOfRange);
        }
        // Map the region (its id is the grant's base) into the caller's own
        // live space and account the mapping so the region's frames are not
        // freed while the caller still maps them. A region torn down between
        // grant and map fails closed `NotFound`.
        let base_va =
            crate::sharedreg::map(self.shared_mem_facility, caller.task_id, resource.base())?;
        // The map grew the caller's live space; re-freeze its snapshot.
        self.refreeze_caller_aspace(caller);
        Ok(base_va)
    }

    fn shm_unmap(&self, caller: &CallerContext<'_>, base: u64, _len: usize) -> SyscallResult {
        // No capability: this releases only the caller's own mapping (the
        // `mem_unmap` posture). Resolve `(caller, base)` against the
        // per-task mapping records, tear down only that mapping's page-table
        // entries, and drop the caller's reference; the region's frames are
        // zeroed and freed at its last reference. A `base` that does not name
        // a live shared mapping of the caller fails closed `NotFound`.
        crate::sharedreg::unmap(self.shared_mem_facility, caller.task_id, base)?;
        // The unmap shrank the caller's live space; re-freeze its snapshot.
        self.refreeze_caller_aspace(caller);
        Ok(0)
    }

    fn waitset_create(&self, caller: &CallerContext<'_>) -> SyscallResult {
        // No capability (the dispatcher gates none): a wait-set observes only
        // resources the caller already holds, each owner-checked when it is
        // added. Mint a fresh handle and record an empty set owned by the
        // kernel-trusted `caller.task_id` (never a caller-supplied value), so
        // only this task can later add to, wait on, or have the set observed.
        Ok(crate::waitset::create(caller.task_id.0))
    }

    fn waitset_ctl(
        &self,
        caller: &CallerContext<'_>,
        set: u64,
        op: u32,
        kind: u32,
        id: u64,
        token: u64,
    ) -> SyscallResult {
        // Validate the scalar arguments (fail closed on an unknown op/kind)
        // before touching any state.
        let op = WaitSetOp::from_u32(op)?;
        let kind = WaitSourceKind::from_u32(kind)?;
        match op {
            WaitSetOp::Add => {
                // Resolve and owner-check the *resource* the member names
                // against the kernel-trusted caller before recording it: a
                // wait-set may observe only resources the caller already holds
                // (no ambient authority). A resource that is unknown or owned
                // by another task resolves to nothing and fails closed
                // `NotFound` — which never confirms a foreign resource's
                // existence. The registry then owner-checks the *set* and
                // refuses a duplicate `(kind, id)`.
                match kind {
                    WaitSourceKind::Endpoint => {
                        let owned = crate::callreg::lookup(EndpointId(id))
                            .is_some_and(|ep| ep.owner() == caller.task_id.0);
                        if !owned {
                            return Err(Errno::NotFound);
                        }
                    }
                    WaitSourceKind::Irq => {
                        if self
                            .irq
                            .line_for(IrqHandle::from_raw(id), caller.task_id)
                            .is_none()
                        {
                            return Err(Errno::NotFound);
                        }
                    }
                }
                crate::waitset::add(
                    caller.task_id.0,
                    set,
                    crate::waitset::Member { kind, id, token },
                )
                .map(|()| 0)
            }
            // Removing a member only edits the caller's own set; the registry
            // owner-checks the set and fails closed if the member is absent.
            WaitSetOp::Del => crate::waitset::remove(caller.task_id.0, set, kind, id).map(|()| 0),
        }
    }

    fn waitset_wait(
        &self,
        caller: &CallerContext<'_>,
        set: u64,
        timeout_ns: u64,
        token_out: u64,
    ) -> SyscallResult {
        // Owner-checked membership snapshot (a forged/foreign handle fails
        // closed `NotFound`). Membership can be mutated only by this same
        // task, which is about to park inside this call, so one snapshot is
        // stable for the whole wait.
        let members = crate::waitset::members(caller.task_id.0, set)?;

        let cpu = SchedulerArch::current_cpu(self.arch);
        let sched_task = caller.task_id.0;
        // One saturating add from the clock so a `u64::MAX` timeout stays
        // `NO_DEADLINE` (explicit wake only) rather than wrapping to a tiny
        // value (fail closed) — exactly as `irq_wait` computes it.
        let deadline_ns = self.arch.monotonic_ns(cpu).saturating_add(timeout_ns);

        // Register on both wake channels *before* the first scan so an event
        // arriving in the register/park window is not lost: `SERVE_WAITQ`
        // (an IPC request posted to a member endpoint, `NO_DEADLINE`) and
        // `IRQ_WAITQ` (a member line firing, plus the timed sweep that
        // enforces the timeout). `register` is idempotent.
        crate::waitq::SERVE_WAITQ.register(sched_task, crate::waitq::NO_DEADLINE);
        crate::waitq::IRQ_WAITQ.register(sched_task, deadline_ns);

        // `(kind, id, token)` of the ready member; `id`/`kind` drive the
        // post-write IRQ-edge consume.
        let outcome: Result<(WaitSourceKind, u64, u64), Errno> = loop {
            let now = self.arch.monotonic_ns(cpu);
            let mut ready: Option<(WaitSourceKind, u64, u64)> = None;
            // Scan in registration order; first ready wins. The IRQ ready
            // flag is *peeked* (`ready_for`), not consumed, here so a faulting
            // `token_out` below never drops a delivered edge. Each member is
            // re-checked against the caller as it is scanned, so a member whose
            // resource was torn down simply is not ready.
            for m in &members {
                let is_ready = match m.kind {
                    WaitSourceKind::Endpoint => crate::callreg::lookup(EndpointId(m.id))
                        .is_some_and(|ep| ep.owner() == sched_task && ep.has_pending()),
                    WaitSourceKind::Irq => {
                        let handle = IrqHandle::from_raw(m.id);
                        self.irq.line_for(handle, caller.task_id).is_some()
                            && self.irq.ready_for(handle)
                    }
                };
                if is_ready {
                    ready = Some((m.kind, m.id, m.token));
                    break;
                }
            }

            if let Some(found) = ready {
                break Ok(found);
            }
            if now >= deadline_ns {
                break Err(Errno::TimedOut);
            }

            // Re-arm every IRQ member's line before parking: a user-space
            // driver holds no controller access, so the kernel routes + unmasks
            // the line on its behalf (mask-before-wake). Best-effort and
            // idempotent — a refusal leaves the line as-is and the deadline
            // still bounds the wait.
            for m in &members {
                if m.kind == WaitSourceKind::Irq {
                    if let Some(line) = self.irq.line_for(IrqHandle::from_raw(m.id), caller.task_id)
                    {
                        let _ = self.irq_controller.rearm(line);
                    }
                }
            }
            // Arm the one-shot to the nearest pending `IRQ_WAITQ` deadline
            // (which includes this wait's), then *park* off the run queue until
            // woken by an endpoint post (`serve_wake`), a member line firing
            // (`irq_wake`), or the timed sweep — never a busy spin. The
            // wake-pending token closes the poll/park race exactly as
            // `call_recv` / `irq_wait` rely on.
            self.arch
                .set_wakeup(crate::waitq::IRQ_WAITQ.earliest_deadline());
            if !crate::kthread::reschedule_current(cpu, RescheduleAction::Park) {
                match self.sched.yield_current(sched_task) {
                    Ok(()) | Err(SchedError::InvalidState) => {}
                    Err(SchedError::NoSuchTask) => break Err(Errno::NotFound),
                    Err(_) => break Err(Errno::OutOfRange),
                }
            }
        };

        crate::waitq::SERVE_WAITQ.deregister(sched_task);
        crate::waitq::IRQ_WAITQ.deregister(sched_task);
        // Re-point the one-shot at whatever deadline any *remaining* waiter
        // needs (or clear it) so a finished wait leaves no stale arming.
        self.arch
            .set_wakeup(crate::waitq::IRQ_WAITQ.earliest_deadline());

        let (kind, id, token) = outcome?;

        // Write the ready member's token through the validated boundary
        // *before* consuming an IRQ edge, so a faulting `token_out` (a buggy
        // caller) fails closed `BadAddress` without dropping a delivered
        // interrupt.
        let token_bytes = token.to_le_bytes();
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(token_out), &token_bytes)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }

        // Consume an IRQ winner's edge now (the same `swap` `irq_wait`
        // performs) so the next wait re-arms and parks rather than
        // re-reporting the same fire; an endpoint winner is left for
        // `call_recv` to drain (the scan only peeked it). A `u64::MAX`
        // deadline means `try_wait_step` never returns `TimedOut` here; a
        // binding torn down between the scan and now resolves to nothing and
        // consumes nothing (harmless).
        if kind == WaitSourceKind::Irq {
            let now = self.arch.monotonic_ns(cpu);
            let _ = self
                .irq
                .try_wait_step(IrqHandle::from_raw(id), caller.task_id, now, u64::MAX);
        }
        Ok(0)
    }

    fn fs_open(
        &self,
        caller: &CallerContext<'_>,
        path: u64,
        path_len: usize,
        flags: OpenFlags,
    ) -> SyscallResult {
        // The dispatcher already checked `CAP_FS_ACCESS` and that `path` is a
        // non-null `UserPtr`, and rejected any illegal `OpenFlags`. Copy the
        // path in, then resolve+authorise it through the secured VFS under
        // the caller's kernel-attested identity (uid + effective caps), which
        // performs the create/exclusive/truncate/directory semantics and
        // every per-inode and mount-flag check. Only on success is a
        // descriptor recorded, so a refused open never produces a handle.
        let path = self.copy_path_in(caller, path, path_len)?;
        let uid = caller.caps.owner().0;
        self.filesystem
            .open(uid, caller.caps.effective(), &path, flags)?;
        let fd = self
            .aspaces
            .write()
            .open_file(caller.task_id, path, flags)?;
        Ok(u64::from(fd))
    }

    fn fs_close(&self, caller: &CallerContext<'_>, fd: u32) -> SyscallResult {
        // Release the caller's own descriptor. `fd` is resolved against the
        // kernel-trusted caller id, so one task cannot close another's
        // handle; an unopened descriptor fails closed with `NotFound`.
        if self.aspaces.write().close_file(caller.task_id, fd) {
            Ok(0)
        } else {
            Err(Errno::NotFound)
        }
    }

    fn fs_read(
        &self,
        caller: &CallerContext<'_>,
        fd: u32,
        offset: u64,
        buf: u64,
        len: usize,
    ) -> SyscallResult {
        // Resolve the handle (owner-checked clone, no lock held across the
        // read). A handle not opened for reading fails closed before the
        // filesystem is touched.
        let handle = self
            .aspaces
            .read()
            .open_file_entry(caller.task_id, fd)
            .ok_or(Errno::NotFound)?;
        if !handle.flags.is_read() {
            return Err(Errno::PermissionDenied);
        }
        // Cap the per-call transfer at the staging bound (the `lib/rt`
        // wrapper splits a larger read into successive calls).
        let len = len.min(FS_IO_MAX);
        if len == 0 {
            return Ok(0);
        }
        let uid = caller.caps.owner().0;
        let mut data = vec![0u8; len];
        let n = self.filesystem.read(
            uid,
            caller.caps.effective(),
            &handle.path,
            offset,
            &mut data,
        )?;
        // `n <= len` by the service contract; copy exactly what was read out
        // through the validated boundary.
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(buf), &data[..n])
        }) {
            Some(Ok(())) => Ok(n as u64),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        }
    }

    fn fs_write(
        &self,
        caller: &CallerContext<'_>,
        fd: u32,
        offset: u64,
        buf: u64,
        len: usize,
    ) -> SyscallResult {
        let handle = self
            .aspaces
            .read()
            .open_file_entry(caller.task_id, fd)
            .ok_or(Errno::NotFound)?;
        if !handle.flags.is_write() {
            return Err(Errno::PermissionDenied);
        }
        let len = len.min(FS_IO_MAX);
        if len == 0 {
            return Ok(0);
        }
        // Stage the caller's bytes in through the validated boundary *before*
        // touching the filesystem (a faulting buffer writes nothing).
        let mut data = vec![0u8; len];
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_in(space, physmap, VirtAddr::new(buf), &mut data)
        }) {
            Some(Ok(())) => {}
            Some(Err(err)) => return Err(copy_fault_errno(err)),
            None => return Err(Errno::BadAddress),
        }
        let uid = caller.caps.owner().0;
        // An append handle writes at the current end of file, ignoring the
        // supplied offset (the journal-append posture).
        let append = handle.flags.contains(OpenFlags::APPEND);
        let n = self.filesystem.write(
            uid,
            caller.caps.effective(),
            &handle.path,
            offset,
            append,
            &data,
        )?;
        Ok(n as u64)
    }

    fn fs_readdir(
        &self,
        caller: &CallerContext<'_>,
        fd: u32,
        buf: u64,
        len: usize,
    ) -> SyscallResult {
        let handle = self
            .aspaces
            .read()
            .open_file_entry(caller.task_id, fd)
            .ok_or(Errno::NotFound)?;
        let uid = caller.caps.owner().0;
        // The secured VFS enforces that the path is a directory the caller
        // may list; a non-directory fails closed there.
        let entries = self
            .filesystem
            .readdir(uid, caller.caps.effective(), &handle.path)?;
        // Pack the listing into the `DirEntry` wire stream. A name the driver
        // reports that is empty or longer than `FS_NAME_MAX` is a structural
        // fault and fails the whole call closed (never a truncated record).
        let mut out = Vec::new();
        let mut rec = [0u8; DirEntry::HEADER_LEN + FS_NAME_MAX];
        for (kind, name) in &entries {
            let entry = DirEntry {
                kind: *kind,
                name: name.as_bytes(),
            };
            let written = entry.encode_into(&mut rec)?;
            out.extend_from_slice(&rec[..written]);
        }
        // The whole listing or nothing: never truncate to fit an undersized
        // buffer (the caller grows `buf` and retries).
        if out.len() > len {
            return Err(Errno::BufferTooSmall);
        }
        if out.is_empty() {
            return Ok(0);
        }
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(buf), &out)
        }) {
            Some(Ok(())) => Ok(out.len() as u64),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        }
    }

    fn fs_stat(
        &self,
        caller: &CallerContext<'_>,
        fd: u32,
        out: u64,
        out_len: usize,
    ) -> SyscallResult {
        let handle = self
            .aspaces
            .read()
            .open_file_entry(caller.task_id, fd)
            .ok_or(Errno::NotFound)?;
        let uid = caller.caps.owner().0;
        let stat = self
            .filesystem
            .stat(uid, caller.caps.effective(), &handle.path)?;
        // The whole record or nothing: an undersized buffer fails closed.
        if out_len < FileStat::WIRE_LEN {
            return Err(Errno::BufferTooSmall);
        }
        let mut buf = [0u8; FileStat::WIRE_LEN];
        stat.encode(&mut buf)?;
        match self.with_caller_aspace(caller, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(out), &buf)
        }) {
            Some(Ok(())) => Ok(FileStat::WIRE_LEN as u64),
            Some(Err(err)) => Err(copy_fault_errno(err)),
            None => Err(Errno::BadAddress),
        }
    }

    fn fs_truncate(&self, caller: &CallerContext<'_>, fd: u32, size: u64) -> SyscallResult {
        let handle = self
            .aspaces
            .read()
            .open_file_entry(caller.task_id, fd)
            .ok_or(Errno::NotFound)?;
        // Truncation is a write; a handle not opened for writing fails closed
        // before the filesystem is touched (the secured VFS also rejects a
        // read-only mount or a directory).
        if !handle.flags.is_write() {
            return Err(Errno::PermissionDenied);
        }
        let uid = caller.caps.owner().0;
        self.filesystem
            .truncate(uid, caller.caps.effective(), &handle.path, size)?;
        Ok(0)
    }

    fn fs_sync(&self, caller: &CallerContext<'_>, fd: u32) -> SyscallResult {
        // The descriptor must be one of the caller's own open handles (a
        // forged or foreign number fails closed); the flush itself is
        // filesystem-wide, so the path is not needed beyond proving the
        // caller holds a live handle on the mounted volume.
        self.aspaces
            .read()
            .open_file_entry(caller.task_id, fd)
            .ok_or(Errno::NotFound)?;
        let uid = caller.caps.owner().0;
        self.filesystem.sync(uid, caller.caps.effective())?;
        Ok(0)
    }

    fn fs_mkdir(&self, caller: &CallerContext<'_>, path: u64, path_len: usize) -> SyscallResult {
        // The dispatcher already checked `CAP_FS_ACCESS` and that `path` is a
        // non-null `UserPtr`. Resolution and the permission/mount-flag model
        // are the secured VFS's, under the caller's attested identity.
        let path = self.copy_path_in(caller, path, path_len)?;
        let uid = caller.caps.owner().0;
        self.filesystem.mkdir(uid, caller.caps.effective(), &path)?;
        Ok(0)
    }

    fn fs_unlink(&self, caller: &CallerContext<'_>, path: u64, path_len: usize) -> SyscallResult {
        let path = self.copy_path_in(caller, path, path_len)?;
        let uid = caller.caps.owner().0;
        self.filesystem
            .unlink(uid, caller.caps.effective(), &path)?;
        Ok(0)
    }

    fn fs_rename(
        &self,
        caller: &CallerContext<'_>,
        src: u64,
        src_len: usize,
        dst: u64,
        dst_len: usize,
    ) -> SyscallResult {
        // The dispatcher already checked `CAP_FS_ACCESS` and that both `src`
        // and `dst` are non-null `UserPtr`s. Both paths are copied in and
        // validated; resolution and the permission/mount-flag model are the
        // secured VFS's, under the caller's attested identity.
        let src = self.copy_path_in(caller, src, src_len)?;
        let dst = self.copy_path_in(caller, dst, dst_len)?;
        let uid = caller.caps.owner().0;
        self.filesystem
            .rename(uid, caller.caps.effective(), &src, &dst)?;
        Ok(0)
    }
}

/// The [`SpawnCtx`] the `spawn` syscall handler hands its architecture
/// spawn producer (`plans/SPAWN.md` SP3).
///
/// Built fresh per `spawn` call from the handler's borrows; it carries no
/// state of its own. [`SpawnCtx::admit_process`] registers the producer's
/// freshly built process with this kernel state's scheduler, capability
/// table, and address-space registry and returns its PID — the
/// runtime-spawn analogue of the PID-1 `KernelInitSpawner` (`init.rs`), but
/// it admits the task **Ready** and does **not** enter user mode or drain
/// the scheduler (the spawning caller keeps running).
///
/// Public so a kernel-side (host-driven) spawn — the drvhost driver-spawn
/// path and its proving QEMU vertical (`PLAN.md` Stage 4.HW) — can drive
/// the *same* production admit path (scheduler admit, capability-record
/// insert, address-space + standard-stream + resource-limit registration,
/// parent/child wait link) instead of duplicating it.
/// Construct it with [`KernelSpawnCtx::new`]; the fields stay private so
/// the admit invariants cannot be bypassed.
pub struct KernelSpawnCtx<'a, A>
where
    A: KernelArch + 'static,
{
    frames: &'a FrameAllocator,
    /// The same allocator as [`Self::frames`], but a `'static` borrow, so
    /// the producer can build the child's **page tables** out of
    /// reclaimable RAM that scales with the machine rather than a fixed
    /// `.bss` pool. [`None`] when the boot path wired
    /// no `'static` allocator, so the producer fails closed.
    page_table_frames: Option<&'static FrameAllocator>,
    audit: &'a (dyn Sink + Sync),
    sched: &'a Scheduler<A>,
    caps: &'a RwLock<CapTable>,
    aspaces: &'a RwLock<AddressSpaceRegistry>,
    arch: &'a A,
    /// The spawning caller's task id — the parent the freshly admitted
    /// child is recorded against so a later `wait` from this parent can
    /// reap it (`plans/SPAWN.md` SP6). Kernel-trusted, never caller-supplied.
    parent: SecTaskId,
    /// The scheduler-side process-wait producer the parent/child link is
    /// recorded with at admit. Defaults to the inert `NULL_PROCESS_WAIT`
    /// until the boot path installs the real producer, so the link is a
    /// no-op until `wait` is wired.
    process_wait: &'static (dyn ProcessWait + 'static),
    /// The standard-stream descriptor table the admitted child is
    /// established with (the spawner decides the
    /// child's stream backing). The spawn handler resolves it from the
    /// syscall's `console` argument — the caller's own table for
    /// `CONSOLE_INHERIT`, else `DescriptorTable::standard_on` a
    /// validated installed-console index (`plans/PI.md` P11).
    streams: DescriptorTable,
    /// The device-resource grants the freshly admitted child is minted —
    /// one unforgeable, owner-checked grant handle per [`HwResource`] the
    /// child's matched hardware-tree node requested, so a user-space
    /// driver can reach exactly those windows through `mmio_map` /
    /// `dma_alloc` and learn its handles through `resource_grants`
    /// (resources are capability-grant requests, never
    /// ambient handles; — only the resources the matched node
    /// requested).
    ///
    /// The resources originate **kernel-side** — from the kernel's own
    /// discovered hardware tree, threaded by the privileged, capability-
    /// gated driver-spawn path — never copied from an untrusted caller, so
    /// minting a grant can never hand a task authority over a window its
    /// matched node did not expose (no ambient authority).
    /// The ordinary `spawn` syscall carries an **empty** slice: a user task
    /// cannot grant device windows to a child (delegation never
    /// widens authority).
    grants: &'a [HwResource],
    /// The discovered hardware-tree node the spawned **driver** was matched
    /// and loaded for, recorded against the child so its `hw_emit_node` calls
    /// parent published children under it. [`None`] for
    /// an ordinary `spawn` and for any spawn that is not a node-matched
    /// driver load, so such a task has no loaded node and may publish no
    /// child (fail closed). Kernel-sourced (the
    /// matched node the device manager resolved), never caller-supplied.
    node_id: Option<u32>,
    /// The kernel-minted process-instance identity attached to the admitted
    /// child's capability record. Minted at the call site from the kernel's
    /// single CSPRNG reserve (an ordinary `spawn`) or the bootstrap counter
    /// (a boot-floor driver-spawn); never supplied or influenced by any
    /// caller, so it cannot be forged.
    proc_id: ProcId,
}

impl<'a, A> KernelSpawnCtx<'a, A>
where
    A: KernelArch + 'static,
{
    /// Bind a spawn context to the live kernel subsystems.
    ///
    /// `parent` is the kernel-trusted identity of the spawning caller — for a syscall-driven spawn the dispatcher's
    /// resolved task id, for a kernel-side (host-driven) spawn the
    /// supervising task the child is recorded against for a later `wait`.
    /// `page_table_frames` is the `'static` allocator the producer builds
    /// the child's page tables from; `None` fails the spawn closed. `streams` is the descriptor table the
    /// child is established with — the spawner's resolved console
    /// attachment. `grants` is the kernel-sourced set of
    /// device resources the child is minted a per-resource grant for — an
    /// **empty** slice for an ordinary `spawn` (a user task grants no
    /// device windows), the matched node's requested
    /// [`HwResource`]s for a privileged driver-spawn.
    #[must_use]
    // Mirrors `KernelDispatchHook::new`: the same distinct kernel-state
    // borrows threaded explicitly, not a one-use
    // wrapper type.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        frames: &'a FrameAllocator,
        page_table_frames: Option<&'static FrameAllocator>,
        audit: &'a (dyn Sink + Sync),
        sched: &'a Scheduler<A>,
        caps: &'a RwLock<CapTable>,
        aspaces: &'a RwLock<AddressSpaceRegistry>,
        arch: &'a A,
        parent: SecTaskId,
        process_wait: &'static (dyn ProcessWait + 'static),
        streams: DescriptorTable,
        grants: &'a [HwResource],
        node_id: Option<u32>,
        proc_id: ProcId,
    ) -> Self {
        Self {
            frames,
            page_table_frames,
            audit,
            sched,
            caps,
            aspaces,
            arch,
            parent,
            process_wait,
            streams,
            grants,
            node_id,
            proc_id,
        }
    }
}

impl<A> SpawnCtx for KernelSpawnCtx<'_, A>
where
    A: KernelArch + 'static,
{
    fn frames(&self) -> &FrameAllocator {
        self.frames
    }

    fn page_table_allocator(&self) -> Option<&'static FrameAllocator> {
        self.page_table_frames
    }

    fn audit(&self) -> &(dyn Sink + Sync) {
        self.audit
    }

    #[allow(clippy::too_many_arguments)]
    unsafe fn admit_process(
        &self,
        caps: CapabilitySet,
        space: Box<dyn UserAddressSpace + Send + Sync>,
        physmap: Box<dyn PhysMap + Send + Sync>,
        stack: Box<dyn crate::kthread::KernelStack + Send>,
        pre_resume: Box<dyn FnMut(u64) + Send>,
        live: Option<Box<dyn rustos_kernel_mem::LiveUserSpace + Send>>,
        mut enter: Box<dyn FnMut() + Send>,
    ) -> Result<u64, AdmitError> {
        let cpu = SchedulerArch::current_cpu(self.arch);

        // Admit the child as a resumable user kthread (`plans/SPAWN.md`
        // SP2): the work body performs the user-mode transition on the
        // task's own kernel stack, and the `pre_resume` hook reactivates
        // the child's address-space root before every switch into it so it
        // `eret`s into EL0 under the correct, isolated translation regime. `enter` diverges into EL0, so the `()` it
        // yields satisfies the body signature for the (impossible) case the
        // transition ever returned. The arch seam owns the kernel stack
        // (`stack`) — an arena-backed stack whose guard page it has unmapped
        // in the child's own root, else the software-canary `BoxStack`
        // fallback (`plans/PI.md` G3b-2).
        let work = move |_yielder: &mut crate::kthread::Yielder<A::Cs>| {
            enter();
        };
        let cs = self.arch.context_switch();
        // When the producer retained a live, mutable address space, admit the
        // child with it so its `mem_map` / `mmio_map` syscalls mutate its own
        // space through the per-CPU live-space slot (`plans/PI.md`
        // 5d-0-ii (b′)); otherwise admit the plain form and those syscalls
        // fail closed.
        let admitted = match live {
            Some(live) => crate::kthread::spawn_user_kthread_with_stack_live(
                self.sched,
                cs,
                stack,
                cpu,
                Priority::Normal,
                pre_resume,
                live,
                work,
            ),
            None => crate::kthread::spawn_user_kthread_with_stack(
                self.sched,
                cs,
                stack,
                cpu,
                Priority::Normal,
                pre_resume,
                work,
            ),
        };
        let task_id = admitted.map_err(|_| AdmitError::SchedulerFull)?;

        // Register the child's caps under the *same* numeric id the
        // dispatcher recovers (`SecTaskId(task_id)`), so its first syscall
        // resolves a caller context. `caps` is already
        // the manifest∩user-grant set the producer derived; pass it as both
        // bounds so the kernel re-derives the same effective set. uid 0 is
        // the system user; a per-user spawn uid is a
        // later stage.
        let sec_id = SecTaskId(task_id);
        // Attach the kernel-minted process-instance identity to the record so
        // every syscall this child makes is attributed to the exact instance
        // (the audit log distinguishes it from a later task that reuses the
        // numeric id). The id is kernel-minted, never caller-supplied.
        let record = TaskCapabilities::derive(sec_id, UserId(0), caps, caps, self.audit)
            .with_proc_id(self.proc_id);
        self.caps.write().insert(record);

        // Register the child's frozen address space + direct map under the
        // same id, so its first user-memory copy resolves its own mappings
        // instead of failing closed with `BadAddress` (`plans/PI.md` P6c-3
        // follow-up). A fresh task id is never already present; a refusal
        // signals a kernel invariant violation, so fail closed rather than admit a task whose user memory the
        // kernel cannot reach. The already-admitted scheduler task is reaped
        // when it is next dispatched and finds no caps/aspace — but that
        // path cannot occur for a fresh id, so the conflict is reported as
        // an invariant violation to the caller.
        if self
            .aspaces
            .write()
            .register(sec_id, space, physmap)
            .is_err()
        {
            return Err(AdmitError::AspaceConflict);
        }

        // Establish the child's standard streams: the
        // spawner-resolved table — the parent's own (inherit) or the
        // standard shape on an explicitly selected, validated console
        // (`plans/PI.md` P11). The program names only the fd numbers; it
        // never reaches an ambient device. A richer
        // inheritance policy (e.g. piping a child's `stdout`) is a later
        // stage.
        self.aspaces.write().set_streams(sec_id, self.streams);

        // Inherit the parent's effective resource limits:
        // the child's set is the parent's intersected against the system
        // default policy, so it can never hold a bound wider than either the
        // parent's ceiling or the default (the never-widen rule, mirroring
        // capability delegation). A parent with no established set
        // resolves to `LimitSet::DEFAULT`, so the child does too. Read and
        // write are separate lock acquisitions because a fresh child id is
        // never concurrently mutated by another path.
        let inherited = LimitSet::inherit(&self.aspaces.read().limits(self.parent));
        self.aspaces.write().set_limits(sec_id, inherited);

        // Mint the child's device-resource grants:
        // one unforgeable, owner-checked handle per [`HwResource`] the
        // matched hardware-tree node requested, keyed to the child's own
        // kernel-trusted id, so the child reaches exactly those windows
        // through `mmio_map` / `dma_alloc` and enumerates its handles
        // through `resource_grants` — and another task presenting the same
        // numeric handle resolves nothing (the registry owner-check).
        // The resources are kernel-sourced (the privileged driver-spawn path
        // threads the matched node's requests, never an untrusted caller),
        // so minting can never widen authority beyond what that node exposed
        // (no ambient authority); the ordinary `spawn` syscall carries
        // an empty slice and mints nothing. Minted only after the child is
        // fully admitted, under one write lock, so a `resource_grants` from
        // the child observes the complete set.
        if !self.grants.is_empty() || self.node_id.is_some() {
            let mut aspaces = self.aspaces.write();
            for resource in self.grants {
                aspaces.mint_grant(sec_id, *resource);
            }
            // Record the matched node the driver was loaded for, beside its
            // grants and under the same write lock, so a later `hw_emit_node`
            // from this driver parents its published child under exactly this
            // node (the emitter cannot forge its
            // tree position). `None` (an ordinary `spawn`) records nothing.
            if let Some(node_id) = self.node_id {
                aspaces.set_loaded_node(sec_id, node_id);
            }
        }

        // Record the parent/child link with the process-wait producer so the
        // spawning parent can later `wait` on this child and reap its exit
        // code (`plans/SPAWN.md` SP6). Done only after the child is fully
        // admitted (scheduler + caps + aspace + streams) so a parent that
        // observes the returned PID can immediately and soundly reap it.
        self.process_wait.register_child(self.parent, sec_id);

        Ok(task_id)
    }
}

/// [`IrqWaiter`] adapter wiring the `irq_wait` syscall handler's
/// scheduler + architecture borrows into the shared
/// [`block_until_ready`] loop.
///
/// Holds only borrows and a captured CPU id; constructed fresh per
/// `irq_wait` call on the issuing CPU's process context.
struct SyscallIrqWaiter<'a, A>
where
    A: KernelArch + 'static,
{
    sched: &'a Scheduler<A>,
    arch: &'a A,
    cpu: CpuId,
    task: SecTaskId,
    /// The controller the bound line is re-armed through before each park.
    irq_controller: &'a (dyn IrqController + Sync),
    /// The line bound to this wait's handle (owner-checked at entry), or
    /// [`None`] if the handle was forged/foreign — in which case nothing is
    /// re-armed and `try_wait_step` fails the wait closed.
    line: Option<u32>,
}

impl<A> IrqWaiter for SyscallIrqWaiter<'_, A>
where
    A: KernelArch + 'static,
{
    fn now_ns(&self) -> u64 {
        self.arch.monotonic_ns(self.cpu)
    }

    fn yield_now(&self) -> Result<(), IrqWaitAbort> {
        // Re-arm the bound line on the driver's behalf before parking: a
        // user-space interrupt-driven driver holds no controller access, so
        // the kernel routes its line to the waiting CPU and clears the mask
        // `IrqTable::fire` set on the previous completion (mask-before-wake,
        // `docs/src/security/irq.md`). On the first park this is the initial
        // route+enable; on later parks it re-enables after a drained
        // completion. Idempotent and best-effort — a refusal (an impossible
        // out-of-range line for a bound handle, or a placeholder controller)
        // leaves the line as-is and the wait is bounded by its deadline. A forged/foreign handle resolved to `None` and
        // re-arms nothing — `try_wait_step` already fails it closed.
        if let Some(line) = self.line {
            let _ = self.irq_controller.rearm(line);
        }
        // Arm the timed-wake one-shot to the nearest pending `irq_wait`
        // deadline so a finite timeout fires even on an otherwise-idle CPU
        // (the nearest armed wakeup), then *park* off
        // the run queue until woken by `irq_wake` (a fire) or the timed
        // sweep. The caller registered this task on `IRQ_WAITQ` before the
        // first poll, so the park/unpark race is closed by the scheduler's
        // wake-pending token (no busy yield). This
        // mirrors `hw_tree_wait`'s park exactly.
        self.arch
            .set_wakeup(crate::waitq::IRQ_WAITQ.earliest_deadline());
        // `reschedule_current` returns `false` only when the caller is not
        // a resumable user kthread (host tests with no live dispatch loop);
        // fall back to a cooperative yield then so a degenerate caller
        // never busy-spins and the loop still terminates on the monotone
        // clock reaching its deadline.
        if !crate::kthread::reschedule_current(self.cpu, RescheduleAction::Park) {
            match self.sched.yield_current(self.task.0) {
                Ok(()) | Err(SchedError::InvalidState) => {}
                Err(SchedError::NoSuchTask) => return Err(IrqWaitAbort::TaskVanished),
                Err(_) => return Err(IrqWaitAbort::SchedulerError),
            }
        }
        Ok(())
    }
}

/// Production [`DispatchHook`] wiring `KernelSyscallHandlers` to the
/// bin-crate dispatch callback.
///
/// Owns the same borrows as [`KernelSyscallHandlers`] plus a
/// [`Dispatcher`] cell built on top of them. The bin-crate
/// `extern "C"` syscall-dispatch callback ((f5)) calls
/// [`Self::dispatch`] once per syscall; this method runs the
/// sequence (identify caller → forward to [`Dispatcher::dispatch`] →
/// translate result) and returns a [`DispatchOutcome`] the bin crate
/// can encode back into the architecture's syscall-return register
/// or, on caller-identification failure, fail-close by halting the
/// CPU.
///
/// # Caller identification
///
/// The hook reads the per-CPU current-task slot from
/// `Scheduler::current_task` ((f1)) and looks up the per-task
/// capability record through the `CapTable` ((f2)). Both lookups are
/// fallible:
///
/// * `current_task` returns `None` when no task is currently running
///   on the issuing CPU. That cannot happen once the scheduler is
///   live, but the trampoline must not assume so.
/// * `caps_for` returns `None` when the running task has no
///   capability record — also impossible during normal operation
///   (`KernelState` populates the record before scheduling any task),
///   but treated as a security failure on the same grounds.
///
/// Either failure emits one [`AuditEvent::SyscallNoCallerContext`]
/// record (carrying a stable `cause` field naming which lookup
/// failed) and returns [`DispatchOutcome::NoCallerContext`]; the
/// bin-crate callback halts the CPU forever in response.
pub struct KernelDispatchHook<'a, A>
where
    A: KernelArch + 'static,
{
    handlers: KernelSyscallHandlers<'a, A>,
    sched: &'a Scheduler<A>,
    caps: &'a RwLock<CapTable>,
    arch: &'a A,
    audit: &'a (dyn Sink + Sync),
}

impl<'a, A> KernelDispatchHook<'a, A>
where
    A: KernelArch + 'static,
{
    /// Build a new dispatch hook bound to the supplied kernel state.
    ///
    /// All borrows must outlive the slot the hook is published into.
    /// `KernelState` (constructed by [`crate::kernel_main`]) holds the
    /// targets for the lifetime of the running kernel; the hook is
    /// `Box::leak`'d alongside it so the published `'static dyn
    /// DispatchHook` is sound (no global mutable
    /// static; the leak is a one-shot, immutable publish).
    #[must_use]
    // Mirrors `KernelSyscallHandlers::new`: the same distinct kernel-
    // state borrows threaded explicitly, not a
    // one-use wrapper type.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        sched: &'a Scheduler<A>,
        caps: &'a RwLock<CapTable>,
        arch: &'a A,
        audit: &'a (dyn Sink + Sync),
        irq: &'a IrqTable,
        irq_controller: &'a (dyn IrqController + Sync),
        ipc: &'a RwLock<PortRegistry>,
        aspaces: &'a RwLock<AddressSpaceRegistry>,
        rng: &'a RwLock<Box<dyn RandomReserve + Send + Sync>>,
        consoles: &'static [ConsoleDevice],
        frames: &'a FrameAllocator,
        page_table_frames: &'static FrameAllocator,
        programs: &'static ProgramRegistry,
        spawn_service: &'static (dyn ProcessSpawn + 'static),
        process_wait: &'static (dyn ProcessWait + 'static),
        input_focus: &'static InputFocus,
        mem_map: &'static (dyn MemMap + 'static),
        mmio_map_facility: &'static (dyn MmioMapFacility + 'static),
        dma_alloc_facility: &'static (dyn DmaAllocFacility + 'static),
    ) -> Self {
        Self {
            handlers: KernelSyscallHandlers::new(
                sched,
                caps,
                arch,
                audit,
                irq,
                irq_controller,
                ipc,
                aspaces,
                rng,
            )
            .with_consoles(consoles)
            .with_frames(frames)
            .with_page_table_frames(page_table_frames)
            .with_spawn(programs, spawn_service)
            .with_process_wait(process_wait)
            .with_input_focus(input_focus)
            .with_mem_map(mem_map)
            .with_mmio_map_facility(mmio_map_facility)
            .with_dma_alloc_facility(dma_alloc_facility),
            sched,
            caps,
            arch,
            audit,
        }
    }

    /// Install the users-database holder the `users_db_read` syscall
    /// serves, consuming and returning `self` (`plans/PI.md` P11).
    ///
    /// The hook-level mirror of
    /// [`KernelSyscallHandlers::with_users_db`]: called once by a boot
    /// path that mounted the root volume and ran the audited
    /// [`crate::load_users_db`] read. A boot path with no root volume
    /// simply never calls it and `users_db_read` stays fail-closed.
    #[must_use]
    pub fn with_users_db(mut self, users_db: &'static (dyn UsersDbSource + 'static)) -> Self {
        self.handlers = self.handlers.with_users_db(users_db);
        self
    }

    /// Install the discovered hardware-tree store the `hw_tree_read` /
    /// `hw_tree_wait` syscalls serve, consuming and returning `self`.
    ///
    /// The hook-level mirror of [`KernelSyscallHandlers::with_hw_tree`]:
    /// called once by the boot path after it seeds the discovered
    /// inventory. A boot path that seeds no tree simply never calls it and
    /// both syscalls stay fail-closed.
    #[must_use]
    pub fn with_hw_tree(mut self, hw_tree: &'static (dyn HwTreeSource + 'static)) -> Self {
        self.handlers = self.handlers.with_hw_tree(hw_tree);
        self
    }

    /// Install the disk-backed filesystem service the `fs_*` syscalls route
    /// through, consuming and returning `self` (`PREREQUISITES.md` P-A).
    ///
    /// The hook-level mirror of [`KernelSyscallHandlers::with_filesystem`]:
    /// called once by the boot path that owns a mounted volume. A boot path
    /// that mounts no volume simply never calls it and every `fs_*` syscall
    /// stays fail-closed through [`NULL_FILESYSTEM`].
    #[must_use]
    pub fn with_filesystem(
        mut self,
        filesystem: &'static (dyn FilesystemService + 'static),
    ) -> Self {
        self.handlers = self.handlers.with_filesystem(filesystem);
        self
    }

    /// Install the kernel wall clock the `wall_time_get` / `wall_time_set`
    /// syscalls read and drive, consuming and returning `self`
    /// (`PREREQUISITES.md` P-D).
    ///
    /// The hook-level mirror of [`KernelSyscallHandlers::with_wall_clock`]:
    /// the boot path passes the leaked production
    /// [`crate::wallclock::KernelWallClock`]. A boot path that never calls it
    /// leaves the fail-closed [`crate::wallclock::NULL_WALL_CLOCK`] default
    /// (`wall_time_get` reports `Unset`, `wall_time_set` returns
    /// `NotImplemented`).
    #[must_use]
    pub fn with_wall_clock(mut self, wall_clock: &'static (dyn WallClockSource + 'static)) -> Self {
        self.handlers = self.handlers.with_wall_clock(wall_clock);
        self
    }

    /// Install the per-boot identifier the `boot_id_get` syscall reports,
    /// consuming and returning `self` (`PREREQUISITES.md` P-E).
    ///
    /// The hook-level mirror of [`KernelSyscallHandlers::with_boot_id`]: the
    /// boot path passes the [`BootId`] it minted from the seeded CSPRNG
    /// reserve. A boot path that never calls it — or a port whose reserve
    /// could not be seeded, where the mint is [`BootId::UNSET`] — leaves
    /// `boot_id_get` failing closed with [`Errno::EntropyNotReady`].
    #[must_use]
    pub fn with_boot_id(mut self, boot_id: BootId) -> Self {
        self.handlers = self.handlers.with_boot_id(boot_id);
        self
    }

    /// Install the kernel's diagnostic log sink the `log_emit` syscall emits
    /// user-space records through, consuming and returning `self`.
    ///
    /// The hook-level mirror of [`KernelSyscallHandlers::with_log_sink`]:
    /// called once by the boot path with the same arch diagnostic sink the
    /// kernel routes its own records through. A boot path that installs no
    /// sink leaves the no-op default and a `log_emit` is silently dropped.
    #[must_use]
    pub fn with_log_sink(mut self, log_sink: &'a (dyn Sink + Sync)) -> Self {
        self.handlers = self.handlers.with_log_sink(log_sink);
        self
    }

    /// Install the architecture MSI-alloc producer the `msi_alloc` syscall
    /// drives, consuming and returning `self` (`plans/PI.md` U-MSI).
    ///
    /// The hook-level mirror of
    /// [`KernelSyscallHandlers::with_msi_alloc_facility`]: the boot path
    /// passes the architecture port's [`KernelArch::msi_alloc_facility`]
    /// directly. [`None`] (a port with no MSI controller) is a no-op that
    /// leaves `msi_alloc` fail-closed with [`Errno::NotImplemented`], so the
    /// caller need not branch on it.
    #[must_use]
    pub fn with_msi_alloc_facility(
        mut self,
        msi_alloc_facility: Option<&'static (dyn MsiAllocFacility + 'static)>,
    ) -> Self {
        if let Some(facility) = msi_alloc_facility {
            self.handlers = self.handlers.with_msi_alloc_facility(facility);
        }
        self
    }

    /// Install the shared-memory producer the `shm_create` / `shm_map` /
    /// `shm_unmap` syscalls drive, consuming and returning `self`
    /// (`plans/USB.md`).
    ///
    /// The hook-level mirror of
    /// [`KernelSyscallHandlers::with_shared_mem_facility`]: the boot path
    /// passes the `kernel/mem`-backed producer (built over the arch direct
    /// physical map). It also publishes the producer to the shared-region
    /// registry so the exit / driver-unload reclaim paths can free a region's
    /// frames.
    #[must_use]
    pub fn with_shared_mem_facility(
        mut self,
        shared_mem_facility: &'static (dyn SharedMemFacility + 'static),
    ) -> Self {
        self.handlers = self.handlers.with_shared_mem_facility(shared_mem_facility);
        self
    }

    /// Borrow the [`KernelSyscallHandlers`] this hook owns.
    ///
    /// Used by the arch-port trap path to reach the [`IrqTable`] +
    /// [`IrqController`] pair through
    /// [`KernelSyscallHandlers::irq_table`] /
    /// [`KernelSyscallHandlers::irq_controller`] without re-borrowing
    /// `KernelState`.
    #[must_use]
    pub fn handlers(&self) -> &KernelSyscallHandlers<'a, A> {
        &self.handlers
    }

    /// Emit one [`AuditEvent::SyscallNoCallerContext`] record.
    fn audit_no_caller_context(&self, cpu: u32, cause: &'static str) {
        let mut cpu_buf = [0u8; 16];
        let ev = AuditEvent::SyscallNoCallerContext;
        rustos_log::log(
            self.audit,
            &rustos_log::Event {
                level: rustos_log::Level::Error,
                id: ev.id(),
                message: ev.message(),
                fields: &[
                    Field {
                        key: "cpu",
                        value: rustos_log::FieldValue::Str(format_hex_u64(
                            u64::from(cpu),
                            &mut cpu_buf,
                        )),
                    },
                    Field {
                        key: "cause",
                        value: rustos_log::FieldValue::Str(cause),
                    },
                ],
            },
        );
    }
}

impl<A> DispatchHook for KernelDispatchHook<'_, A>
where
    A: KernelArch + 'static,
{
    fn dispatch(&self, raw_number: u16, args: RawArgs) -> DispatchOutcome {
        // Step 1 — identify the caller. The
        // scheduler's per-CPU current-task slot is the only sanctioned
        // source; no caller-supplied identity is accepted.
        let cpu = SchedulerArch::current_cpu(self.arch);
        let Some(sched_task_id) = self.sched.current_task(cpu) else {
            self.audit_no_caller_context(cpu, "no_current_task");
            return DispatchOutcome::NoCallerContext;
        };

        // Snapshot the caller's capability record under a *briefly* held
        // read lock, then drop the guard before dispatching. The dispatcher
        // checks the required capability against this consistent
        // point-in-time snapshot (check before any
        // state touch), so there is no TOCTOU between the snapshot and the
        // check. Holding the read lock across the whole call instead would
        // self-deadlock the caps-mutating handlers — `exit`, `cap_delegate`,
        // `cap_revoke` all take `self.caps.write()`, and the
        // writer-preference `RwLock` cannot grant a writer while this thread
        // still holds a reader. Revocation by another CPU mid-call therefore
        // takes effect from the caller's *next* syscall, the same
        // credential-snapshot semantics a POSIX kernel gives a syscall in
        // flight; the mutating handlers operate on the live table under
        // their own write lock, so the revocation itself is not lost.
        let task_id = SecTaskId(sched_task_id);
        let caps_snapshot = {
            let guard = self.caps.read();
            if let Some(record) = guard.caps_for(task_id) {
                record.clone()
            } else {
                drop(guard);
                self.audit_no_caller_context(cpu, "no_capability_record");
                return DispatchOutcome::NoCallerContext;
            }
        };

        let caller = CallerContext {
            task_id,
            caps: &caps_snapshot,
        };

        // Steps 2–5: hand off to the dispatcher, which performs the
        // capability check, argument validation, handler dispatch,
        // and audit emission.
        let dispatcher = Dispatcher::new(&self.handlers, self.audit);
        let result = dispatcher.dispatch(&caller, raw_number, args);

        // SP2b producer (`plans/SPAWN.md` SP2): a rescheduling syscall from
        // a resumable user kthread must suspend its caller back to the
        // scheduler rather than `eret` straight back into EL0 — the kthread
        // `TaskAction` the scheduler observes when the task switches back is
        // authoritative for the re-enqueue/reap, so the `yield_now`/`exit`
        // handlers no longer drive the scheduler directly (reconciling the
        // double-handling the SP2a note flagged). The bin-crate callback
        // turns this into a `reschedule_current` call; if no user kthread is
        // published on this CPU it falls back to an ordinary encoded return
        // (fail closed — `crate::dispatch_slot::DispatchOutcome::Reschedule`).
        if let Some(action) = reschedule_action_for(raw_number) {
            return DispatchOutcome::Reschedule {
                result,
                action,
                cpu,
            };
        }
        DispatchOutcome::Returned(result)
    }
}

/// Map a rescheduling syscall number to the [`RescheduleAction`] its
/// caller must be suspended with, or `None` for an ordinary syscall that
/// returns straight to user space (`plans/SPAWN.md` SP2).
///
/// `yield` re-enqueues the caller; `exit` reaps it. Every other syscall
/// (`stream_write`, `ipc_*`, `cap_*`, `clock_get`, `irq_*`, `random_get`)
/// returns to the same EL0 task without a context switch, so it is `None`.
/// This is the single place the dispatch path names the rescheduling
/// syscalls.
fn reschedule_action_for(raw_number: u16) -> Option<RescheduleAction> {
    if raw_number == SyscallNumber::YIELD.as_u16() {
        Some(RescheduleAction::Yield)
    } else if raw_number == SyscallNumber::EXIT.as_u16() {
        Some(RescheduleAction::Exit)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sched::SchedulerConfig;
    use crate::test_arch::TestArch;
    use crate::test_sink::TestSink;
    use alloc::boxed::Box;
    use alloc::sync::Arc;
    use rustos_abi::input::{KeyValue, Modifiers};
    use rustos_abi::{CapabilityId, DescriptorTable, Errno, STDIN, STDOUT};
    use rustos_caps::CapabilitySet;
    use rustos_kernel_ipc::{CallEndpoint, CallEndpointLimits, Port, RecvCall};
    use rustos_kernel_irq::{IrqTable, UnsupportedController};
    use rustos_kernel_mem::{
        AddressSpace, AnonError, BootMemoryMap, DmaError, DmaMapping, Frame, FrameAllocator,
        FrozenAddressSpace, HostPageTable, LiveSpaceError, LiveUserSpace, MapFlags, MemoryRegion,
        Page, PhysAddr, RegionKind, SimPhysMap, VirtAddr, PAGE_SIZE,
    };
    use rustos_kernel_sched_api::{Priority, TaskAction};
    use rustos_kernel_sec::{TaskCapabilities, UserId};

    // `ProcessSpawn`, `ProgramRegistry`, `SpawnCtx`, and `AdmitError` are
    // already in scope through `use super::*`; only `EmbeddedProgram` is
    // additionally needed here.
    use crate::spawn::EmbeddedProgram;
    use rustos_log::{set_max_level, Level};
    use rustos_rng::{EntropyError, EntropySource, OutputReserve};

    use crate::random::BootReserve;

    fn install_trace_filter() {
        // The global log filter defaults to `Error`; `SyscallFeatureUnavailable`
        // is emitted at `Error`, but `set_max_level(Trace)` keeps the
        // tests robust against a future raise of `SyscallFeatureUnavailable`'s
        // severity or against other dispatcher events flowing through
        // the same sink (no flaky tests).
        set_max_level(Level::Trace);
    }

    fn make_sink() -> &'static TestSink {
        Box::leak(Box::new(TestSink::new()))
    }

    fn caps_with(items: &[CapabilityId]) -> CapabilitySet {
        let mut s = CapabilitySet::empty();
        for c in items {
            s.insert(*c);
        }
        s
    }

    fn make_caps_record(
        task: u64,
        items: &[CapabilityId],
        sink: &(dyn Sink + Sync),
    ) -> TaskCapabilities {
        let set = caps_with(items);
        TaskCapabilities::derive(SecTaskId(task), UserId(1000), set, set, sink)
    }

    fn make_sched(arch: Arc<TestArch>) -> Scheduler<TestArch> {
        let cfg = SchedulerConfig::defaults_for(1);
        Scheduler::new(cfg, arch).expect("scheduler builds")
    }

    /// Deterministic stand-in entropy source for the `random_get`
    /// copy-out tests (not real entropy): a counter expanded so a seeded
    /// reserve's output is reproducible and non-zero.
    struct TestEntropy(u64);

    impl EntropySource for TestEntropy {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), EntropyError> {
            for byte in out.iter_mut() {
                self.0 = self
                    .0
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                *byte = self.0.to_le_bytes()[4];
            }
            Ok(())
        }
    }

    /// The **unseeded** boot reserve every generic handler test composes:
    /// those tests never call `random_get`, so the reserve's state is
    /// irrelevant — what matters is that the handler receives its ninth
    /// borrow exactly as production does (`KernelState`).
    fn unseeded_rng() -> RwLock<Box<dyn RandomReserve + Send + Sync>> {
        RwLock::new(Box::new(BootReserve::new()) as Box<dyn RandomReserve + Send + Sync>)
    }

    /// A reserve **seeded** from the deterministic [`TestEntropy`] source,
    /// ready to serve the `random_get` copy-out tests with reproducible
    /// non-zero bytes.
    fn seeded_rng() -> RwLock<Box<dyn RandomReserve + Send + Sync>> {
        let mut reserve = OutputReserve::<TestEntropy>::new();
        reserve
            .seed(TestEntropy(0x00C0_FFEE))
            .expect("the deterministic source seeds");
        RwLock::new(Box::new(reserve) as Box<dyn RandomReserve + Send + Sync>)
    }

    /// Bind an unrestricted port at `endpoint` into `registry`.
    ///
    /// The port accepts any sender (empty `required_send_caps`, so no
    /// `IPC_BIND_PRIVILEGED` is needed) and any receiver, which is all
    /// the `ipc_send` / `ipc_recv` *endpoint-resolution* path under
    /// test cares about; the per-send capability check lives on
    /// `Port::send` and is exercised by `kernel/ipc`'s own tests.
    fn register_port(registry: &RwLock<PortRegistry>, endpoint: u64, sink: &(dyn Sink + Sync)) {
        let creator = make_caps_record(0xB1, &[], sink);
        let port = Port::create(
            EndpointId(endpoint),
            &creator,
            CapabilitySet::empty(),
            CapabilitySet::empty(),
            64,
            4,
            sink,
        )
        .expect("unrestricted port creation succeeds");
        // `register`'s error half is `(Box<Port>, Errno)`, which is not
        // `Debug`, so assert on the `Ok` discriminant rather than
        // `.expect()`ing the value.
        assert!(
            registry.write().register(port, sink).is_ok(),
            "first registration of a fresh endpoint succeeds"
        );
    }

    /// `SP2b` (`plans/SPAWN.md` SP2): the `yield_now` handler is inert
    /// toward the scheduler — the reschedule path (driven from the
    /// dispatch hook by the `yield` syscall number) re-enqueues the
    /// caller from the kthread `TaskAction`, so the handler always reports
    /// success and never touches `Scheduler::yield_current`. Driving the
    /// scheduler here would double-handle (and re-entrantly mutate) the
    /// in-flight `step` the kthread runs under.
    #[test]
    fn yield_now_is_inert_and_reports_success() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(0xDEAD, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(0xDEAD),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        // Even for a task the scheduler never admitted, the handler is a
        // no-op success: the reschedule path is authoritative.
        assert_eq!(h.yield_now(&ctx), Ok(0));
    }

    /// `SP2b` (`plans/SPAWN.md` SP2): the `exit` handler keeps only the
    /// security-state cleanup (evicting the capability record; releasing
    /// IRQ bindings) and reports success — the scheduler reap is driven by
    /// the reschedule path, not from this handler. It evicts the caps
    /// record even when the scheduler never admitted the task.
    #[test]
    fn exit_clears_caps_record_and_reports_success() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;

        // Register a record so we can confirm `exit` evicts it even
        // though the scheduler half fails.
        let record = make_caps_record(7, &[CapabilityId::FS_MOUNT], sink);
        table.write().insert(record);
        assert_eq!(table.read().len(), 1);

        let caps = make_caps_record(7, &[CapabilityId::FS_MOUNT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let r = h.exit(&ctx, 0);
        // The handler no longer drives the scheduler, so it reports
        // success; the reschedule path reaps the task.
        assert_eq!(r, Ok(0));
        // The capability record was evicted as part of the security
        // cleanup the reschedule path does not perform.
        assert!(table.read().is_empty());
    }

    /// `SP2b` (`plans/SPAWN.md` SP2): the producer maps exactly the two
    /// rescheduling syscalls (`yield`, `exit`) onto a `RescheduleAction`
    /// and leaves every other syscall as an ordinary return. This is the
    /// single decision point the dispatch hook consults before turning a
    /// completed syscall into `DispatchOutcome::Reschedule`.
    #[test]
    fn reschedule_action_for_maps_only_yield_and_exit() {
        assert_eq!(
            reschedule_action_for(SyscallNumber::YIELD.as_u16()),
            Some(RescheduleAction::Yield)
        );
        assert_eq!(
            reschedule_action_for(SyscallNumber::EXIT.as_u16()),
            Some(RescheduleAction::Exit)
        );
        // Every non-rescheduling syscall returns straight to EL0.
        for n in [
            SyscallNumber::IPC_SEND,
            SyscallNumber::IPC_RECV,
            SyscallNumber::CAP_QUERY,
            SyscallNumber::CAP_DELEGATE,
            SyscallNumber::CAP_REVOKE,
            SyscallNumber::CLOCK_GET,
            SyscallNumber::IRQ_BIND,
            SyscallNumber::IRQ_WAIT,
            SyscallNumber::RANDOM_GET,
            SyscallNumber::STREAM_WRITE,
        ] {
            assert_eq!(
                reschedule_action_for(n.as_u16()),
                None,
                "{n:?} must return to user space without a reschedule"
            );
        }
    }

    /// `cap_query` returns 1 for a held capability and 0 otherwise.
    #[test]
    fn cap_query_matches_caller_caps() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(1, &[CapabilityId::FS_MOUNT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(1),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.cap_query(&ctx, CapabilityId::FS_MOUNT), Ok(1));
        assert_eq!(h.cap_query(&ctx, CapabilityId::DRV_KERNEL), Ok(0));
    }

    /// `ipc_send` to an endpoint that is not bound in the registry
    /// fails closed with `NotFound` — a real lookup miss. The deferral
    /// audit is *not* emitted: the call never reached the copy-in
    /// branch, so there is no `SyscallFeatureUnavailable` record.
    #[test]
    fn ipc_send_to_unbound_endpoint_is_not_found_without_deferral_audit() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        sink.clear();
        assert_eq!(h.ipc_send(&ctx, 1, 0x1000, 4), Err(Errno::NotFound));
        // The endpoint never resolved, so the handler did not announce
        // the copy-in deferral.
        assert!(sink.event_ids().is_empty());
    }

    /// Frame backing the user payload in the `ipc_send` copy-in tests,
    /// chosen so its physical base falls inside a one-page
    /// [`SimPhysMap`] window the test can seed.
    const SEND_FRAME: usize = 16;

    /// Build a caller address space mapping user page 1 (`0x1000`) to
    /// [`SEND_FRAME`] with `flags`, plus a single-page physical map
    /// covering that frame seeded with `payload` at offset 0. Returns
    /// the boxed pair the registry stores.
    fn send_aspace(
        flags: MapFlags,
        payload: &[u8],
    ) -> (
        Box<dyn UserAddressSpace + Send + Sync>,
        Box<dyn PhysMap + Send + Sync>,
    ) {
        let base = PhysAddr::new(SEND_FRAME as u64 * PAGE_SIZE as u64);
        let sim = SimPhysMap::new(base, PAGE_SIZE);
        if !payload.is_empty() {
            let ptr = sim.translate(base, payload.len()).expect("seed in window");
            // SAFETY: the window owns these bytes for the simulator's
            // lifetime and nothing else aliases them during the test.
            unsafe {
                core::ptr::copy_nonoverlapping(payload.as_ptr(), ptr.as_ptr(), payload.len());
            }
        }
        let mut space = AddressSpace::new(HostPageTable::new());
        space
            .map(page(1), Frame(SEND_FRAME), flags)
            .expect("mapped");
        (Box::new(space), Box::new(sim))
    }

    /// `ipc_send` to a *bound* endpoint copies the payload in from the
    /// caller's address space and delivers it to the port — the
    /// increment-D.1 wiring of the user-memory copy-in path. The
    /// receiver observes the exact bytes and the sender's task id, and
    /// no `SyscallFeatureUnavailable` deferral is announced.
    #[test]
    fn ipc_send_to_bound_endpoint_copies_payload_and_delivers() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let payload = [0xDEu8, 0xAD, 0xBE, 0xEF];
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &payload);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        register_port(&ipc, 1, sink);
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        sink.clear();
        assert_eq!(h.ipc_send(&ctx, 1, 0x1000, payload.len()), Ok(0));
        // The deferral audit is gone now that the copy-in path is live.
        assert!(!sink
            .event_ids()
            .contains(&AuditEvent::SyscallFeatureUnavailable.id().0));
        // The receiver drains exactly the bytes the caller sent.
        let guard = ipc.read();
        let port = guard.lookup(EndpointId(1)).expect("port stays bound");
        let msg = port.recv().expect("a message was delivered");
        assert_eq!(msg.sender, 2);
        assert_eq!(msg.payload.as_slice(), &payload);
    }

    /// `log_emit` copies the encoded record in, decodes it, and emits it to
    /// the kernel's **diagnostic** `log_sink` — attributed to the calling
    /// task with a kernel-supplied `task` field the caller cannot forge. The caller's own fields follow.
    #[test]
    fn log_emit_emits_the_decoded_record_with_task_attribution() {
        install_trace_filter();
        let audit = make_sink();
        let log_sink = TestSink::new();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());

        let mut record = [0u8; LOG_RECORD_MAX];
        let len = rustos_abi::encode_log_record(
            &mut record,
            Level::Info.as_u8(),
            7030,
            "bundle accepted",
            &[("driver", rustos_abi::FieldValue::Str("vcmailbox"))],
        )
        .expect("encodes within bounds");

        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &record[..len]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::LOG_EMIT], audit);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, audit, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_log_sink(&log_sink);
        assert_eq!(h.log_emit(&ctx, 0x1000, len), Ok(0));

        let events = log_sink.snapshot();
        assert_eq!(events.len(), 1, "exactly one record reached the log sink");
        let event = &events[0];
        assert_eq!(event.level, Level::Info);
        assert_eq!(event.id, EventId(7030));
        assert_eq!(event.message, "bundle accepted");
        // The kernel-supplied attribution comes first and names the caller.
        assert_eq!(event.fields[0].0, "task");
        // The caller's own field is preserved after the attribution.
        assert!(event
            .fields
            .iter()
            .any(|(k, v)| k == "driver" && v == "vcmailbox"));
    }

    /// `log_emit` rejects a `len` larger than any valid encoded record
    /// before copying — a hostile length cannot drive a large kernel
    /// allocation — and emits nothing.
    #[test]
    fn log_emit_rejects_an_oversize_length_fail_closed() {
        install_trace_filter();
        let audit = make_sink();
        let log_sink = TestSink::new();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::LOG_EMIT], audit);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, audit, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_log_sink(&log_sink);
        assert_eq!(
            h.log_emit(&ctx, 0x1000, LOG_RECORD_MAX + 1),
            Err(Errno::LengthOutOfRange)
        );
        assert!(log_sink.snapshot().is_empty(), "no record was emitted");
    }

    /// `log_emit` copies in a record whose level byte is out of range and
    /// fails closed at decode, emitting nothing.
    #[test]
    fn log_emit_rejects_a_malformed_record_fail_closed() {
        install_trace_filter();
        let audit = make_sink();
        let log_sink = TestSink::new();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());

        // A well-formed header byte length but an invalid level byte (99).
        let mut record = [0u8; 8];
        record[0] = 99;
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &record);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::LOG_EMIT], audit);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, audit, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_log_sink(&log_sink);
        assert_eq!(
            h.log_emit(&ctx, 0x1000, record.len()),
            Err(Errno::OutOfRange)
        );
        assert!(log_sink.snapshot().is_empty(), "no record was emitted");
    }

    /// `ipc_send` to a bound endpoint with a faulting user pointer fails
    /// closed with `BadAddress` (the RustOS `EFAULT`) and delivers
    /// nothing: the page is not mapped in the caller's space.
    #[test]
    fn ipc_send_with_faulting_pointer_is_bad_address_and_delivers_nothing() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        register_port(&ipc, 1, sink);
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        // Page 2 (`0x2000`) is unmapped in the caller's space.
        assert_eq!(h.ipc_send(&ctx, 1, 0x2000, 4), Err(Errno::BadAddress));
        assert!(
            ipc.read().lookup(EndpointId(1)).expect("bound").is_empty(),
            "a faulting send must not enqueue a message"
        );
    }

    /// `ipc_send` from a caller with no registered address space (a
    /// kernel task, or one withdrawn on `exit`) fails closed with
    /// `BadAddress` — the same code a fault produces, never an oracle.
    #[test]
    fn ipc_send_without_registered_aspace_is_bad_address() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        register_port(&ipc, 1, sink);
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.ipc_send(&ctx, 1, 0x1000, 4), Err(Errno::BadAddress));
        assert!(ipc.read().lookup(EndpointId(1)).expect("bound").is_empty());
    }

    /// `ipc_send` of a payload larger than the port advertises is
    /// rejected with `MessageTooLarge` before any copy is attempted —
    /// the caller need not have a mapped buffer at all.
    #[test]
    fn ipc_send_oversize_payload_is_message_too_large() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // `register_port` binds a port with `max_payload == 64`.
        register_port(&ipc, 1, sink);
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.ipc_send(&ctx, 1, 0x1000, 65), Err(Errno::MessageTooLarge));
        assert!(ipc.read().lookup(EndpointId(1)).expect("bound").is_empty());
    }

    /// `ipc_recv` from an unbound endpoint mirrors `ipc_send`: a real
    /// lookup miss is `NotFound` with no deferral audit.
    #[test]
    fn ipc_recv_from_unbound_endpoint_is_not_found_without_deferral_audit() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(3, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(3),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        sink.clear();
        assert_eq!(h.ipc_recv(&ctx, 1, 0x2000, 8), Err(Errno::NotFound));
        assert!(sink.event_ids().is_empty());
    }

    /// Enqueue `payload` into the port bound at `endpoint`. Used to
    /// stage a message the `ipc_recv` copy-out tests then drain.
    fn enqueue(registry: &RwLock<PortRegistry>, endpoint: u64, payload: &[u8], sink: &TestSink) {
        let sender = make_caps_record(0xB1, &[], sink);
        registry
            .read()
            .lookup(EndpointId(endpoint))
            .expect("endpoint is bound")
            .send(&sender, payload, sink)
            .expect("unrestricted port accepts the send");
    }

    /// `ipc_recv` from a *bound* endpoint copies the queued message out
    /// into the caller's buffer and commits the dequeue — the
    /// increment-D.2 wiring of the user-memory copy-out path. The
    /// handler returns the payload length, the mailbox is drained, and
    /// the bytes land at the caller's pointer.
    #[test]
    fn ipc_recv_from_bound_endpoint_copies_payload_out_and_commits() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        // A writable + readable user page so `copy_out` can deliver and
        // the test can read the bytes back through `copy_in`.
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(3), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(3, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(3),
            caps: &caps,
        };

        register_port(&ipc, 1, sink);
        let payload = [0xDEu8, 0xAD, 0xBE, 0xEF];
        enqueue(&ipc, 1, &payload, sink);
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );

        assert_eq!(h.ipc_recv(&ctx, 1, 0x1000, 64), Ok(payload.len() as u64));
        // The dequeue committed: the mailbox is now empty.
        assert!(ipc.read().lookup(EndpointId(1)).expect("bound").is_empty());
        // The bytes landed at the caller's pointer.
        let read_back = h
            .with_caller_aspace(&ctx, |space, physmap| {
                let mut buf = [0u8; 4];
                copy_in(space, physmap, VirtAddr::new(0x1000), &mut buf).expect("readable");
                buf
            })
            .expect("caller has a registered space");
        assert_eq!(read_back, payload);
    }

    /// `ipc_recv` from a bound but *empty* endpoint is `WouldBlock` — a
    /// live endpoint with nothing to deliver is retryable, distinct from
    /// the `NotFound` an unbound endpoint returns.
    #[test]
    fn ipc_recv_from_empty_bound_endpoint_is_would_block() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(3), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(3, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(3),
            caps: &caps,
        };

        register_port(&ipc, 1, sink);
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.ipc_recv(&ctx, 1, 0x1000, 64), Err(Errno::WouldBlock));
    }

    /// `ipc_recv` into a buffer smaller than the queued message fails
    /// closed with `BufferTooSmall` and — because the dequeue is only
    /// committed on a successful copy — leaves the message queued for a
    /// retry with a larger buffer.
    #[test]
    fn ipc_recv_into_undersized_buffer_is_buffer_too_small_and_retains() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(3), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(3, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(3),
            caps: &caps,
        };

        register_port(&ipc, 1, sink);
        enqueue(&ipc, 1, &[1, 2, 3, 4], sink);
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );

        // A 2-byte buffer cannot hold the 4-byte message.
        assert_eq!(h.ipc_recv(&ctx, 1, 0x1000, 2), Err(Errno::BufferTooSmall));
        // Nothing was dropped: the message is still queued.
        assert_eq!(ipc.read().lookup(EndpointId(1)).expect("bound").len(), 1);
    }

    /// `ipc_recv` with a faulting user pointer fails closed with
    /// `BadAddress` and leaves the message queued (the copy-out did not
    /// commit), so a transient fault never loses a delivered message.
    #[test]
    fn ipc_recv_with_faulting_pointer_is_bad_address_and_retains() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(3), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(3, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(3),
            caps: &caps,
        };

        register_port(&ipc, 1, sink);
        enqueue(&ipc, 1, &[1, 2, 3, 4], sink);
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );

        // Page 2 (`0x2000`) is unmapped in the caller's space.
        assert_eq!(h.ipc_recv(&ctx, 1, 0x2000, 64), Err(Errno::BadAddress));
        assert_eq!(ipc.read().lookup(EndpointId(1)).expect("bound").len(), 1);
    }

    /// `ipc_recv` from a caller with no registered address space fails
    /// closed with `BadAddress` — the same code a fault produces — and
    /// is resolved before the mailbox is even peeked, so the message is
    /// left untouched.
    #[test]
    fn ipc_recv_without_registered_aspace_is_bad_address_and_retains() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(3, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(3),
            caps: &caps,
        };

        register_port(&ipc, 1, sink);
        enqueue(&ipc, 1, &[1, 2, 3, 4], sink);
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.ipc_recv(&ctx, 1, 0x1000, 64), Err(Errno::BadAddress));
        assert_eq!(ipc.read().lookup(EndpointId(1)).expect("bound").len(), 1);
    }

    /// Stage `set`'s wire form (`CapabilitySet::WIRE_LEN` bytes) at the
    /// caller's page 1 (`0x1000`) and register the caller's task id in
    /// `aspaces`. Mirrors `send_aspace` for the `cap_delegate` copy-in
    /// path; the page is `READ | USER` because the handler only reads it.
    fn register_set_at_page1(
        aspaces: &RwLock<AddressSpaceRegistry>,
        task: u64,
        set: &CapabilitySet,
    ) {
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &set.to_le_bytes());
        aspaces
            .write()
            .register(SecTaskId(task), space, physmap)
            .expect("registration succeeds");
    }

    /// `cap_delegate` copies the requested set in and narrows the target
    /// task's effective set to that subset — the increment-D.3 wiring of
    /// the capability-set copy-in + `CapTable` delegate path. The handler
    /// returns `0`, the target loses the un-delegated capability, and the
    /// `CapTable` delegate decision is audited (no
    /// `SyscallFeatureUnavailable` deferral).
    #[test]
    fn cap_delegate_copies_set_in_and_narrows_target() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;

        // Target task 10 holds two capabilities; we delegate only one.
        let target = make_caps_record(10, &[CapabilityId::FS_MOUNT, CapabilityId::DRV_LOAD], sink);
        table.write().insert(target);

        let caps = make_caps_record(4, &[CapabilityId::USER_ADMIN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(4),
            caps: &caps,
        };
        register_set_at_page1(&aspaces, 4, &caps_with(&[CapabilityId::FS_MOUNT]));

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        sink.clear();
        assert_eq!(h.cap_delegate(&ctx, 10, 0x1000), Ok(0));

        // The target's effective set was replaced with the delegated subset.
        let guard = table.read();
        let record = guard.caps_for(SecTaskId(10)).expect("target still present");
        assert!(record.has(CapabilityId::FS_MOUNT));
        assert!(!record.has(CapabilityId::DRV_LOAD));
        drop(guard);

        // The delegate decision is audited through `kernel/sec`'s own
        // `AuditEvent` (distinct from `kernel/core`'s); no `kernel/core`
        // deferral record is announced.
        let ids = sink.event_ids();
        assert!(ids.contains(
            &rustos_kernel_sec::AuditEvent::TaskCapabilitiesDelegated
                .id()
                .0
        ));
        assert!(!ids.contains(&AuditEvent::SyscallFeatureUnavailable.id().0));
    }

    /// A delegation that would *widen* the target's authority fails closed
    /// with `DelegationWiden` and leaves the target's set untouched
    /// (the central capability invariant).
    #[test]
    fn cap_delegate_rejects_widening_and_preserves_target() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;

        let target = make_caps_record(10, &[CapabilityId::FS_MOUNT], sink);
        table.write().insert(target);

        let caps = make_caps_record(4, &[CapabilityId::USER_ADMIN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(4),
            caps: &caps,
        };
        // Request a superset of the target's set: FS_MOUNT + NET_RAW.
        register_set_at_page1(
            &aspaces,
            4,
            &caps_with(&[CapabilityId::FS_MOUNT, CapabilityId::NET_RAW]),
        );

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.cap_delegate(&ctx, 10, 0x1000),
            Err(Errno::DelegationWiden)
        );
        // The target's set is unchanged: it still holds FS_MOUNT and
        // never gained NET_RAW.
        let guard = table.read();
        let record = guard.caps_for(SecTaskId(10)).expect("target still present");
        assert!(record.has(CapabilityId::FS_MOUNT));
        assert!(!record.has(CapabilityId::NET_RAW));
    }

    /// `cap_delegate` to an unknown target task is `NotFound` — a real
    /// table miss, the same condition `cap_revoke` surfaces.
    #[test]
    fn cap_delegate_unknown_target_is_not_found() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(4, &[CapabilityId::USER_ADMIN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(4),
            caps: &caps,
        };
        register_set_at_page1(&aspaces, 4, &caps_with(&[CapabilityId::FS_MOUNT]));

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.cap_delegate(&ctx, 999, 0x1000), Err(Errno::NotFound));
    }

    /// `cap_delegate` with a faulting set pointer fails closed with
    /// `BadAddress` (the RustOS `EFAULT`) and never touches the table:
    /// page 2 (`0x2000`) is unmapped in the caller's space.
    #[test]
    fn cap_delegate_with_faulting_pointer_is_bad_address() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;

        let target = make_caps_record(10, &[CapabilityId::FS_MOUNT], sink);
        table.write().insert(target);

        let caps = make_caps_record(4, &[CapabilityId::USER_ADMIN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(4),
            caps: &caps,
        };
        register_set_at_page1(&aspaces, 4, &caps_with(&[CapabilityId::FS_MOUNT]));

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.cap_delegate(&ctx, 10, 0x2000), Err(Errno::BadAddress));
        // The target's set is untouched.
        assert!(table
            .read()
            .caps_for(SecTaskId(10))
            .expect("target still present")
            .has(CapabilityId::FS_MOUNT));
    }

    /// `cap_delegate` from a caller with no registered address space
    /// fails closed with `BadAddress` rather than an oracle that
    /// distinguishes "no space" from "faulting pointer".
    #[test]
    fn cap_delegate_without_caller_aspace_is_bad_address() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;

        let target = make_caps_record(10, &[CapabilityId::FS_MOUNT], sink);
        table.write().insert(target);

        // The caller (task 4) is never registered in `aspaces`.
        let caps = make_caps_record(4, &[CapabilityId::USER_ADMIN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(4),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.cap_delegate(&ctx, 10, 0x1000), Err(Errno::BadAddress));
    }

    /// `cap_revoke` against a known task succeeds; unknown target is
    /// `NotFound`.
    #[test]
    fn cap_revoke_hits_known_task_and_misses_unknown() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;

        // Register target task 10 with FS_MOUNT.
        let record = make_caps_record(10, &[CapabilityId::FS_MOUNT], sink);
        table.write().insert(record);

        let caller_caps = make_caps_record(5, &[CapabilityId::USER_ADMIN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(5),
            caps: &caller_caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );

        // Hit: revoke FS_MOUNT from task 10.
        assert_eq!(h.cap_revoke(&ctx, 10, CapabilityId::FS_MOUNT), Ok(0));
        // The record now lacks FS_MOUNT.
        assert!(!table
            .read()
            .caps_for(SecTaskId(10))
            .expect("still present")
            .has(CapabilityId::FS_MOUNT));

        // Miss: revoke from a non-existent task.
        assert_eq!(
            h.cap_revoke(&ctx, 999, CapabilityId::FS_MOUNT),
            Err(Errno::NotFound)
        );
    }

    /// `irq_bind` succeeds for an in-range line, mints a non-zero
    /// handle, and records the binding against the caller's task id.
    /// The dispatcher's `SyscallInvoked` audit record is emitted by
    /// the outer dispatcher and is therefore not asserted here
    /// (`kernel/syscall::Dispatcher` covers it); this test asserts
    /// the handler's *behaviour* — a fresh handle returned, the
    /// table populated, no `SyscallFeatureUnavailable` emission.
    #[test]
    fn irq_bind_mints_handle_and_records_owner_against_caller() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(7, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        sink.clear();
        let raw = h.irq_bind(&ctx, 5).expect("bind succeeds");
        assert_ne!(raw, 0, "fresh handle must not be IrqHandle::INVALID");
        let entry = irq
            .lookup(IrqHandle::from_raw(raw))
            .expect("binding present");
        assert_eq!(entry.line, 5);
        assert_eq!(entry.owner, SecTaskId(7));
        // No `SyscallFeatureUnavailable` audit emission — the
        // subsystem is now wired (`docs/src/security/irq.md`
        // failure-mode table: a successful bind is audited by the
        // dispatcher's `SyscallInvoked`, not by the handler).
        assert!(
            !sink
                .event_ids()
                .contains(&AuditEvent::SyscallFeatureUnavailable.id().0),
            "deferred-feature audit must no longer fire"
        );
    }

    /// `irq_bind` rejects a line outside the configured `max_line`
    /// with `Errno::OutOfRange`. The dispatcher emits
    /// `SyscallHandlerRejected` for the failure; this test focuses
    /// on the handler's errno mapping.
    #[test]
    fn irq_bind_returns_out_of_range_for_line_above_max() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(7, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.irq_bind(&ctx, 100), Err(Errno::OutOfRange));
    }

    /// `irq_bind` rejects a duplicate binding for the same line
    /// with `Errno::OutOfRange` (the closest stable variant for
    /// "operation inapplicable to current state").
    #[test]
    fn irq_bind_rejects_duplicate_line() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(7, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let _ = h.irq_bind(&ctx, 5).expect("first bind ok");
        assert_eq!(h.irq_bind(&ctx, 5), Err(Errno::OutOfRange));
    }

    /// `irq_wait` against a forged handle (one not minted for the
    /// calling task) returns `Errno::NotFound`. The dispatcher
    /// emits the `SyscallHandlerRejected` audit record.
    #[test]
    fn irq_wait_returns_not_found_on_forged_handle() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(8, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(8),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.irq_wait(&ctx, IrqHandle::from_raw(0xDEAD_BEEF), 0),
            Err(Errno::NotFound)
        );
    }

    /// `irq_wait` with a zero-duration timeout returns
    /// `Errno::TimedOut` when the bound line has not fired.
    #[test]
    fn irq_wait_returns_timed_out_when_no_fire_within_zero_timeout() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(8, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(8),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let raw = h.irq_bind(&ctx, 5).expect("bind");
        assert_eq!(
            h.irq_wait(&ctx, IrqHandle::from_raw(raw), 0),
            Err(Errno::TimedOut)
        );
    }

    /// Permissive [`rustos_kernel_irq::IrqController`] for the
    /// pre-fired-ready test below. Accepts every line; the
    /// in-crate `UnsupportedController` would reject the test's
    /// `IrqTable::fire` call before the table could set the ready
    /// flag (`UnsupportedController::mask` always returns
    /// `MaskError::Unsupported`).
    struct PermissiveController;
    impl rustos_kernel_irq::IrqController for PermissiveController {
        fn mask(&self, _line: u32) -> Result<(), rustos_kernel_irq::MaskError> {
            Ok(())
        }
    }

    /// `irq_wait` returns `Ok(0)` when the binding has been fired
    /// before the call (the ready flag is set and the handler
    /// consumes it on the first iteration).
    #[test]
    fn irq_wait_returns_ok_when_binding_pre_fired() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController; // syscall handler does not invoke `fire`
        let permissive = PermissiveController;
        let caps = make_caps_record(8, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(8),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let raw = h.irq_bind(&ctx, 5).expect("bind");
        // Fire externally against the permissive controller (the
        // arch-port's trap path uses the controller borrowed by
        // `KernelSyscallHandlers`; this test exercises the
        // wait-side handler in isolation).
        irq.fire(5, &permissive).expect("fire");
        // Even with `timeout_ns = 0`, the pre-existing ready flag
        // is consumed on the first iteration (per the
        // ordering contract: ready beats timeout in a tie).
        assert_eq!(h.irq_wait(&ctx, IrqHandle::from_raw(raw), 0), Ok(0));
    }

    /// `exit` releases every IRQ binding the exiting task held.
    #[test]
    fn exit_releases_every_irq_binding_owned_by_task() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(9, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(9),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let _ = h.irq_bind(&ctx, 5).expect("bind 5");
        let _ = h.irq_bind(&ctx, 6).expect("bind 6");
        assert_eq!(irq.len(), 2);
        // `exit` against an unknown scheduler task returns
        // `Errno::NotFound`, but the IRQ release still happens
        // (the ordering documented in the handler's source).
        let _ = h.exit(&ctx, 0);
        assert!(irq.is_empty(), "exit must drop every binding the task held");
    }

    /// A caller holding `CAP_TIME_HIRES` reads `KernelArch::monotonic_ns`
    /// at full resolution and observes strictly-increasing values across
    /// consecutive calls (the `TestArch` impl is strictly monotonic).
    #[test]
    fn clock_get_hires_returns_raw_monotonic_ns_from_arch() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(6, &[CapabilityId::TIME_HIRES], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(6),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let a = h.clock_get(&ctx).expect("first read");
        let b = h.clock_get(&ctx).expect("second read");
        // Full resolution: consecutive single-tick reads are distinct.
        assert!(b > a, "expected b > a, got a={a} b={b}");
    }

    /// A caller *without* `CAP_TIME_HIRES` reads the monotonic clock
    /// floored to `COARSE_CLOCK_GRANULARITY_NS`, so sub-granularity
    /// detail is hidden while the reading stays
    /// monotonically non-decreasing.
    #[test]
    fn clock_get_without_hires_is_coarsened() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(6, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(6),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let g = rustos_abi::COARSE_CLOCK_GRANULARITY_NS;

        // Stage a known raw reading; the next `monotonic_ns` returns
        // `value + 1`, so a raw of `12_345` must floor to `12_000`.
        arch.set_monotonic_ns(12_344);
        let coarse = h.clock_get(&ctx).expect("coarsened read");
        assert_eq!(coarse, 12_000, "raw 12_345 must floor to 12_000");

        // Across many sub-granularity ticks the value never decreases
        // and is always a multiple of the granularity.
        arch.set_monotonic_ns(0);
        let mut last = 0;
        for _ in 0..(3 * g) {
            let v = h.clock_get(&ctx).expect("coarsened read");
            assert_eq!(v % g, 0, "coarsened reading must be a multiple of {g}");
            assert!(v >= last, "coarsened reading must not decrease");
            last = v;
        }
        assert!(last >= g, "after >{g} ticks at least one boundary crossed");
    }

    /// The same underlying instant is hidden from an untrusted caller
    /// but visible to a `CAP_TIME_HIRES` holder.
    #[test]
    fn clock_get_hires_sees_more_than_coarsened_caller() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let hires = make_caps_record(6, &[CapabilityId::TIME_HIRES], sink);
        let plain = make_caps_record(7, &[], sink);
        let hires_ctx = CallerContext {
            task_id: SecTaskId(6),
            caps: &hires,
        };
        let plain_ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &plain,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );

        arch.set_monotonic_ns(7_000);
        let raw = h.clock_get(&hires_ctx).expect("hires read"); // 7_001
        arch.set_monotonic_ns(7_000);
        let coarse = h.clock_get(&plain_ctx).expect("coarse read"); // 7_000
        assert_eq!(raw, 7_001);
        assert_eq!(coarse, 7_000);
        assert!(raw > coarse, "hires caller resolves the sub-µs detail");
    }

    /// `random_get` refuses an over-large request up front with
    /// `LengthOutOfRange`, before it consults the reserve or the
    /// caller's buffer.
    #[test]
    fn random_get_rejects_request_above_cap() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(11, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(11),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        sink.clear();
        assert_eq!(
            h.random_get(
                &ctx,
                0x4000,
                rustos_abi::RANDOM_REQUEST_MAX_BYTES + 1,
                RandomFlags::empty()
            ),
            Err(Errno::LengthOutOfRange)
        );
        // Refused before consulting the reserve or the caller's buffer.
        assert!(sink.event_ids().is_empty());
    }

    /// A zero-length `random_get` succeeds with `0` without consulting
    /// the reserve or the caller's buffer — it returns before
    /// `with_caller_aspace`, so even an unseeded reserve and an
    /// unregistered caller are fine.
    #[test]
    fn random_get_zero_len_is_ok_without_touching_reserve() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(12, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(12),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.random_get(&ctx, 0x1000, 0, RandomFlags::empty()), Ok(0));
    }

    /// `random_get` against the **unseeded** boot reserve fails closed
    /// with `EntropyNotReady` (never weak bytes before
    /// the RNG is seeded), even with a perfectly writable buffer, and
    /// emits no `SyscallFeatureUnavailable` deferral.
    #[test]
    fn random_get_unseeded_reserve_is_entropy_not_ready() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(12), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(12, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(12),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        sink.clear();
        assert_eq!(
            h.random_get(&ctx, 0x1000, 32, RandomFlags::NON_BLOCKING),
            Err(Errno::EntropyNotReady)
        );
        assert!(!sink
            .event_ids()
            .contains(&AuditEvent::SyscallFeatureUnavailable.id().0));
    }

    /// `random_get` against a **seeded** reserve copies the requested
    /// bytes into the caller's buffer and returns the count — the
    /// increment-D.4 wiring of the output-reserve copy-out path. The
    /// bytes land at the caller's pointer (CSPRNG output is non-zero
    /// w.h.p.) and no deferral is announced.
    #[test]
    fn random_get_copies_bytes_out_from_seeded_reserve() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = seeded_rng();
        aspaces
            .write()
            .register(SecTaskId(12), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(12, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(12),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        sink.clear();
        assert_eq!(h.random_get(&ctx, 0x1000, 32, RandomFlags::empty()), Ok(32));
        assert!(!sink
            .event_ids()
            .contains(&AuditEvent::SyscallFeatureUnavailable.id().0));
        // The bytes landed at the caller's pointer and are not all zero
        // (a seeded CSPRNG never returns an all-zero block here).
        let delivered = h
            .with_caller_aspace(&ctx, |space, physmap| {
                let mut buf = [0u8; 32];
                copy_in(space, physmap, VirtAddr::new(0x1000), &mut buf).expect("readable");
                buf
            })
            .expect("caller has a registered space");
        assert_ne!(delivered, [0u8; 32], "the reserve wrote real output");
    }

    /// `random_get` from a seeded reserve but a caller with no registered
    /// address space fails closed with `BadAddress` (the RustOS
    /// `EFAULT`) — the same code every copy-path handler returns.
    #[test]
    fn random_get_without_registered_aspace_is_bad_address() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = seeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(12, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(12),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.random_get(&ctx, 0x1000, 32, RandomFlags::empty()),
            Err(Errno::BadAddress)
        );
    }

    /// `random_get` with a faulting user pointer fails closed with
    /// `BadAddress`: page 2 (`0x2000`) is unmapped in the caller's space,
    /// so the copy-out cannot deliver.
    #[test]
    fn random_get_with_faulting_pointer_is_bad_address() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = seeded_rng();
        aspaces
            .write()
            .register(SecTaskId(12), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(12, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(12),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.random_get(&ctx, 0x2000, 32, RandomFlags::empty()),
            Err(Errno::BadAddress)
        );
    }

    /// Page `n`'s base virtual address, as a [`Page`].
    fn page(n: u64) -> Page {
        Page::from_addr(VirtAddr::new(n * PAGE_SIZE as u64)).expect("aligned page")
    }

    /// A boxed user address space with page `n` → frame `frame` mapped
    /// `USER | READ`, behind the object-safe trait the registry stores.
    fn user_space(n: u64, frame: usize) -> Box<dyn UserAddressSpace + Send + Sync> {
        let mut space = AddressSpace::new(HostPageTable::new());
        space
            .map(page(n), Frame(frame), MapFlags::READ | MapFlags::USER)
            .expect("mapped");
        Box::new(space)
    }

    /// A boxed single-page direct physical map for the registry entry.
    fn sim_map() -> Box<dyn PhysMap + Send + Sync> {
        Box::new(SimPhysMap::new(PhysAddr::new(0), PAGE_SIZE))
    }

    /// `with_caller_aspace` resolves a registered caller to its address
    /// space and runs the closure against the borrowed pair — the
    /// increment-C bridge from `caller.task_id` to the user mappings the
    /// copy path walks.
    #[test]
    fn with_caller_aspace_runs_closure_against_registered_caller() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(5), user_space(1, 9), sim_map())
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(5, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(5),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        // The closure sees the caller's own address space: page 1
        // resolves to frame 9 with the flags it was mapped with.
        let resolved = h.with_caller_aspace(&ctx, |space, _physmap| space.translate(page(1)));
        assert_eq!(
            resolved,
            Some(Some((Frame(9), MapFlags::READ | MapFlags::USER)))
        );
    }

    /// `with_caller_aspace` fails closed with `None` (never invoking the
    /// closure) when the caller has no registered address space — a
    /// kernel task, or a task already withdrawn on `exit`.
    #[test]
    fn with_caller_aspace_returns_none_for_unregistered_caller() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(6, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(6),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let mut ran = false;
        let resolved = h.with_caller_aspace(&ctx, |_space, _physmap| {
            ran = true;
            0u8
        });
        assert_eq!(resolved, None);
        assert!(!ran, "the closure must not run when no entry resolves");
    }

    /// Each caller resolves to *its own* address space: the bridge keys
    /// strictly on `caller.task_id`, never leaking one task's mappings
    /// to another.
    #[test]
    fn with_caller_aspace_resolves_only_the_calling_task() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(1), user_space(1, 100), sim_map())
            .expect("task 1 registers");
        aspaces
            .write()
            .register(SecTaskId(2), user_space(1, 200), sim_map())
            .expect("task 2 registers");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps1 = make_caps_record(1, &[], sink);
        let caps2 = make_caps_record(2, &[], sink);
        let ctx1 = CallerContext {
            task_id: SecTaskId(1),
            caps: &caps1,
        };
        let ctx2 = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps2,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let frame1 = h
            .with_caller_aspace(&ctx1, |space, _| space.translate(page(1)).map(|(f, _)| f))
            .expect("task 1 resolves");
        let frame2 = h
            .with_caller_aspace(&ctx2, |space, _| space.translate(page(1)).map(|(f, _)| f))
            .expect("task 2 resolves");
        assert_eq!(frame1, Some(Frame(100)));
        assert_eq!(frame2, Some(Frame(200)));
    }

    /// A console sink that records every byte handed to it, for the
    /// `stream_write` handler tests.
    struct RecordingConsole {
        written: rustos_sync::SpinLock<alloc::vec::Vec<u8>>,
    }

    impl RecordingConsole {
        fn new() -> Self {
            Self {
                written: rustos_sync::SpinLock::new(alloc::vec::Vec::new()),
            }
        }
    }

    impl crate::console::ConsoleWrite for RecordingConsole {
        fn write(&self, bytes: &[u8]) -> Result<usize, Errno> {
            self.written.lock().extend_from_slice(bytes);
            Ok(bytes.len())
        }
    }

    /// Leak a single-console list whose write half is `write` and whose
    /// read half fails closed — the write-side test fixture for
    /// [`KernelSyscallHandlers::with_consoles`].
    fn single_write_console(
        write: &'static (dyn crate::console::ConsoleWrite + 'static),
    ) -> &'static [ConsoleDevice] {
        Box::leak(Box::new([ConsoleDevice::new(
            write,
            &crate::console::NULL_CONSOLE_READ,
        )]))
    }

    /// Leak a single-console list whose read half is `read` and whose
    /// write half fails closed — the read-side test fixture for
    /// [`KernelSyscallHandlers::with_consoles`].
    fn single_read_console(
        read: &'static (dyn crate::console::ConsoleRead + Sync + 'static),
    ) -> &'static [ConsoleDevice] {
        Box::leak(Box::new([ConsoleDevice::new(
            &crate::console::NULL_CONSOLE,
            read,
        )]))
    }

    /// `stream_write` copies the caller's bytes in and hands the exact
    /// buffer to the installed console, returning the byte count.
    #[test]
    fn console_write_copies_in_and_emits_to_installed_console() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let banner = b"RustOS init: hello\n";
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, banner);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let console: &'static RecordingConsole = Box::leak(Box::new(RecordingConsole::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(single_write_console(console));
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());

        assert_eq!(
            h.stream_write(&ctx, STDOUT, 0x1000, banner.len()),
            Ok(banner.len() as u64)
        );
        assert_eq!(console.written.lock().as_slice(), banner);
    }

    /// With no console installed the handler holds `NULL_CONSOLE` and
    /// fails closed with `NotImplemented` rather than silently dropping
    /// the bytes. The user copy still succeeds first.
    #[test]
    fn console_write_without_device_is_not_implemented() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, b"hi");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());
        assert_eq!(
            h.stream_write(&ctx, STDOUT, 0x1000, 2),
            Err(Errno::NotImplemented)
        );
    }

    /// A zero-length `stream_write` to an installed console succeeds
    /// without touching the caller's buffer or the device; with **no**
    /// console installed even a zero-length write announces the inert
    /// interface (the descriptor's backing is
    /// resolved before anything else).
    #[test]
    fn console_write_zero_length_is_ok_and_inert() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // No address space registered: a real copy would fail closed,
        // but a zero-length write to an *open* descriptor never reaches
        // the copy path. The descriptor table is still established so the
        // write is to a valid (writable) stream.
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());
        let console: &'static RecordingConsole = Box::leak(Box::new(RecordingConsole::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(single_write_console(console));
        assert_eq!(h.stream_write(&ctx, STDOUT, 0x1000, 0), Ok(0));
        // The device was never touched.
        assert!(console.written.lock().is_empty());

        // With no console installed the descriptor resolves to no
        // backing and even a zero-length write fails closed.
        let bare = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            bare.stream_write(&ctx, STDOUT, 0x1000, 0),
            Err(Errno::NotImplemented)
        );
    }

    /// `stream_write` from a caller with no registered address space
    /// fails closed with `BadAddress`, never leaking the missing-space
    /// case.
    #[test]
    fn console_write_unregistered_caller_is_bad_address() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let console: &'static RecordingConsole = Box::leak(Box::new(RecordingConsole::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(single_write_console(console));
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());
        assert_eq!(
            h.stream_write(&ctx, STDOUT, 0x1000, 4),
            Err(Errno::BadAddress)
        );
        assert!(console.written.lock().is_empty());
    }

    /// A `len` above `CONSOLE_WRITE_MAX` writes a bounded prefix and
    /// reports the count — POSIX short-write semantics, never an
    /// unbounded kernel allocation.
    #[test]
    fn console_write_caps_length_at_console_write_max() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        // One full page of readable user bytes backs the bounded copy.
        let page_bytes = alloc::vec![0x5Au8; PAGE_SIZE];
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &page_bytes);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let console: &'static RecordingConsole = Box::leak(Box::new(RecordingConsole::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(single_write_console(console));
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());
        // Ask for more than the cap; the handler writes exactly the cap.
        let r = h.stream_write(&ctx, STDOUT, 0x1000, CONSOLE_WRITE_MAX + 100);
        assert_eq!(r, Ok(CONSOLE_WRITE_MAX as u64));
        assert_eq!(console.written.lock().len(), CONSOLE_WRITE_MAX);
    }

    /// A console input source that yields a preset byte string and
    /// records the length of the buffer it was handed, for the
    /// `stream_read` handler tests. It fills the caller's buffer with up
    /// to `buf.len()` bytes of the preset input (a real device's
    /// short-read behaviour) and reports the count.
    struct RecordingConsoleRead {
        input: alloc::vec::Vec<u8>,
        last_buf_len: rustos_sync::SpinLock<Option<usize>>,
    }

    impl RecordingConsoleRead {
        fn new(input: &[u8]) -> Self {
            Self {
                input: input.to_vec(),
                last_buf_len: rustos_sync::SpinLock::new(None),
            }
        }
    }

    impl crate::console::ConsoleRead for RecordingConsoleRead {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            *self.last_buf_len.lock() = Some(buf.len());
            let n = core::cmp::min(buf.len(), self.input.len());
            buf[..n].copy_from_slice(&self.input[..n]);
            Ok(n)
        }
    }

    /// `stream_read` reads the device bytes into the kernel staging
    /// buffer and copies them out to the caller, returning the count. The
    /// bytes land at the caller's pointer.
    #[test]
    fn console_read_copies_device_bytes_out_to_caller() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let line = b"login: \n";
        let console: &'static RecordingConsoleRead =
            Box::leak(Box::new(RecordingConsoleRead::new(line)));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(single_read_console(console));
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());

        assert_eq!(
            h.stream_read(&ctx, STDIN, 0x1000, 64),
            Ok(line.len() as u64)
        );
        // The bytes landed at the caller's pointer.
        let delivered = h
            .with_caller_aspace(&ctx, |space, physmap| {
                let mut buf = [0u8; 8];
                copy_in(space, physmap, VirtAddr::new(0x1000), &mut buf).expect("readable");
                buf
            })
            .expect("caller has a registered space");
        assert_eq!(&delivered, line);
    }

    /// With no console installed the handler holds `NULL_CONSOLE_READ`
    /// and fails closed with `NotImplemented` rather than fabricating
    /// input.
    #[test]
    fn console_read_without_device_is_not_implemented() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());
        assert_eq!(
            h.stream_read(&ctx, STDIN, 0x1000, 8),
            Err(Errno::NotImplemented)
        );
    }

    /// A zero-length `stream_read` from an installed console succeeds
    /// without touching the device or the caller's buffer; with **no**
    /// console installed even a zero-length read announces the inert
    /// interface (the descriptor's backing is
    /// resolved before anything else).
    #[test]
    fn console_read_zero_length_is_ok_and_inert() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // No address space registered: a real copy would fail closed,
        // but a zero-length read to an *open* descriptor never reaches
        // the device or copy path.
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());
        let console: &'static RecordingConsoleRead =
            Box::leak(Box::new(RecordingConsoleRead::new(b"unseen")));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(single_read_console(console));
        assert_eq!(h.stream_read(&ctx, STDIN, 0x1000, 0), Ok(0));
        // The device was never read.
        assert_eq!(*console.last_buf_len.lock(), None);

        // With no console installed the descriptor resolves to no
        // backing and even a zero-length read fails closed.
        let bare = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            bare.stream_read(&ctx, STDIN, 0x1000, 0),
            Err(Errno::NotImplemented)
        );
    }

    /// A device with no input pending reports a zero-length read without
    /// touching the caller's buffer; the caller loops (
    /// short-read semantics).
    #[test]
    fn console_read_no_input_pending_reports_zero() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let console: &'static RecordingConsoleRead =
            Box::leak(Box::new(RecordingConsoleRead::new(&[])));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(single_read_console(console));
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());
        assert_eq!(h.stream_read(&ctx, STDIN, 0x1000, 8), Ok(0));
    }

    /// A `len` above `CONSOLE_READ_MAX` hands the device a bounded buffer
    /// and reports the bounded count — never an unbounded kernel
    /// allocation.
    #[test]
    fn console_read_caps_length_at_console_read_max() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        // One full page of writable user bytes backs the bounded copy.
        let page_bytes = alloc::vec![0u8; PAGE_SIZE];
        let (space, physmap) = send_aspace(
            MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
            &page_bytes,
        );
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // The device has more bytes than the cap, so the read is bounded
        // by `CONSOLE_READ_MAX`, not by the device.
        let input = alloc::vec![0x41u8; CONSOLE_READ_MAX + 100];
        let console: &'static RecordingConsoleRead =
            Box::leak(Box::new(RecordingConsoleRead::new(&input)));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(single_read_console(console));
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());
        let r = h.stream_read(&ctx, STDIN, 0x1000, CONSOLE_READ_MAX + 100);
        assert_eq!(r, Ok(CONSOLE_READ_MAX as u64));
        // The device was handed exactly the capped buffer, never the
        // caller's oversized request.
        assert_eq!(*console.last_buf_len.lock(), Some(CONSOLE_READ_MAX));
    }

    /// `stream_read` from a caller with no registered address space
    /// fails closed with `BadAddress`, never leaking the missing-space
    /// case.
    #[test]
    fn console_read_unregistered_caller_is_bad_address() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let console: &'static RecordingConsoleRead =
            Box::leak(Box::new(RecordingConsoleRead::new(b"data")));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(single_read_console(console));
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());
        assert_eq!(
            h.stream_read(&ctx, STDIN, 0x1000, 4),
            Err(Errno::BadAddress)
        );
    }

    /// `stream_write` to a descriptor that is not a writable inherited
    /// stream fails closed with `NotFound` before any copy — the
    /// descriptor table, not an ambient device, is the authority. The cases: a read-only fd (`STDIN`), an
    /// out-of-range fd, and a caller whose table is the closed default.
    #[test]
    fn stream_write_to_non_writable_fd_is_not_found() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let banner = b"nope";
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, banner);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let console: &'static RecordingConsole = Box::leak(Box::new(RecordingConsole::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(single_write_console(console));
        // `STDIN` is read-only; an out-of-range fd resolves to Closed.
        assert_eq!(h.stream_write(&ctx, STDIN, 0x1000, 4), Err(Errno::NotFound));
        assert_eq!(h.stream_write(&ctx, 99, 0x1000, 4), Err(Errno::NotFound));
        // The device was never reached.
        assert!(console.written.lock().is_empty());
    }

    /// `stream_read` from a descriptor that is not a readable inherited
    /// stream fails closed with `NotFound` before touching the device: a write-only fd (`STDOUT`) and a closed
    /// (unestablished) caller.
    #[test]
    fn stream_read_from_non_readable_fd_is_not_found() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let console: &'static RecordingConsoleRead =
            Box::leak(Box::new(RecordingConsoleRead::new(b"data")));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(single_read_console(console));
        // `STDOUT` is write-only here, so a read of it is refused.
        assert_eq!(h.stream_read(&ctx, STDOUT, 0x1000, 4), Err(Errno::NotFound));
        // The device was never read.
        assert_eq!(*console.last_buf_len.lock(), None);
    }

    /// A caller whose descriptor table was never established (the
    /// fail-closed `Closed` default) cannot reach any stream backing: both directions deny with `NotFound`.
    #[test]
    fn stream_ops_without_established_table_are_not_found() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // No `set_streams`: the caller resolves to the closed default.
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.stream_write(&ctx, STDOUT, 0x1000, 4),
            Err(Errno::NotFound)
        );
        assert_eq!(h.stream_read(&ctx, STDIN, 0x1000, 4), Err(Errno::NotFound));
    }

    /// A live frame allocator over 64 usable frames — enough to pass the
    /// `spawn` handler's "subsystem wired" gate. The host `ProcessSpawn`
    /// double never actually allocates from it (it admits a host-built
    /// space), so its capacity is irrelevant beyond being non-empty.
    fn spawn_test_frames() -> FrameAllocator {
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            start: PhysAddr::new(0),
            length: (PAGE_SIZE as u64) * 64,
            kind: RegionKind::Usable,
        });
        FrameAllocator::new(&map).expect("frame allocator builds")
    }

    /// Absolute program path the spawn tests register and look up.
    static SPAWN_PATH: &[u8] = b"/Apps/Child.app/Run";
    /// Stand-in `rxe` bytes; the host producer double only records them,
    /// it does not parse them (parsing is the arch producer's job, `SP3b`).
    static SPAWN_RXE: &[u8] = b"child-rxe-blob";

    /// A `ProcessSpawn` double that admits a freshly built **host**
    /// address space through `ctx.admit_process` and records the `rxe` it
    /// was handed, returning the new PID. It proves the core admit
    /// machinery (scheduler + caps + aspace registration) end-to-end
    /// without the arch-specific image build (`plans/SPAWN.md` SP3 host
    /// proof; `SP3b` wires the real aarch64 producer).
    struct RecordingSpawn {
        seen_rxe: rustos_sync::SpinLock<alloc::vec::Vec<u8>>,
    }

    impl RecordingSpawn {
        fn new() -> Self {
            Self {
                seen_rxe: rustos_sync::SpinLock::new(alloc::vec::Vec::new()),
            }
        }
    }

    impl ProcessSpawn for RecordingSpawn {
        fn spawn(&self, program: &EmbeddedProgram, ctx: &dyn SpawnCtx) -> Result<u64, Errno> {
            self.seen_rxe.lock().extend_from_slice(program.rxe);
            // Build a one-page host user space and freeze it into the
            // registry-storable snapshot, exactly as the real producer
            // freezes its built image.
            let mut space = AddressSpace::new(HostPageTable::new());
            space
                .map(
                    Page::from_addr(VirtAddr::new(0x1000)).expect("aligned"),
                    Frame(9),
                    MapFlags::READ | MapFlags::USER,
                )
                .expect("host map");
            let frozen: Box<dyn UserAddressSpace + Send + Sync> = Box::new(space.freeze());
            let physmap: Box<dyn PhysMap + Send + Sync> =
                Box::new(SimPhysMap::new(PhysAddr::new(0), PAGE_SIZE));
            let mut child_caps = CapabilitySet::empty();
            child_caps.insert(CapabilityId::CONSOLE_WRITE);
            // Inert closures: a host test never enters user mode or
            // reactivates a page-table root.
            let pre_resume: Box<dyn FnMut(u64) + Send> = Box::new(|_stack_top| {});
            let enter: Box<dyn FnMut() + Send> = Box::new(|| {});
            // The host double hands a plain software-canary `BoxStack` — the
            // arena-backed guard-page stack is the arch producer's job
            // (`plans/PI.md` G3b-2), unreachable from a host test.
            let stack: Box<dyn crate::kthread::KernelStack + Send> =
                Box::new(crate::kthread::BoxStack::new());
            // SAFETY: the host test never dispatches the admitted task, so
            // the (inert) `enter`/`pre_resume` closures never run and the
            // frozen host space need only answer `translate`; it faithfully
            // describes the one page mapped above. The host double retains no
            // live space (`None`), so the child's `mem_map`/`mmio_map` would
            // fail closed — unexercised here.
            unsafe {
                ctx.admit_process(child_caps, frozen, physmap, stack, pre_resume, None, enter)
            }
            .map_err(|_| Errno::NoSpace)
        }
    }

    /// Build a handler whose `spawn` subsystem is fully wired: frames +
    /// a registry holding `SPAWN_PATH` + a producer.
    #[allow(clippy::too_many_arguments)]
    fn spawn_handler<'a>(
        sched: &'a Scheduler<TestArch>,
        table: &'a RwLock<CapTable>,
        arch: &'a TestArch,
        sink: &'static TestSink,
        irq: &'a IrqTable,
        ctl: &'a UnsupportedController,
        ipc: &'a RwLock<PortRegistry>,
        aspaces: &'a RwLock<AddressSpaceRegistry>,
        rng: &'a RwLock<Box<dyn RandomReserve + Send + Sync>>,
        frames: &'a FrameAllocator,
        programs: &'static ProgramRegistry,
        producer: &'static (dyn ProcessSpawn + 'static),
    ) -> KernelSyscallHandlers<'a, TestArch> {
        KernelSyscallHandlers::new(sched, table, arch, sink, irq, ctl, ipc, aspaces, rng)
            .with_frames(frames)
            .with_spawn(programs, producer)
    }

    /// `spawn` copies the caller's path in, resolves the embedded program,
    /// builds + admits the child, and returns its PID — and the child is
    /// registered with the scheduler, capability table, and aspace
    /// registry (`plans/SPAWN.md` SP3).
    #[test]
    fn spawn_resolves_path_builds_and_admits_child_returning_pid() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, SPAWN_PATH);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("caller registration");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let frames = spawn_test_frames();
        let caps = make_caps_record(2, &[CapabilityId::PROC_SPAWN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let programs: &'static ProgramRegistry =
            Box::leak(Box::new(ProgramRegistry::new(Box::leak(Box::new([
                EmbeddedProgram {
                    path: SPAWN_PATH,
                    rxe: SPAWN_RXE,
                    caps: &[],
                    args: &[],
                },
            ])))));
        let producer: &'static RecordingSpawn = Box::leak(Box::new(RecordingSpawn::new()));

        let h = spawn_handler(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng, &frames, programs,
            producer,
        );

        let before = sched.live_task_count();
        let pid = h
            .spawn(&ctx, 0x1000, SPAWN_PATH.len(), CONSOLE_INHERIT)
            .expect("spawn succeeds");
        assert!(
            sched.live_task_count() > before,
            "child must be admitted as a live task"
        );
        assert!(
            table.read().caps_for(SecTaskId(pid)).is_some(),
            "child caps registered under its pid"
        );
        assert!(
            aspaces.read().contains(SecTaskId(pid)),
            "child address space registered under its pid"
        );
        assert_eq!(producer.seen_rxe.lock().as_slice(), SPAWN_RXE);
        // A user-driven `spawn` grants the child no device resources: the
        // handler passes an empty grant slice, so the child holds no
        // resolvable handle (no ambient authority).
        assert_eq!(aspaces.read().grant(SecTaskId(pid), 1), None);
    }

    /// The privileged driver-spawn path mints one device-resource grant
    /// per the matched node's requested [`HwResource`] for the freshly
    /// admitted child, keyed to the child's own kernel-trusted id: the
    /// child resolves each handle (the `resource_grants` / `mmio_map`
    /// source), handles are monotonic from `1` in request order, and a
    /// different task presenting the same numeric handle resolves nothing
    /// — the registry owner-check.
    #[test]
    fn driver_spawn_mints_an_owner_checked_grant_per_requested_resource() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let frames = spawn_test_frames();

        // The matched node's two requested resources: a register window and
        // a DMA constraint. These originate kernel-side (the driver-spawn
        // path threads the discovered node's requests), never a caller.
        let regs = HwResource::mmio(0x0a00_0000, 0x1000);
        let dma = HwResource::dma(0, 0x1000);
        let requested = [regs, dma];

        let ctx = KernelSpawnCtx::new(
            &frames,
            None,
            sink,
            &sched,
            &table,
            &aspaces,
            arch.as_ref(),
            SecTaskId(1),
            &NULL_PROCESS_WAIT,
            DescriptorTable::standard(),
            &requested,
            // The matched node the driver was loaded for.
            Some(0x55),
            // A fixed minted identity so the admit path's attestation is
            // observable.
            rustos_abi::ProcId::from_raw([0x11; 16]),
        );

        let program = EmbeddedProgram {
            path: SPAWN_PATH,
            rxe: SPAWN_RXE,
            caps: &[],
            args: &[],
        };
        // `RecordingSpawn::spawn` admits a host child through
        // `ctx.admit_process`, returning the new PID the grants are minted
        // against.
        let pid = RecordingSpawn::new()
            .spawn(&program, &ctx)
            .expect("driver child admitted");
        let child = SecTaskId(pid);

        // The minted process-instance identity is attested onto the child's
        // capability record by `admit_process`, distinct from its numeric id.
        assert_eq!(
            table.read().caps_for(child).map(TaskCapabilities::proc_id),
            Some(rustos_abi::ProcId::from_raw([0x11; 16]))
        );

        // The driver's matched node is recorded against it, so a later
        // `hw_emit_node` parents its published child under exactly this node
        // (the emitter cannot forge its position).
        assert_eq!(aspaces.read().loaded_node(child), Some(0x55));
        // A different task has no loaded node (owner-bound, fail closed).
        assert_eq!(aspaces.read().loaded_node(SecTaskId(pid + 1)), None);

        // One handle per requested resource, monotonic from 1, in order.
        assert_eq!(aspaces.read().grant(child, 1), Some(regs));
        assert_eq!(aspaces.read().grant(child, 2), Some(dma));
        // No third grant was minted.
        assert_eq!(aspaces.read().grant(child, 3), None);
        // Owner-check: a different task presenting the same handle value
        // resolves nothing — a driver cannot reach another's window by
        // guessing a handle.
        assert_eq!(aspaces.read().grant(SecTaskId(pid + 1), 1), None);

        // The set serialises for delivery through `resource_grants`: two
        // consecutive `GrantedResource` records in ascending handle order.
        let wire = aspaces.read().grants_to_le_bytes(child);
        assert_eq!(
            wire.len(),
            2 * rustos_abi::hwtree::GrantedResource::WIRE_LEN
        );
        let first =
            rustos_abi::hwtree::GrantedResource::from_bytes(&wire).expect("first record decodes");
        assert_eq!(first.handle, 1);
        assert_eq!(first.resource, regs);
        let second = rustos_abi::hwtree::GrantedResource::from_bytes(
            &wire[rustos_abi::hwtree::GrantedResource::WIRE_LEN..],
        )
        .expect("second record decodes");
        assert_eq!(second.handle, 2);
        assert_eq!(second.resource, dma);
    }

    /// A [`ProcessSpawn`] double that records whether the [`SpawnCtx`] it
    /// is handed exposes a `'static` page-table frame allocator. It never admits a task — it returns
    /// `NotImplemented` after recording — so a test can assert the wiring
    /// without standing up an arch image build.
    struct PageTableAllocProbeSpawn {
        saw_static_allocator: core::sync::atomic::AtomicBool,
    }

    impl PageTableAllocProbeSpawn {
        fn new() -> Self {
            Self {
                saw_static_allocator: core::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    impl ProcessSpawn for PageTableAllocProbeSpawn {
        fn spawn(&self, _program: &EmbeddedProgram, ctx: &dyn SpawnCtx) -> Result<u64, Errno> {
            self.saw_static_allocator.store(
                ctx.page_table_allocator().is_some(),
                core::sync::atomic::Ordering::SeqCst,
            );
            Err(Errno::NotImplemented)
        }
    }

    /// When the boot path threads a `'static` page-table allocator through
    /// [`KernelSyscallHandlers::with_page_table_frames`], the producer sees
    /// it via [`SpawnCtx::page_table_allocator`] — the seam an arch producer
    /// builds a child's page tables out of reclaimable RAM through.
    #[test]
    fn spawn_threads_the_static_page_table_allocator_to_the_producer() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, SPAWN_PATH);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("caller registration");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let frames = spawn_test_frames();
        // The page-table allocator is a `'static` borrow in production; leak
        // a test one so the type matches.
        let ptf: &'static FrameAllocator = Box::leak(Box::new(spawn_test_frames()));
        let caps = make_caps_record(2, &[CapabilityId::PROC_SPAWN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let programs: &'static ProgramRegistry =
            Box::leak(Box::new(ProgramRegistry::new(Box::leak(Box::new([
                EmbeddedProgram {
                    path: SPAWN_PATH,
                    rxe: SPAWN_RXE,
                    caps: &[],
                    args: &[],
                },
            ])))));
        let probe: &'static PageTableAllocProbeSpawn =
            Box::leak(Box::new(PageTableAllocProbeSpawn::new()));

        // Wired: the producer must observe `Some`.
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_frames(&frames)
        .with_page_table_frames(ptf)
        .with_spawn(programs, probe);
        // The probe records and returns `NotImplemented`; the recording, not
        // the result, is what proves the wiring.
        let _ = h.spawn(&ctx, 0x1000, SPAWN_PATH.len(), CONSOLE_INHERIT);
        assert!(
            probe
                .saw_static_allocator
                .load(core::sync::atomic::Ordering::SeqCst),
            "the producer must see the wired `'static` page-table allocator"
        );

        // Unwired: with no `with_page_table_frames`, the producer sees `None`
        // and an arch producer fails closed.
        probe
            .saw_static_allocator
            .store(true, core::sync::atomic::Ordering::SeqCst);
        let h2 = spawn_handler(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng, &frames, programs, probe,
        );
        let _ = h2.spawn(&ctx, 0x1000, SPAWN_PATH.len(), CONSOLE_INHERIT);
        assert!(
            !probe
                .saw_static_allocator
                .load(core::sync::atomic::Ordering::SeqCst),
            "with no page-table allocator wired the producer must see None"
        );
    }

    /// A spawned child inherits the parent's effective resource limits,
    /// intersected against the system default so it can never widen past
    /// the parent's ceiling (inheritance across spawn).
    #[test]
    fn spawn_child_inherits_the_parents_resource_limits() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, SPAWN_PATH);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("caller registration");
        // The parent caps its own child-process fan-out below the default.
        let parent_cap = ResourceLimit::new(2, 4).expect("well-formed");
        aspaces
            .write()
            .set_limit(SecTaskId(2), LimitKind::Processes, parent_cap);
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let frames = spawn_test_frames();
        let caps = make_caps_record(2, &[CapabilityId::PROC_SPAWN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let programs: &'static ProgramRegistry =
            Box::leak(Box::new(ProgramRegistry::new(Box::leak(Box::new([
                EmbeddedProgram {
                    path: SPAWN_PATH,
                    rxe: SPAWN_RXE,
                    caps: &[],
                    args: &[],
                },
            ])))));
        let producer: &'static RecordingSpawn = Box::leak(Box::new(RecordingSpawn::new()));

        let h = spawn_handler(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng, &frames, programs,
            producer,
        );

        let pid = h
            .spawn(&ctx, 0x1000, SPAWN_PATH.len(), CONSOLE_INHERIT)
            .expect("spawn succeeds");
        // The child carries the parent's tighter Processes ceiling and the
        // default policy for every other kind.
        let child = aspaces.read().limits(SecTaskId(pid));
        assert_eq!(child.get(LimitKind::Processes), parent_cap);
        assert_eq!(child.get(LimitKind::StackBytes), ResourceLimit::UNLIMITED);
    }

    /// The spawn admit path records the new child against the spawning
    /// caller (the parent) with the process-wait producer, so a later `wait`
    /// from that parent can reap it (`plans/SPAWN.md` SP6).
    #[test]
    fn spawn_admit_registers_child_with_process_wait_producer() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, SPAWN_PATH);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("caller registration");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let frames = spawn_test_frames();
        let caps = make_caps_record(2, &[CapabilityId::PROC_SPAWN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let programs: &'static ProgramRegistry =
            Box::leak(Box::new(ProgramRegistry::new(Box::leak(Box::new([
                EmbeddedProgram {
                    path: SPAWN_PATH,
                    rxe: SPAWN_RXE,
                    caps: &[],
                    args: &[],
                },
            ])))));
        let producer: &'static RecordingSpawn = Box::leak(Box::new(RecordingSpawn::new()));
        let wait_producer: &'static RecordingProcessWait = Box::leak(Box::new(
            RecordingProcessWait::new(Ok(crate::procwait::ReapedChild { pid: 0, code: 0 })),
        ));

        let h = spawn_handler(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng, &frames, programs,
            producer,
        )
        .with_process_wait(wait_producer);

        let pid = h
            .spawn(&ctx, 0x1000, SPAWN_PATH.len(), CONSOLE_INHERIT)
            .expect("spawn succeeds");
        // The child (its returned PID) was registered against parent 2.
        assert_eq!(*wait_producer.last_register.lock(), Some((2, pid)));
    }

    /// With no spawn producer wired the handler holds `NULL_PROCESS_SPAWN`
    /// and fails closed with `NotImplemented` — but only
    /// after the path resolves, proving the null producer is reached.
    #[test]
    fn spawn_without_producer_is_not_implemented() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, SPAWN_PATH);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("caller registration");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let frames = spawn_test_frames();
        let caps = make_caps_record(2, &[CapabilityId::PROC_SPAWN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let programs: &'static ProgramRegistry =
            Box::leak(Box::new(ProgramRegistry::new(Box::leak(Box::new([
                EmbeddedProgram {
                    path: SPAWN_PATH,
                    rxe: SPAWN_RXE,
                    caps: &[],
                    args: &[],
                },
            ])))));

        // Registry + frames wired, but the producer is the null default.
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_frames(&frames)
        .with_spawn(programs, &NULL_PROCESS_SPAWN);

        assert_eq!(
            h.spawn(&ctx, 0x1000, SPAWN_PATH.len(), CONSOLE_INHERIT),
            Err(Errno::NotImplemented)
        );
    }

    /// With no frame allocator threaded the spawn subsystem is unwired, so
    /// `spawn` fails closed with `NotImplemented` before touching any
    /// state — the boot default.
    #[test]
    fn spawn_without_frames_is_not_implemented() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::PROC_SPAWN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.spawn(&ctx, 0x1000, SPAWN_PATH.len(), CONSOLE_INHERIT),
            Err(Errno::NotImplemented)
        );
    }

    /// A path naming no registered program fails closed with `NotFound` — the empty boot registry resolves nothing.
    #[test]
    fn spawn_unknown_path_is_not_found() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, SPAWN_PATH);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("caller registration");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let frames = spawn_test_frames();
        let caps = make_caps_record(2, &[CapabilityId::PROC_SPAWN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // Frames + a real producer wired, but the registry is empty, so no
        // path resolves and the producer is never reached.
        let producer: &'static RecordingSpawn = Box::leak(Box::new(RecordingSpawn::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_frames(&frames)
        .with_spawn(&EMPTY_PROGRAM_REGISTRY, producer);

        assert_eq!(
            h.spawn(&ctx, 0x1000, SPAWN_PATH.len(), CONSOLE_INHERIT),
            Err(Errno::NotFound)
        );
        assert!(producer.seen_rxe.lock().is_empty());
    }

    /// `spawn` from a caller with no registered address space fails closed
    /// with `BadAddress`, never leaking the missing-space case.
    #[test]
    fn spawn_unregistered_caller_is_bad_address() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let frames = spawn_test_frames();
        let caps = make_caps_record(2, &[CapabilityId::PROC_SPAWN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let producer: &'static RecordingSpawn = Box::leak(Box::new(RecordingSpawn::new()));
        let programs: &'static ProgramRegistry =
            Box::leak(Box::new(ProgramRegistry::new(Box::leak(Box::new([
                EmbeddedProgram {
                    path: SPAWN_PATH,
                    rxe: SPAWN_RXE,
                    caps: &[],
                    args: &[],
                },
            ])))));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_frames(&frames)
        .with_spawn(programs, producer);

        assert_eq!(
            h.spawn(&ctx, 0x1000, SPAWN_PATH.len(), CONSOLE_INHERIT),
            Err(Errno::BadAddress)
        );
        assert!(producer.seen_rxe.lock().is_empty());
    }

    /// A zero-length or over-long path cannot name a registered program,
    /// so it fails closed with `NotFound` without staging an unbounded
    /// allocation.
    #[test]
    fn spawn_empty_or_oversize_path_is_not_found() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let frames = spawn_test_frames();
        let caps = make_caps_record(2, &[CapabilityId::PROC_SPAWN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let producer: &'static RecordingSpawn = Box::leak(Box::new(RecordingSpawn::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_frames(&frames)
        .with_spawn(&EMPTY_PROGRAM_REGISTRY, producer);

        assert_eq!(
            h.spawn(&ctx, 0x1000, 0, CONSOLE_INHERIT),
            Err(Errno::NotFound)
        );
        assert_eq!(
            h.spawn(&ctx, 0x1000, SPAWN_PATH_MAX + 1, CONSOLE_INHERIT),
            Err(Errno::NotFound)
        );
        assert!(producer.seen_rxe.lock().is_empty());
    }

    /// `console_count` reports the installed list's length, and zero on
    /// the empty pre-install default — never an invented topology.
    #[test]
    fn console_count_reports_the_installed_list_length() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::CONSOLE_WRITE], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let bare = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(bare.console_count(&ctx), Ok(0));

        let video: &'static RecordingConsole = Box::leak(Box::new(RecordingConsole::new()));
        let uart: &'static RecordingConsole = Box::leak(Box::new(RecordingConsole::new()));
        let consoles: &'static [ConsoleDevice] = Box::leak(Box::new([
            ConsoleDevice::new(video, &crate::console::NULL_CONSOLE_READ),
            ConsoleDevice::new(uart, &crate::console::NULL_CONSOLE_READ),
        ]));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(consoles);
        assert_eq!(h.console_count(&ctx), Ok(2));
    }

    /// `stream_write` reaches exactly the console the caller's descriptor
    /// table names: a process attached to console 1 (the UART beside an
    /// active video console, `plans/PI.md` P11) writes the UART and never
    /// the display.
    #[test]
    fn stream_write_routes_to_the_descriptor_console() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let line = b"Username: ";
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, line);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let video: &'static RecordingConsole = Box::leak(Box::new(RecordingConsole::new()));
        let uart: &'static RecordingConsole = Box::leak(Box::new(RecordingConsole::new()));
        let consoles: &'static [ConsoleDevice] = Box::leak(Box::new([
            ConsoleDevice::new(video, &crate::console::NULL_CONSOLE_READ),
            ConsoleDevice::new(uart, &crate::console::NULL_CONSOLE_READ),
        ]));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(consoles);
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard_on(1));

        assert_eq!(
            h.stream_write(&ctx, STDOUT, 0x1000, line.len()),
            Ok(line.len() as u64)
        );
        assert_eq!(uart.written.lock().as_slice(), line);
        assert!(video.written.lock().is_empty());

        // A descriptor naming a console index with nothing installed at
        // it fails closed rather than falling back to another device.
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard_on(7));
        assert_eq!(
            h.stream_write(&ctx, STDOUT, 0x1000, line.len()),
            Err(Errno::NotImplemented)
        );
    }

    /// `stream_read` draws from exactly the console the caller's
    /// descriptor table names: the UART console's session reads the UART
    /// RX, never another console's input (`plans/PI.md` P11 — the UART
    /// no longer feeds the video login).
    #[test]
    fn stream_read_routes_to_the_descriptor_console() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let keyboard: &'static RecordingConsoleRead =
            Box::leak(Box::new(RecordingConsoleRead::new(b"")));
        let uart_rx: &'static RecordingConsoleRead =
            Box::leak(Box::new(RecordingConsoleRead::new(b"root\n")));
        let consoles: &'static [ConsoleDevice] = Box::leak(Box::new([
            ConsoleDevice::new(&crate::console::NULL_CONSOLE, keyboard),
            ConsoleDevice::new(&crate::console::NULL_CONSOLE, uart_rx),
        ]));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(consoles);
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard_on(1));

        assert_eq!(h.stream_read(&ctx, STDIN, 0x1000, 16), Ok(5));
        // The UART console's input was drained; the video console's
        // keyboard source was never touched.
        assert!(uart_rx.last_buf_len.lock().is_some());
        assert_eq!(*keyboard.last_buf_len.lock(), None);
    }

    /// With echo on (the default), `stream_read` echoes the consumed
    /// bytes back to the *same* console's write half so an interactive
    /// user sees what they type (terminal local echo),
    /// translating the Return key's CR into CR-LF.
    #[test]
    fn stream_read_echoes_consumed_bytes_to_the_console_write_half() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let echo: &'static RecordingConsole = Box::leak(Box::new(RecordingConsole::new()));
        let rx: &'static RecordingConsoleRead =
            Box::leak(Box::new(RecordingConsoleRead::new(b"hi\r")));
        let consoles: &'static [ConsoleDevice] =
            Box::leak(Box::new([ConsoleDevice::new(echo, rx)]));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(consoles);
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());

        assert_eq!(h.stream_read(&ctx, STDIN, 0x1000, 16), Ok(3));
        // The consumed bytes were echoed back, with the CR rendered as
        // CR-LF.
        assert_eq!(echo.written.lock().as_slice(), b"hi\r\n");
    }

    /// `stream_echo` disabling echo on the read descriptor's console
    /// stops a subsequent `stream_read` from echoing (the password-read
    /// contract — never render a credential).
    #[test]
    fn stream_echo_disables_console_echo_for_the_following_read() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let echo: &'static RecordingConsole = Box::leak(Box::new(RecordingConsole::new()));
        let rx: &'static RecordingConsoleRead =
            Box::leak(Box::new(RecordingConsoleRead::new(b"secret")));
        let consoles: &'static [ConsoleDevice] =
            Box::leak(Box::new([ConsoleDevice::new(echo, rx)]));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_consoles(consoles);
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard());

        // Disable echo on the input descriptor, then read: nothing is
        // echoed back.
        assert_eq!(h.stream_echo(&ctx, STDIN, 0), Ok(0));
        assert_eq!(h.stream_read(&ctx, STDIN, 0x1000, 16), Ok(6));
        assert!(echo.written.lock().is_empty());

        // A `stream_echo` on a descriptor that is not a readable stream
        // fails closed rather than toggling another console.
        assert_eq!(h.stream_echo(&ctx, STDOUT, 1), Err(Errno::NotFound));
    }

    /// Build a writable user address space at `0x1000` seeded with one
    /// encoded [`KeyInput`] record, registered against task 2, plus the
    /// caller context. The mapping is `READ|WRITE` so the same buffer can
    /// be read by `key_inject` and written by `keyboard_read`.
    fn key_inject_aspace(aspaces: &RwLock<AddressSpaceRegistry>, record: KeyInput) {
        let bytes = record.to_le_bytes();
        let (space, physmap) =
            send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &bytes);
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
    }

    fn press_char(c: char) -> KeyInput {
        KeyInput::Pressed {
            key: KeyValue::Char(c),
            modifiers: Modifiers::default(),
        }
    }

    /// `key_inject` in the default text focus encodes a key *press* to the
    /// console (tty) bytes through the shared `lib/keymap` map and enqueues
    /// them on the arbiter's text sink, which is the same queue a
    /// `stream_read` from the login then drains (`plans/PI.md` P11 —
    /// keyboard input for the video console).
    #[test]
    fn key_inject_text_focus_encodes_a_press_to_the_text_sink() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        key_inject_aspace(&aspaces, press_char('a'));
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // The arbiter's text sink is the video console's input queue.
        let queue: &'static crate::console::ConsoleInputQueue =
            Box::leak(Box::new(crate::console::ConsoleInputQueue::new()));
        let focus: &'static InputFocus = Box::leak(Box::new(InputFocus::new(queue)));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_input_focus(focus);

        assert_eq!(
            h.key_inject(&ctx, 0x1000, KeyInput::WIRE_LEN),
            Ok(KeyInput::WIRE_LEN as u64)
        );
        // The encoded byte is now drainable from the text sink.
        let mut buf = [0u8; 8];
        assert_eq!(crate::console::ConsoleRead::read(queue, &mut buf), Ok(1));
        assert_eq!(&buf[..1], b"a");
    }

    /// The first successful `key_inject` emits exactly one
    /// `AuditEvent::InputDelivered` (`EventId(4050)`) — the witness that an
    /// (autoloaded) keyboard driver is delivering input — and a second
    /// inject emits no further witness, so the log never carries a
    /// per-keystroke record (`plans/PI.md` P11). The
    /// record carries **no** fields, so a typed character never reaches the
    /// log (secret hygiene).
    #[test]
    fn key_inject_witnesses_first_delivery_exactly_once_with_no_content() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        key_inject_aspace(&aspaces, press_char('a'));
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let queue: &'static crate::console::ConsoleInputQueue =
            Box::leak(Box::new(crate::console::ConsoleInputQueue::new()));
        let focus: &'static InputFocus = Box::leak(Box::new(InputFocus::new(queue)));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_input_focus(focus);

        // Drop the fixture-setup records (e.g. `TaskCapabilitiesDerived`) so
        // the assertion sees only the witness.
        sink.clear();

        let id = AuditEvent::InputDelivered.id().0;
        assert_eq!(
            h.key_inject(&ctx, 0x1000, KeyInput::WIRE_LEN),
            Ok(KeyInput::WIRE_LEN as u64)
        );
        let snapshot = sink.snapshot();
        assert_eq!(
            snapshot.iter().filter(|e| e.id.0 == id).count(),
            1,
            "first inject emits exactly one witness"
        );
        // The witness carries no key content: a typed secret never reaches
        // the log.
        let witness = snapshot
            .iter()
            .find(|e| e.id.0 == id)
            .expect("witness present");
        assert!(witness.fields.is_empty(), "the witness carries no fields");

        // A second successful inject emits no further witness — never one
        // per keystroke.
        assert_eq!(
            h.key_inject(&ctx, 0x1000, KeyInput::WIRE_LEN),
            Ok(KeyInput::WIRE_LEN as u64)
        );
        assert_eq!(
            sink.event_ids().iter().filter(|&&i| i == id).count(),
            1,
            "no further witness on later injects"
        );
    }

    /// `key_inject` fails closed when no arbiter is wired: the default
    /// `NULL_INPUT_FOCUS` text sink is `NULL_CONSOLE_INPUT`, so a press
    /// that would be enqueued there surfaces `NotImplemented` rather than
    /// dropping it. A `len` too small to hold a
    /// record fails closed before any state is touched.
    #[test]
    fn key_inject_without_arbiter_fails_closed() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        key_inject_aspace(&aspaces, press_char('a'));
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );

        // A short buffer is refused before the arbiter is consulted.
        assert_eq!(
            h.key_inject(&ctx, 0x1000, KeyInput::WIRE_LEN - 1),
            Err(Errno::BufferTooSmall)
        );
        // The NULL text sink accepts no injected input.
        assert_eq!(
            h.key_inject(&ctx, 0x1000, KeyInput::WIRE_LEN),
            Err(Errno::NotImplemented)
        );
    }

    /// `display_acquire` switches the arbiter's foreground to the desktop
    /// keyboard channel, so an injected record is delivered whole to
    /// `keyboard_read`; `display_release` returns focus to the text
    /// console (`plans/PI.md` P11 — input follows the surface owner).
    #[test]
    fn display_acquire_routes_records_to_keyboard_read() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let record = press_char('z');
        key_inject_aspace(&aspaces, record);
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let queue: &'static crate::console::ConsoleInputQueue =
            Box::leak(Box::new(crate::console::ConsoleInputQueue::new()));
        let focus: &'static InputFocus = Box::leak(Box::new(InputFocus::new(queue)));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_input_focus(focus);

        // A short read buffer is refused before the channel is touched.
        assert_eq!(
            h.keyboard_read(&ctx, 0x1000, KeyInput::WIRE_LEN - 1),
            Err(Errno::BufferTooSmall)
        );

        assert_eq!(h.display_acquire(&ctx), Ok(0));
        assert_eq!(
            h.key_inject(&ctx, 0x1000, KeyInput::WIRE_LEN),
            Ok(KeyInput::WIRE_LEN as u64)
        );
        // The record routed to the desktop channel, not the text sink.
        let mut text = [0u8; 4];
        assert_eq!(crate::console::ConsoleRead::read(queue, &mut text), Ok(0));
        // `keyboard_read` writes the whole record back into the buffer.
        assert_eq!(
            h.keyboard_read(&ctx, 0x1000, KeyInput::WIRE_LEN),
            Ok(KeyInput::WIRE_LEN as u64)
        );
        // The channel is now drained.
        assert_eq!(h.keyboard_read(&ctx, 0x1000, KeyInput::WIRE_LEN), Ok(0));

        // Releasing returns focus to the text console: the next press
        // routes to the text sink (the buffer still holds the 'z' record).
        assert_eq!(h.display_release(&ctx), Ok(0));
        assert_eq!(
            h.key_inject(&ctx, 0x1000, KeyInput::WIRE_LEN),
            Ok(KeyInput::WIRE_LEN as u64)
        );
        let mut text = [0u8; 4];
        assert_eq!(crate::console::ConsoleRead::read(queue, &mut text), Ok(1));
        assert_eq!(&text[..1], b"z");
    }

    /// A `spawn` naming an installed console attaches the child's
    /// standard streams to exactly that console (`plans/PI.md` P11 — one
    /// login per console), and an index with no installed console fails
    /// closed with `NotFound`.
    #[test]
    fn spawn_attaches_the_child_to_the_selected_console() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, SPAWN_PATH);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("caller registration");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let frames = spawn_test_frames();
        let caps = make_caps_record(2, &[CapabilityId::PROC_SPAWN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let programs: &'static ProgramRegistry =
            Box::leak(Box::new(ProgramRegistry::new(Box::leak(Box::new([
                EmbeddedProgram {
                    path: SPAWN_PATH,
                    rxe: SPAWN_RXE,
                    caps: &[],
                    args: &[],
                },
            ])))));
        let producer: &'static RecordingSpawn = Box::leak(Box::new(RecordingSpawn::new()));
        let console: &'static RecordingConsole = Box::leak(Box::new(RecordingConsole::new()));
        let consoles: &'static [ConsoleDevice] = Box::leak(Box::new([
            ConsoleDevice::new(console, &crate::console::NULL_CONSOLE_READ),
            ConsoleDevice::new(console, &crate::console::NULL_CONSOLE_READ),
        ]));

        let h = spawn_handler(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng, &frames, programs,
            producer,
        )
        .with_consoles(consoles);

        // An explicit, installed console: the child's table is the
        // standard shape on exactly that console.
        let pid = h
            .spawn(&ctx, 0x1000, SPAWN_PATH.len(), 1)
            .expect("spawn succeeds");
        assert_eq!(
            aspaces.read().streams(SecTaskId(pid)),
            DescriptorTable::standard_on(1)
        );

        // An index with no installed console fails closed before any
        // state is touched.
        assert_eq!(
            h.spawn(&ctx, 0x1000, SPAWN_PATH.len(), 2),
            Err(Errno::NotFound)
        );
        assert_eq!(
            h.spawn(&ctx, 0x1000, SPAWN_PATH.len(), u64::from(u32::MAX)),
            Err(Errno::NotFound)
        );
    }

    /// `CONSOLE_INHERIT` copies the caller's own descriptor table into
    /// the child (login's shell stays on login's
    /// console).
    #[test]
    fn spawn_inherit_copies_the_callers_table() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, SPAWN_PATH);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("caller registration");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let frames = spawn_test_frames();
        let caps = make_caps_record(2, &[CapabilityId::PROC_SPAWN], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let programs: &'static ProgramRegistry =
            Box::leak(Box::new(ProgramRegistry::new(Box::leak(Box::new([
                EmbeddedProgram {
                    path: SPAWN_PATH,
                    rxe: SPAWN_RXE,
                    caps: &[],
                    args: &[],
                },
            ])))));
        let producer: &'static RecordingSpawn = Box::leak(Box::new(RecordingSpawn::new()));
        let console: &'static RecordingConsole = Box::leak(Box::new(RecordingConsole::new()));
        let consoles: &'static [ConsoleDevice] = Box::leak(Box::new([
            ConsoleDevice::new(console, &crate::console::NULL_CONSOLE_READ),
            ConsoleDevice::new(console, &crate::console::NULL_CONSOLE_READ),
        ]));

        let h = spawn_handler(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng, &frames, programs,
            producer,
        )
        .with_consoles(consoles);

        // The caller sits on console 1; its child inherits that table
        // verbatim.
        aspaces
            .write()
            .set_streams(SecTaskId(2), DescriptorTable::standard_on(1));
        let pid = h
            .spawn(&ctx, 0x1000, SPAWN_PATH.len(), CONSOLE_INHERIT)
            .expect("spawn succeeds");
        assert_eq!(
            aspaces.read().streams(SecTaskId(pid)),
            DescriptorTable::standard_on(1)
        );
    }

    /// The dispatcher refuses `spawn` from a caller without
    /// `CAP_PROC_SPAWN` before the handler is reached (step 2): the producer is never invoked.
    #[test]
    fn spawn_without_capability_is_denied_by_dispatcher() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, SPAWN_PATH);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("caller registration");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let frames = spawn_test_frames();
        // No CAP_PROC_SPAWN in the caller's effective set.
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let producer: &'static RecordingSpawn = Box::leak(Box::new(RecordingSpawn::new()));
        let programs: &'static ProgramRegistry =
            Box::leak(Box::new(ProgramRegistry::new(Box::leak(Box::new([
                EmbeddedProgram {
                    path: SPAWN_PATH,
                    rxe: SPAWN_RXE,
                    caps: &[],
                    args: &[],
                },
            ])))));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_frames(&frames)
        .with_spawn(programs, producer);

        let mut args = RawArgs::ZERO;
        args.0[0] = 0x1000;
        args.0[1] = SPAWN_PATH.len() as u64;
        let d = Dispatcher::new(&h, sink);
        assert_eq!(
            d.dispatch(&ctx, SyscallNumber::SPAWN.as_u16(), args),
            Err(Errno::PermissionDenied)
        );
        assert!(producer.seen_rxe.lock().is_empty());
    }

    /// A `MemMap` producer that records the last `(len, flags, addr_hint)`
    /// it was handed and returns a fabricated base, so the handler tests
    /// can assert the arguments reached it without a real `kernel/mem`
    /// live-mapping path.
    struct RecordingMemMap {
        last_map: rustos_sync::SpinLock<Option<(usize, u32, u64)>>,
        last_unmap: rustos_sync::SpinLock<Option<(u64, usize)>>,
    }
    impl RecordingMemMap {
        fn new() -> Self {
            Self {
                last_map: rustos_sync::SpinLock::new(None),
                last_unmap: rustos_sync::SpinLock::new(None),
            }
        }
    }
    impl crate::memmap::MemMap for RecordingMemMap {
        fn map(
            &self,
            len: usize,
            flags: rustos_abi::MapFlags,
            addr_hint: u64,
        ) -> Result<u64, Errno> {
            *self.last_map.lock() = Some((len, flags.bits(), addr_hint));
            // Echo a fabricated base derived from the request so the test
            // can confirm the handler returned the producer's value verbatim.
            Ok(0x5000_0000 | addr_hint)
        }
        fn unmap(&self, base: u64, len: usize) -> Result<(), Errno> {
            *self.last_unmap.lock() = Some((base, len));
            Ok(())
        }
    }

    /// `mem_map` needs no capability and forwards the
    /// decoded `(len, flags, addr_hint)` to the installed producer,
    /// returning its base verbatim.
    #[test]
    fn mem_map_forwards_to_installed_producer() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let producer: &'static RecordingMemMap = Box::leak(Box::new(RecordingMemMap::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_mem_map(producer);

        let flags = rustos_abi::MapFlags::FIXED;
        assert_eq!(
            h.mem_map(&ctx, 0x2000, flags, 0x10_0000),
            Ok(0x5000_0000 | 0x10_0000)
        );
        assert_eq!(
            *producer.last_map.lock(),
            Some((0x2000, flags.bits(), 0x10_0000))
        );
    }

    /// With no producer installed the handler holds `NULL_MEM_MAP` and
    /// fails closed with `NotImplemented`.
    #[test]
    fn mem_map_without_producer_is_not_implemented() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.mem_map(&ctx, 0x1000, rustos_abi::MapFlags::empty(), 0),
            Err(Errno::NotImplemented)
        );
    }

    /// A zero-length `mem_map` is rejected before the producer is reached: an empty mapping is meaningless.
    #[test]
    fn mem_map_zero_length_is_length_out_of_range() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let producer: &'static RecordingMemMap = Box::leak(Box::new(RecordingMemMap::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_mem_map(producer);

        assert_eq!(
            h.mem_map(&ctx, 0, rustos_abi::MapFlags::empty(), 0),
            Err(Errno::LengthOutOfRange)
        );
        // The producer was never reached: the zero-length guard fails
        // closed before any state is touched.
        assert!(producer.last_map.lock().is_none());
    }

    /// The `AddressSpaceBytes` ulimit is actually enforced on the `mem_map`
    /// path: a request whose page-rounded size would push the task's live
    /// total past its soft ceiling is refused *before* the producer is
    /// reached (fail closed), an admitted map is charged, and a `mem_unmap`
    /// credits the freed bytes back so a later map fits again. Without the
    /// enforcement the limit was settable but silently ignored (fail open).
    #[test]
    fn mem_map_enforces_the_address_space_limit_and_fails_closed() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // Impose a 2-page (0x2000-byte) address-space ceiling on the caller.
        aspaces.write().set_limit(
            SecTaskId(2),
            LimitKind::AddressSpaceBytes,
            ResourceLimit::new(0x2000, u64::MAX).expect("well-formed"),
        );

        let producer: &'static RecordingMemMap = Box::leak(Box::new(RecordingMemMap::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_mem_map(producer);

        // A request exactly at the ceiling is admitted and charged.
        let base = h
            .mem_map(&ctx, 0x2000, rustos_abi::MapFlags::empty(), 0)
            .expect("a map at the ceiling succeeds");
        assert_eq!(aspaces.read().mapped_anon_bytes(SecTaskId(2)), 0x2000);

        // A further page would exceed the ceiling: denied, fail closed, and
        // the producer is never reached for the rejected request.
        *producer.last_map.lock() = None;
        assert_eq!(
            h.mem_map(&ctx, 0x1000, rustos_abi::MapFlags::empty(), 0),
            Err(Errno::OutOfRange)
        );
        assert!(producer.last_map.lock().is_none());
        // The denied request changed no accounting.
        assert_eq!(aspaces.read().mapped_anon_bytes(SecTaskId(2)), 0x2000);

        // Freeing the mapped region credits the bytes back, so a fresh map
        // of the same size fits under the ceiling again.
        assert_eq!(h.mem_unmap(&ctx, base, 0x2000), Ok(0));
        assert_eq!(aspaces.read().mapped_anon_bytes(SecTaskId(2)), 0);
        assert!(h
            .mem_map(&ctx, 0x2000, rustos_abi::MapFlags::empty(), 0)
            .is_ok());
        assert_eq!(aspaces.read().mapped_anon_bytes(SecTaskId(2)), 0x2000);
    }

    /// A page-rounded request: a sub-page `len` is charged as a whole page,
    /// the same figure `mem_unmap` later credits, so accounting stays
    /// consistent and a single byte still consumes one page of the ceiling.
    #[test]
    fn mem_map_charges_whole_pages_against_the_limit() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // A one-page ceiling.
        aspaces.write().set_limit(
            SecTaskId(2),
            LimitKind::AddressSpaceBytes,
            ResourceLimit::new(0x1000, u64::MAX).expect("well-formed"),
        );

        let producer: &'static RecordingMemMap = Box::leak(Box::new(RecordingMemMap::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_mem_map(producer);

        // One byte rounds up to a whole page and just fits the one-page
        // ceiling; it is charged as a full page, not a single byte.
        assert!(h.mem_map(&ctx, 1, rustos_abi::MapFlags::empty(), 0).is_ok());
        assert_eq!(aspaces.read().mapped_anon_bytes(SecTaskId(2)), 0x1000);
        // A second single byte would need a second page: denied.
        assert_eq!(
            h.mem_map(&ctx, 1, rustos_abi::MapFlags::empty(), 0),
            Err(Errno::OutOfRange)
        );
    }

    /// A minimal published live space whose [`LiveUserSpace::freeze`] returns
    /// a snapshot of an inner [`AddressSpace`] — standing in for the live
    /// space a real `mem_map` would have grown. Only `freeze` is exercised;
    /// the mutating methods are unreachable in this test (the producer is the
    /// fake `RecordingMemMap`), so they fail closed.
    struct PublishedLive {
        space: AddressSpace<HostPageTable>,
    }

    impl LiveUserSpace for PublishedLive {
        fn map_anonymous(&mut self, _base: u64, _pages: u64) -> Result<u64, LiveSpaceError> {
            Err(LiveSpaceError::Anon(AnonError::OutOfMemory))
        }
        fn map_anonymous_placed(&mut self, _pages: u64) -> Result<u64, LiveSpaceError> {
            Err(LiveSpaceError::Anon(AnonError::OutOfMemory))
        }
        fn unmap_anonymous(&mut self, _base: u64, _pages: u64) -> Result<(), LiveSpaceError> {
            Err(LiveSpaceError::Anon(AnonError::NotMapped))
        }
        fn map_device_window(&mut self, _phys: u64, _len: usize) -> Result<u64, LiveSpaceError> {
            Err(LiveSpaceError::Anon(AnonError::OutOfMemory))
        }
        fn alloc_dma(&mut self, _len: usize, _limit: u64) -> Result<DmaMapping, LiveSpaceError> {
            Err(LiveSpaceError::Anon(AnonError::OutOfMemory))
        }
        fn free_dma(&mut self, _cpu_va: u64) -> Result<(), LiveSpaceError> {
            Err(LiveSpaceError::Dma(DmaError::UnknownBuffer))
        }
        fn map_shared(&mut self, _phys: u64, _len: usize) -> Result<u64, LiveSpaceError> {
            Err(LiveSpaceError::Anon(AnonError::OutOfMemory))
        }
        fn unmap_shared(&mut self, _base: u64, _len: usize) -> Result<(), LiveSpaceError> {
            Err(LiveSpaceError::Anon(AnonError::NotMapped))
        }
        fn freeze(&self) -> FrozenAddressSpace {
            self.space.freeze()
        }
    }

    /// The metal regression for the login `spawn err=18` (`BadAddress`):
    /// the registry's frozen snapshot is taken at spawn and was never
    /// refreshed when `mem_map` grew the live space, so `copy_in` of a
    /// heap-allocated pointer (the shell path `login` passed to `spawn`)
    /// found it unmapped. After a successful `mem_map` the handler must
    /// re-freeze the caller's live space into the registry, so the next
    /// `with_caller_aspace` copy sees the new region.
    #[test]
    fn mem_map_refreezes_the_caller_snapshot_so_a_new_region_is_reachable() {
        install_trace_filter();
        let sink = make_sink();
        // `with_cpus(1)` reports current CPU 0 — the slot the live space is
        // published on. No other `kernel/core` test publishes on CPU 0
        // (`live_producer` uses CPUs ≥ 1), so the global slot is unshared
        // here (no flaky tests).
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // The page the "grown heap" lives at — absent from the spawn-time
        // snapshot, present in the live space.
        let heap = Page::from_addr(VirtAddr::new(0x4000)).expect("aligned");

        // Register the spawn-time snapshot: an empty space (no heap page).
        {
            let spawn_time = AddressSpace::new(HostPageTable::new());
            let physmap = SimPhysMap::new(PhysAddr::new(0), PAGE_SIZE);
            aspaces
                .write()
                .register(
                    SecTaskId(2),
                    Box::new(spawn_time.freeze()),
                    Box::new(physmap),
                )
                .expect("register spawn-time snapshot");
        }
        // The stale snapshot cannot see the heap page — the faulting state.
        assert!(aspaces
            .read()
            .resolve(SecTaskId(2))
            .expect("registered")
            .0
            .translate(heap)
            .is_none());

        // Publish a live space that maps the heap page (what the producer
        // would have mapped). `mem_map`'s fake producer returns `Ok`, which
        // must trigger the re-freeze.
        let mut live_space = AddressSpace::new(HostPageTable::new());
        live_space
            .map(heap, Frame(9), MapFlags::READ | MapFlags::USER)
            .expect("map heap page");
        let live: &'static mut PublishedLive =
            Box::leak(Box::new(PublishedLive { space: live_space }));
        let _guard = crate::kthread::publish_live_space_for_test(0, live);

        let producer: &'static RecordingMemMap = Box::leak(Box::new(RecordingMemMap::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_mem_map(producer);

        h.mem_map(&ctx, 0x1000, rustos_abi::MapFlags::FIXED, 0x4000)
            .expect("mem_map succeeds");

        // The re-freeze published the live space's mappings: the heap page
        // now resolves through the registry, so a subsequent `copy_in`
        // (e.g. `spawn`'s path argument) would reach it.
        let reg = aspaces.read();
        let (space, _) = reg.resolve(SecTaskId(2)).expect("still registered");
        let (frame, flags) = space.translate(heap).expect("heap page now visible");
        assert_eq!(frame, Frame(9));
        assert!(flags.contains(MapFlags::USER));
    }

    /// `mem_unmap` forwards `(base, len)` to the producer and reports
    /// `Ok(0)` (the `Errno`-return ABI shape) on success; a zero-length
    /// range and the no-producer build both fail closed.
    #[test]
    fn mem_unmap_forwards_and_fails_closed() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // No producer → NotImplemented.
        let bare = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            bare.mem_unmap(&ctx, 0x10_0000, 0x1000),
            Err(Errno::NotImplemented)
        );

        let producer: &'static RecordingMemMap = Box::leak(Box::new(RecordingMemMap::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_mem_map(producer);

        // Zero-length range rejected before the producer is reached.
        assert_eq!(
            h.mem_unmap(&ctx, 0x10_0000, 0),
            Err(Errno::LengthOutOfRange)
        );
        assert!(producer.last_unmap.lock().is_none());

        // A well-formed range reaches the producer and reports Ok(0).
        assert_eq!(h.mem_unmap(&ctx, 0x10_0000, 0x1000), Ok(0));
        assert_eq!(*producer.last_unmap.lock(), Some((0x10_0000, 0x1000)));
    }

    /// An MMIO-map facility that records the `(phys_base, len)` it was
    /// handed and returns a fabricated base, so the handler tests can
    /// assert the validated window reached the mechanism without a real
    /// `kernel/mem` mapping path.
    struct RecordingMmioFacility {
        last: rustos_sync::SpinLock<Option<(u64, usize)>>,
        ret: Result<u64, Errno>,
    }
    impl crate::devres::MmioMapFacility for RecordingMmioFacility {
        fn map_window(&self, phys_base: u64, len: usize) -> Result<u64, Errno> {
            *self.last.lock() = Some((phys_base, len));
            self.ret
        }
    }

    /// Build the shared handler test scaffolding (scheduler, registries,
    /// caller context for `SecTaskId(2)`), returning everything the
    /// `mmio_map` tests borrow. Keeping it inline per-test mirrors the
    /// other handler tests; this helper exists only because the five
    /// `mmio_map` tests share the exact same scaffold.
    #[allow(clippy::type_complexity)]
    fn mmio_scaffold() -> (
        Arc<TestArch>,
        RwLock<CapTable>,
        RwLock<PortRegistry>,
        RwLock<AddressSpaceRegistry>,
        RwLock<Box<dyn RandomReserve + Send + Sync>>,
        IrqTable,
    ) {
        install_trace_filter();
        let arch = Arc::new(TestArch::with_cpus(1));
        (
            arch,
            RwLock::new(CapTable::new()),
            RwLock::new(PortRegistry::new()),
            RwLock::new(AddressSpaceRegistry::new()),
            unseeded_rng(),
            IrqTable::new(31),
        )
    }

    /// With no grant minted for the caller the per-task grant table resolves
    /// nothing, so any handle fails closed with `NotFound` — a driver can
    /// never map an ungranted region.
    #[test]
    fn mmio_map_without_grant_is_not_found() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.mmio_map(&ctx, 7, 0, 0x10), Err(Errno::NotFound));
    }

    /// The grant is owner-bound: a handle minted for another task, or an
    /// unknown handle value, resolves to nothing and is refused
    /// (no trusted-caller shortcut; handle forgery is
    /// rejected exactly as `irq_wait` re-checks its binding).
    #[test]
    fn mmio_map_forged_or_foreign_handle_is_not_found() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);

        // Mint one grant for task 2, capturing its kernel-issued handle.
        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::mmio(0xFE98_0000, 0x4000),
        );
        let facility: &'static RecordingMmioFacility = Box::leak(Box::new(RecordingMmioFacility {
            last: rustos_sync::SpinLock::new(None),
            ret: Ok(0x9000_0000),
        }));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_mmio_map_facility(facility);

        // Right owner, wrong handle → NotFound.
        let owner = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        assert_eq!(
            h.mmio_map(&owner, handle + 1, 0, 0x10),
            Err(Errno::NotFound)
        );

        // Right handle value, wrong (foreign) task → NotFound: a driver
        // cannot reach another driver's window by reusing its handle.
        let foreign = CallerContext {
            task_id: SecTaskId(3),
            caps: &caps,
        };
        assert_eq!(h.mmio_map(&foreign, handle, 0, 0x10), Err(Errno::NotFound));

        // Neither refusal touched the mapping mechanism.
        assert!(facility.last.lock().is_none());
    }

    /// The success path: the owner's handle resolves to its granted MMIO
    /// window, the validated `(phys_base, len)` reaches the facility, and
    /// the facility's mapped base flows back verbatim — only the granted
    /// region, nothing else.
    #[test]
    fn mmio_map_maps_granted_window_through_facility() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::mmio(0xFE98_0000, 0x4000),
        );
        let facility: &'static RecordingMmioFacility = Box::leak(Box::new(RecordingMmioFacility {
            last: rustos_sync::SpinLock::new(None),
            ret: Ok(0x9000_0000),
        }));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_mmio_map_facility(facility);

        assert_eq!(h.mmio_map(&ctx, handle, 0, 0x4000), Ok(0x9000_0000));
        // Exactly the requested sub-region reached the mechanism — its base
        // is the grant base plus the offset, and its length the request,
        // never a caller-supplied physical address (here offset 0, full
        // length: the whole granted window).
        assert_eq!(*facility.last.lock(), Some((0xFE98_0000, 0x4000)));
    }

    /// A valid, owned grant whose mechanism is unwired holds
    /// `NULL_MMIO_MAP_FACILITY` and fails closed with `NotImplemented` — proving the lookup + validation passed and the
    /// missing producer denies rather than fabricating a mapping.
    #[test]
    fn mmio_map_with_grant_but_no_facility_is_not_implemented() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::mmio(0xFE98_0000, 0x4000),
        );
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );

        assert_eq!(
            h.mmio_map(&ctx, handle, 0, 0x4000),
            Err(Errno::NotImplemented)
        );
    }

    /// A grant of a non-window kind (a DMA constraint) is refused with
    /// `OutOfRange` before the mapping mechanism is reached: `mmio_map`
    /// maps memory windows, not every resource a node may request
    /// (validate every input).
    #[test]
    fn mmio_map_non_window_grant_is_out_of_range() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::dma(0x4000_0000, 0),
        );
        let facility: &'static RecordingMmioFacility = Box::leak(Box::new(RecordingMmioFacility {
            last: rustos_sync::SpinLock::new(None),
            ret: Ok(0x9000_0000),
        }));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_mmio_map_facility(facility);

        assert_eq!(h.mmio_map(&ctx, handle, 0, 0x10), Err(Errno::OutOfRange));
        // The wrong-kind grant was refused before the mechanism ran.
        assert!(facility.last.lock().is_none());
    }

    /// Regression: a driver granted a large outbound bus
    /// window maps only the small `[offset, offset + len)` sub-region it
    /// names (e.g. one enumerated BAR), never the whole 1 GiB aperture. The
    /// mechanism receives the sub-region's absolute base (`grant base +
    /// offset`) and the request length — not the grant's full extent — so the
    /// map cannot exhaust the per-task MMIO virtual window and fail closed
    /// with `OutOfMemory` (the keyboard-chain defect this fix closes). A
    /// sub-region escaping the grant is refused with `OutOfRange`, before the
    /// mechanism runs.
    #[test]
    fn mmio_map_maps_only_the_requested_sub_region_of_a_bus_window() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // A 1 GiB outbound bus window (the BCM2711 PCIe geometry): CPU base
        // 0x6_0000_0000, PCIe-space base 0xC000_0000.
        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::bus_window(0x6_0000_0000, 0x4000_0000, 0xC000_0000),
        );
        let facility: &'static RecordingMmioFacility = Box::leak(Box::new(RecordingMmioFacility {
            last: rustos_sync::SpinLock::new(None),
            ret: Ok(0x9000_0000),
        }));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_mmio_map_facility(facility);

        // A 4 KiB BAR 0x3D50_0000 into the window maps exactly 4 KiB at the
        // absolute base `0x6_0000_0000 + 0x3D50_0000` — never the 1 GiB grant.
        assert_eq!(
            h.mmio_map(&ctx, handle, 0x3D50_0000, 0x1000),
            Ok(0x9000_0000)
        );
        assert_eq!(*facility.last.lock(), Some((0x6_3D50_0000, 0x1000)));

        // A sub-region running past the grant's end is refused before the
        // mechanism (fail closed).
        *facility.last.lock() = None;
        assert_eq!(
            h.mmio_map(&ctx, handle, 0x3FFF_F000, 0x2000),
            Err(Errno::OutOfRange)
        );
        assert!(facility.last.lock().is_none());
    }

    /// A DMA-alloc facility that records the `(len, addr_limit)` it was
    /// handed and returns a configured carve, so the `dma_alloc` handler
    /// tests can assert the validated request reached the mechanism without
    /// a real `kernel/mem` carve path.
    struct RecordingDmaFacility {
        last: rustos_sync::SpinLock<Option<(usize, u64)>>,
        freed: rustos_sync::SpinLock<Option<u64>>,
        ret: Result<crate::devres::DmaCarve, Errno>,
    }
    impl crate::devres::DmaAllocFacility for RecordingDmaFacility {
        fn alloc(&self, len: usize, addr_limit: u64) -> Result<crate::devres::DmaCarve, Errno> {
            *self.last.lock() = Some((len, addr_limit));
            self.ret
        }
        fn free(&self, cpu_va: u64) -> Result<(), Errno> {
            *self.freed.lock() = Some(cpu_va);
            Ok(())
        }
    }

    /// With no grant minted for the caller, `dma_alloc` resolves nothing and
    /// fails closed with `NotFound` — a driver can never carve against an
    /// ungranted constraint.
    #[test]
    fn dma_alloc_without_grant_is_not_found() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.dma_alloc(&ctx, 7, 0x1000, 0x1234), Err(Errno::NotFound));
    }

    /// The DMA grant is owner-bound: a handle minted for another task, or an
    /// unknown handle value, resolves to nothing and is refused
    /// (handle forgery rejected as in `mmio_map`).
    #[test]
    fn dma_alloc_forged_or_foreign_handle_is_not_found() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);

        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::dma(0x4000_0000, 0x1_0000),
        );
        let facility: &'static RecordingDmaFacility = Box::leak(Box::new(RecordingDmaFacility {
            last: rustos_sync::SpinLock::new(None),
            freed: rustos_sync::SpinLock::new(None),
            ret: Ok(crate::devres::DmaCarve {
                cpu_va: 0xD000_0000,
                device_addr: 0x4000_0000,
            }),
        }));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_dma_alloc_facility(facility);

        // Right owner, wrong handle → NotFound.
        let owner = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        assert_eq!(
            h.dma_alloc(&owner, handle + 1, 0x1000, 0x1234),
            Err(Errno::NotFound)
        );
        // Right handle value, wrong (foreign) task → NotFound.
        let foreign = CallerContext {
            task_id: SecTaskId(3),
            caps: &caps,
        };
        assert_eq!(
            h.dma_alloc(&foreign, handle, 0x1000, 0x1234),
            Err(Errno::NotFound)
        );
        // Neither refusal touched the carve mechanism.
        assert!(facility.last.lock().is_none());
    }

    /// A grant of a non-DMA kind (an MMIO window) is refused with
    /// `OutOfRange` before the carve mechanism is reached: `dma_alloc`
    /// carves against DMA constraints only.
    #[test]
    fn dma_alloc_non_dma_grant_is_out_of_range() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::mmio(0xFE98_0000, 0x4000),
        );
        let facility: &'static RecordingDmaFacility = Box::leak(Box::new(RecordingDmaFacility {
            last: rustos_sync::SpinLock::new(None),
            freed: rustos_sync::SpinLock::new(None),
            ret: Ok(crate::devres::DmaCarve {
                cpu_va: 0xD000_0000,
                device_addr: 0x4000_0000,
            }),
        }));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_dma_alloc_facility(facility);

        assert_eq!(
            h.dma_alloc(&ctx, handle, 0x1000, 0x1234),
            Err(Errno::OutOfRange)
        );
        assert!(facility.last.lock().is_none());
    }

    /// A translating inbound-viewport DMA grant (`dma_translated`, e.g. the
    /// Pi 4's `IB MEM 0x0..0x1ffffffff -> 0x4_0000_0000`) now reaches the
    /// carve mechanism: it is no longer rejected pre-carve. The carve runs
    /// bounded by the grant's CPU-side `addr_limit` (never a caller-supplied
    /// bound); with no caller address space registered the
    /// device-address copy-out then fails closed with `BadAddress` (the same
    /// fault `wait`'s copy-out produces). The translation arithmetic
    /// itself is unit-tested directly on `translate_device_addr`
    /// (`kernel/core::devres`).
    #[test]
    fn dma_alloc_translated_grant_reaches_the_mechanism() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::dma_translated(
                0x2_0000_0000,
                0x2_0000_0000,
                0x4_0000_0000,
            ),
        );
        let facility: &'static RecordingDmaFacility = Box::leak(Box::new(RecordingDmaFacility {
            last: rustos_sync::SpinLock::new(None),
            freed: rustos_sync::SpinLock::new(None),
            ret: Ok(crate::devres::DmaCarve {
                cpu_va: 0xD000_0000,
                device_addr: 0x10_0000,
            }),
        }));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_dma_alloc_facility(facility);

        // No address space registered for task 2 → the (translated)
        // device-address copy-out fails closed; the point is the carve ran
        // rather than the grant being rejected pre-carve.
        assert_eq!(
            h.dma_alloc(&ctx, handle, 0x1000, 0x1234),
            Err(Errno::BadAddress)
        );
        // The carve ran with the request length and the grant's CPU-side
        // addressing limit — proving the translated grant is no longer
        // refused before the mechanism.
        assert_eq!(*facility.last.lock(), Some((0x1000, 0x2_0000_0000)));
    }

    /// A zero-length request and an over-the-grant-maximum request are both
    /// refused before the carve (validate every input).
    #[test]
    fn dma_alloc_rejects_zero_and_over_max_length() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // The grant declares a 0x1_0000-byte maximum extent.
        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::dma(0x4000_0000, 0x1_0000),
        );
        let facility: &'static RecordingDmaFacility = Box::leak(Box::new(RecordingDmaFacility {
            last: rustos_sync::SpinLock::new(None),
            freed: rustos_sync::SpinLock::new(None),
            ret: Ok(crate::devres::DmaCarve {
                cpu_va: 0xD000_0000,
                device_addr: 0x4000_0000,
            }),
        }));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_dma_alloc_facility(facility);

        assert_eq!(
            h.dma_alloc(&ctx, handle, 0, 0x1234),
            Err(Errno::LengthOutOfRange)
        );
        assert_eq!(
            h.dma_alloc(&ctx, handle, 0x1_0001, 0x1234),
            Err(Errno::OutOfRange)
        );
        assert!(facility.last.lock().is_none());
    }

    /// A valid, owned, untranslated DMA grant whose mechanism is unwired
    /// holds `NULL_DMA_ALLOC_FACILITY` and fails closed with `NotImplemented` — proving the lookup + validation passed and the
    /// missing producer denies rather than fabricating a buffer.
    #[test]
    fn dma_alloc_with_grant_but_no_facility_is_not_implemented() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::dma(0x4000_0000, 0x1_0000),
        );
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );

        assert_eq!(
            h.dma_alloc(&ctx, handle, 0x1000, 0x1234),
            Err(Errno::NotImplemented)
        );
    }

    /// The validated request reaches the carve mechanism with the grant's
    /// `addr_limit` (not any caller-supplied bound): with the facility
    /// returning a carve but no caller address space registered for the
    /// device-address copy-out, the handler fails closed with `BadAddress`
    /// (the same fault `wait`'s copy-out produces) — proving the request
    /// flowed all the way to the mechanism with the granted limit.
    #[test]
    fn dma_alloc_reaches_the_mechanism_with_the_granted_limit() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::dma(0x4000_0000, 0x1_0000),
        );
        let facility: &'static RecordingDmaFacility = Box::leak(Box::new(RecordingDmaFacility {
            last: rustos_sync::SpinLock::new(None),
            freed: rustos_sync::SpinLock::new(None),
            ret: Ok(crate::devres::DmaCarve {
                cpu_va: 0xD000_0000,
                device_addr: 0x4000_0000,
            }),
        }));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_dma_alloc_facility(facility);

        // No address space is registered for task 2, so the device-address
        // copy-out fails closed with `BadAddress`.
        assert_eq!(
            h.dma_alloc(&ctx, handle, 0x1000, 0x1234),
            Err(Errno::BadAddress)
        );
        // The carve nonetheless ran with the request length and the grant's
        // addressing limit — never a caller-supplied bound.
        assert_eq!(*facility.last.lock(), Some((0x1000, 0x4000_0000)));
    }

    /// Build a `RecordingDmaFacility` over a fresh-and-`None` carve record,
    /// leaked to the `'static` shape the handler holds. The five `dma_free`
    /// tests share this.
    fn recording_dma_facility() -> &'static RecordingDmaFacility {
        Box::leak(Box::new(RecordingDmaFacility {
            last: rustos_sync::SpinLock::new(None),
            freed: rustos_sync::SpinLock::new(None),
            ret: Ok(crate::devres::DmaCarve {
                cpu_va: 0xD000_0000,
                device_addr: 0x4000_0000,
            }),
        }))
    }

    /// A valid `dma_free` against an owned DMA grant reaches the release
    /// mechanism with the caller-supplied CPU base and reports `Ok(0)` — the
    /// symmetric free for `dma_alloc`.
    #[test]
    fn dma_free_reaches_the_mechanism_with_the_cpu_base() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::dma(0x4000_0000, 0x1_0000),
        );
        let facility = recording_dma_facility();
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_dma_alloc_facility(facility);

        assert_eq!(h.dma_free(&ctx, handle, 0xD000_0000), Ok(0));
        assert_eq!(*facility.freed.lock(), Some(0xD000_0000));
    }

    /// With no grant minted for the caller, `dma_free` resolves nothing and
    /// fails closed with `NotFound` without touching the release mechanism —
    /// a driver can never free against an ungranted constraint.
    #[test]
    fn dma_free_without_grant_is_not_found() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let facility = recording_dma_facility();
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_dma_alloc_facility(facility);

        assert_eq!(h.dma_free(&ctx, 7, 0xD000_0000), Err(Errno::NotFound));
        assert!(facility.freed.lock().is_none(), "no release was attempted");
    }

    /// The DMA grant is owner-bound: a handle minted for another task, or an
    /// unknown handle value, resolves to nothing and `dma_free` is refused
    /// (handle forgery rejected exactly as `dma_alloc`).
    #[test]
    fn dma_free_forged_or_foreign_handle_is_not_found() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::dma(0x4000_0000, 0x1_0000),
        );
        let facility = recording_dma_facility();
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_dma_alloc_facility(facility);

        // Right owner, wrong handle → NotFound.
        let owner = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        assert_eq!(
            h.dma_free(&owner, handle + 1, 0xD000_0000),
            Err(Errno::NotFound)
        );
        // Right handle value, wrong (foreign) task → NotFound.
        let foreign = CallerContext {
            task_id: SecTaskId(3),
            caps: &caps,
        };
        assert_eq!(
            h.dma_free(&foreign, handle, 0xD000_0000),
            Err(Errno::NotFound)
        );
        assert!(facility.freed.lock().is_none(), "no release was attempted");
    }

    /// A grant of a non-DMA kind (an MMIO window) is refused with `OutOfRange`
    /// before the release mechanism is reached: `dma_free` releases against
    /// DMA constraints only.
    #[test]
    fn dma_free_non_dma_grant_is_out_of_range() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::mmio(0xFE98_0000, 0x4000),
        );
        let facility = recording_dma_facility();
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_dma_alloc_facility(facility);

        assert_eq!(
            h.dma_free(&ctx, handle, 0xD000_0000),
            Err(Errno::OutOfRange)
        );
        assert!(facility.freed.lock().is_none(), "no release was attempted");
    }

    /// With a DMA grant but no facility wired, `dma_free` reaches the default
    /// `NullDmaAllocFacility` and fails closed with `NotImplemented` (the
    /// fail-closed default, symmetric with `dma_alloc`).
    #[test]
    fn dma_free_with_grant_but_no_facility_is_not_implemented() {
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = mmio_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let handle = aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::dma(0x4000_0000, 0x1_0000),
        );
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.dma_free(&ctx, handle, 0xD000_0000),
            Err(Errno::NotImplemented)
        );
    }

    // --- resource_grants (the grant-delivery syscall, `plans/PI.md` 5d-2) ---

    /// Build a handler over a caller (task 2) whose address space maps a
    /// writable user page at `0x1000` (so the grant copy-out has somewhere
    /// to land), mirroring the `wait` copy-out tests. The five
    /// `resource_grants` tests share this exact scaffold.
    #[allow(clippy::type_complexity)]
    fn grants_scaffold() -> (
        Arc<TestArch>,
        RwLock<CapTable>,
        RwLock<PortRegistry>,
        RwLock<AddressSpaceRegistry>,
        RwLock<Box<dyn RandomReserve + Send + Sync>>,
        IrqTable,
    ) {
        let arch = Arc::new(TestArch::with_cpus(1));
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        (arch, table, ipc, aspaces, unseeded_rng(), IrqTable::new(31))
    }

    /// A task with no minted grants reads an empty set: `Ok(0)`, never an
    /// error (an unbound driver is normal). No copy is
    /// performed.
    #[test]
    fn resource_grants_with_no_grants_returns_zero() {
        install_trace_filter();
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = grants_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.resource_grants(&ctx, 0x1000, 0x1000), Ok(0));
    }

    /// The caller's minted grants are serialised and copied out: the return
    /// is the total byte count (one [`rustos_abi::hwtree::GrantedResource`]
    /// record per grant), proving enumeration + the copy-out succeeded.
    #[test]
    fn resource_grants_returns_total_byte_count() {
        install_trace_filter();
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = grants_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::mmio(0xFE98_0000, 0x4000),
        );
        aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::dma(0x4000_0000, 0x1_0000),
        );
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let expected = 2 * rustos_abi::hwtree::GrantedResource::WIRE_LEN as u64;
        assert_eq!(h.resource_grants(&ctx, 0x1000, 0x1000), Ok(expected));
    }

    /// A buffer too small for the whole grant set fails closed with
    /// `BufferTooSmall` rather than delivering a partial list. The check happens before any copy.
    #[test]
    fn resource_grants_buffer_too_small_fails_closed() {
        install_trace_filter();
        let sink = make_sink();
        let (arch, table, ipc, aspaces, rng, irq) = grants_scaffold();
        let sched = make_sched(arch.clone());
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::mmio(0xFE98_0000, 0x4000),
        );
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        // 8 bytes cannot hold one 40-byte record.
        assert_eq!(
            h.resource_grants(&ctx, 0x1000, 8),
            Err(Errno::BufferTooSmall)
        );
    }

    /// A caller with grants but no registered address space cannot receive
    /// the copy-out and fails closed with `BadAddress`, never leaking the
    /// missing-space case.
    #[test]
    fn resource_grants_unregistered_caller_is_bad_address() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::mmio(0xFE98_0000, 0x4000),
        );
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.resource_grants(&ctx, 0x1000, 0x1000),
            Err(Errno::BadAddress)
        );
    }

    /// Grants are owner-scoped: a task reads only its *own* grants. Task 2
    /// holds a grant; task 3 (the registered caller here) sees an empty set
    /// — it cannot enumerate another driver's handles.
    #[test]
    fn resource_grants_is_owner_scoped() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        aspaces
            .write()
            .register(SecTaskId(3), space, physmap)
            .expect("registration succeeds");
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(3, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(3),
            caps: &caps,
        };
        // The grant belongs to task 2, not the calling task 3.
        aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::hwtree::HwResource::mmio(0xFE98_0000, 0x4000),
        );
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.resource_grants(&ctx, 0x1000, 0x1000), Ok(0));
    }

    /// A `ProcessWait` producer that records the last `(parent, pid)` it was
    /// handed and returns a configured result, so the handler tests can
    /// assert the arguments reached it and the result flowed back without a
    /// real scheduler-side wait path.
    struct RecordingProcessWait {
        last: rustos_sync::SpinLock<Option<(u64, i32)>>,
        last_exit: rustos_sync::SpinLock<Option<(u64, i32)>>,
        last_register: rustos_sync::SpinLock<Option<(u64, u64)>>,
        result: Result<crate::procwait::ReapedChild, Errno>,
    }
    impl RecordingProcessWait {
        fn new(result: Result<crate::procwait::ReapedChild, Errno>) -> Self {
            Self {
                last: rustos_sync::SpinLock::new(None),
                last_exit: rustos_sync::SpinLock::new(None),
                last_register: rustos_sync::SpinLock::new(None),
                result,
            }
        }
    }
    impl crate::procwait::ProcessWait for RecordingProcessWait {
        fn wait(&self, parent: SecTaskId, pid: i32) -> Result<crate::procwait::ReapedChild, Errno> {
            *self.last.lock() = Some((parent.0, pid));
            self.result
        }
        fn record_exit(&self, task: SecTaskId, code: i32) {
            *self.last_exit.lock() = Some((task.0, code));
        }
        fn register_child(&self, parent: SecTaskId, child: SecTaskId) {
            *self.last_register.lock() = Some((parent.0, child.0));
        }
    }

    /// `wait` needs no capability (a process reaps its
    /// own children): it forwards the decoded `(parent, pid)` to the
    /// installed producer, writes the reaped child's exit code to the
    /// caller's `status` pointer through the validated copy-out boundary,
    /// and returns the reaped child's PID.
    #[test]
    fn wait_forwards_to_producer_and_returns_reaped_pid() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        // A writable user page at VA 0x1000 backs the exit-code copy-out.
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let producer: &'static RecordingProcessWait = Box::leak(Box::new(
            RecordingProcessWait::new(Ok(crate::procwait::ReapedChild { pid: 42, code: 7 })),
        ));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_process_wait(producer);

        // Returns the reaped child's PID; the producer saw the caller's
        // task id as `parent` and the requested `pid` verbatim.
        assert_eq!(h.wait(&ctx, 9, 0x1000), Ok(42));
        assert_eq!(*producer.last.lock(), Some((2, 9)));
    }

    /// With no producer installed the handler holds `NULL_PROCESS_WAIT` and
    /// fails closed with `NotImplemented`.
    #[test]
    fn wait_without_producer_is_not_implemented() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.wait(&ctx, 9, 0x1000), Err(Errno::NotImplemented));
    }

    /// A producer error (e.g. `pid` is not a child of the caller) propagates
    /// verbatim, and the `status` pointer is never written on the error path
    /// (fail closed).
    #[test]
    fn wait_propagates_producer_error() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let producer: &'static RecordingProcessWait =
            Box::leak(Box::new(RecordingProcessWait::new(Err(Errno::NotFound))));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_process_wait(producer);

        assert_eq!(h.wait(&ctx, 9, 0x1000), Err(Errno::NotFound));
        assert_eq!(*producer.last.lock(), Some((2, 9)));
    }

    /// `wait` from a caller with no registered address space fails closed
    /// with `BadAddress` — the reaped child's code cannot be copied out, and
    /// the missing-space case is not leaked.
    #[test]
    fn wait_unregistered_caller_is_bad_address() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let producer: &'static RecordingProcessWait = Box::leak(Box::new(
            RecordingProcessWait::new(Ok(crate::procwait::ReapedChild { pid: 42, code: 0 })),
        ));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_process_wait(producer);

        // The child was reaped (the producer ran) but its code cannot be
        // delivered: the unregistered caller fails closed with BadAddress.
        assert_eq!(h.wait(&ctx, 9, 0x1000), Err(Errno::BadAddress));
    }

    /// `exit` hands the caller's task id and exit code to the process-wait
    /// producer so a parent blocked in `wait` can later reap it and read the
    /// code back (`plans/SPAWN.md` SP6). The handler still reports success and
    /// performs its security cleanup.
    #[test]
    fn exit_records_exit_code_with_the_process_wait_producer() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(7, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &caps,
        };

        let producer: &'static RecordingProcessWait = Box::leak(Box::new(
            RecordingProcessWait::new(Ok(crate::procwait::ReapedChild { pid: 0, code: 0 })),
        ));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_process_wait(producer);

        assert_eq!(h.exit(&ctx, 42), Ok(0));
        // The producer saw this task's id and its exit code.
        assert_eq!(*producer.last_exit.lock(), Some((7, 42)));
    }

    /// `rlimit_get` for a registered caller copies the effective limit out:
    /// with none imposed, every kind reads the default policy
    /// ([`LimitSet::DEFAULT`], unlimited).
    #[test]
    fn rlimit_get_returns_the_default_policy_for_a_fresh_task() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.rlimit_get(&ctx, LimitKind::Processes.as_u32(), 0x1000),
            Ok(0)
        );
        // The encoded default limit landed at the caller's pointer.
        let delivered = h
            .with_caller_aspace(&ctx, |space, physmap| {
                let mut buf = [0u8; ResourceLimit::WIRE_LEN];
                copy_in(space, physmap, VirtAddr::new(0x1000), &mut buf).expect("readable");
                ResourceLimit::decode(&buf).expect("well-formed")
            })
            .expect("caller has a registered space");
        assert_eq!(delivered, ResourceLimit::UNLIMITED);
    }

    /// `rlimit_get` validates `kind` against the closed abi-v1 set before
    /// touching state: an unassigned discriminant fails closed with
    /// `OutOfRange`.
    #[test]
    fn rlimit_get_rejects_an_unassigned_kind() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let bad_kind = u32::try_from(LimitKind::COUNT).expect("small count");
        assert_eq!(h.rlimit_get(&ctx, bad_kind, 0x1000), Err(Errno::OutOfRange));
    }

    /// `rlimit_get` from a caller with no registered address space fails
    /// closed with `BadAddress` — the limit cannot be copied out and the
    /// missing-space case is not leaked.
    #[test]
    fn rlimit_get_unregistered_caller_is_bad_address() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.rlimit_get(&ctx, LimitKind::Processes.as_u32(), 0x1000),
            Err(Errno::BadAddress)
        );
    }

    /// `rlimit_set` lowering a bound is free (no capability) and the new
    /// ceiling is stored against the caller's own task id.
    #[test]
    fn rlimit_set_lowers_freely_and_stores_against_the_caller() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let lower = ResourceLimit::new(10, 50).expect("well-formed");
        let (space, physmap) = send_aspace(
            MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
            &lower.encode(),
        );
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        // No CAP_RLIMIT_RAISE: lowering still succeeds.
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.rlimit_set(&ctx, LimitKind::Processes.as_u32(), 0x1000),
            Ok(0)
        );
        assert_eq!(
            aspaces
                .read()
                .limits(SecTaskId(2))
                .get(LimitKind::Processes),
            lower
        );
    }

    /// Stage the encoded `limit` at the caller's page-1 pointer (`0x1000`)
    /// so a following `rlimit_set` reads it. Used to drive a second `set`
    /// in the raise tests after the first lowered the bound.
    fn stage_limit<A: KernelArch + 'static>(
        h: &KernelSyscallHandlers<'_, A>,
        ctx: &CallerContext<'_>,
        limit: ResourceLimit,
    ) {
        h.with_caller_aspace(ctx, |space, physmap| {
            copy_out(space, physmap, VirtAddr::new(0x1000), &limit.encode()).expect("writable");
        })
        .expect("caller has a registered space");
    }

    /// `rlimit_set` raising a hard bound above the current ceiling without
    /// `CAP_RLIMIT_RAISE` is refused with `PermissionDenied`, and the stored
    /// limit is left unchanged (fail closed).
    #[test]
    fn rlimit_set_raising_hard_without_capability_is_denied() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let lower = ResourceLimit::new(10, 50).expect("well-formed");
        let (space, physmap) = send_aspace(
            MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
            &lower.encode(),
        );
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        // Lower the ceiling to (10, 50) first — this is free.
        assert_eq!(
            h.rlimit_set(&ctx, LimitKind::Processes.as_u32(), 0x1000),
            Ok(0)
        );
        // Now attempt to raise the hard bound above the new ceiling.
        let higher = ResourceLimit::new(10, 100).expect("well-formed");
        stage_limit(&h, &ctx, higher);
        assert_eq!(
            h.rlimit_set(&ctx, LimitKind::Processes.as_u32(), 0x1000),
            Err(Errno::PermissionDenied)
        );
        // The stored ceiling is unchanged.
        assert_eq!(
            aspaces
                .read()
                .limits(SecTaskId(2))
                .get(LimitKind::Processes),
            lower
        );
    }

    /// `rlimit_set` raising a hard bound *with* `CAP_RLIMIT_RAISE` succeeds
    /// and the higher ceiling is stored.
    #[test]
    fn rlimit_set_raising_hard_with_capability_succeeds() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let lower = ResourceLimit::new(10, 50).expect("well-formed");
        let (space, physmap) = send_aspace(
            MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
            &lower.encode(),
        );
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::RLIMIT_RAISE], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.rlimit_set(&ctx, LimitKind::Processes.as_u32(), 0x1000),
            Ok(0)
        );
        let higher = ResourceLimit::new(10, 100).expect("well-formed");
        stage_limit(&h, &ctx, higher);
        assert_eq!(
            h.rlimit_set(&ctx, LimitKind::Processes.as_u32(), 0x1000),
            Ok(0)
        );
        assert_eq!(
            aspaces
                .read()
                .limits(SecTaskId(2))
                .get(LimitKind::Processes),
            higher
        );
    }

    /// `rlimit_set` fed a malformed pair (`soft > hard`) fails closed with
    /// `OutOfRange` at decode and stores nothing.
    #[test]
    fn rlimit_set_rejects_a_malformed_pair() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        // soft (10) > hard (5): a hand-built buffer the kernel must reject.
        let mut bytes = [0u8; ResourceLimit::WIRE_LEN];
        bytes[0] = 10;
        bytes[8] = 5;
        let (space, physmap) =
            send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &bytes);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::RLIMIT_RAISE], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.rlimit_set(&ctx, LimitKind::Processes.as_u32(), 0x1000),
            Err(Errno::OutOfRange)
        );
        // Nothing was stored: the caller still runs under the default.
        assert_eq!(aspaces.read().limits(SecTaskId(2)), LimitSet::DEFAULT);
    }

    /// A [`UsersDbSource`] double holding a fixed `users-v1` text.
    struct StaticUsersDb(&'static [u8]);
    impl UsersDbSource for StaticUsersDb {
        fn text(&self) -> Result<&[u8], Errno> {
            Ok(self.0)
        }
    }

    /// A wired [`UsersDbSource`] whose boot read refused the record, so
    /// no database is held.
    struct AbsentUsersDb;
    impl UsersDbSource for AbsentUsersDb {
        fn text(&self) -> Result<&[u8], Errno> {
            Err(Errno::NotFound)
        }
    }

    /// A wired [`UsersDbSource`] still in its *pending* state: the unlock is
    /// in flight, so `text` returns the live-but-not-ready
    /// [`Errno::WouldBlock`] and [`UsersDbSource::is_pending`] is `true`.
    /// This is the only state `users_db_wait` blocks on.
    struct PendingUsersDb;
    impl UsersDbSource for PendingUsersDb {
        fn text(&self) -> Result<&[u8], Errno> {
            Err(Errno::WouldBlock)
        }
    }

    /// Stand-in database text the handler tests serve. The handler copies
    /// the held text verbatim (the caller re-parses it), so the double
    /// does not need to be a full valid `users-v1` document.
    static USERS_DB_TEXT: &[u8] = b"users-v1\nroot:0:0::root:/Users/root:/Apps/Shell.app/Run\n";

    /// `users_db_read` copies the held database text out to the caller
    /// and returns its exact length (`plans/PI.md` P11).
    #[test]
    fn users_db_read_copies_held_text_out_to_caller() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::USERS_READ], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let source: &'static StaticUsersDb = Box::leak(Box::new(StaticUsersDb(USERS_DB_TEXT)));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_users_db(source);

        assert_eq!(
            h.users_db_read(&ctx, 0x1000, 4096),
            Ok(USERS_DB_TEXT.len() as u64)
        );
        // The exact text landed at the caller's pointer.
        let delivered = h
            .with_caller_aspace(&ctx, |space, physmap| {
                let mut buf = alloc::vec![0u8; USERS_DB_TEXT.len()];
                copy_in(space, physmap, VirtAddr::new(0x1000), &mut buf).expect("readable");
                buf
            })
            .expect("caller has a registered space");
        assert_eq!(delivered.as_slice(), USERS_DB_TEXT);
    }

    /// With no holder wired the handler keeps `NULL_USERS_DB` and fails
    /// closed with `NotImplemented` rather than fabricating accounts.
    #[test]
    fn users_db_read_without_holder_is_not_implemented() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::USERS_READ], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.users_db_read(&ctx, 0x1000, 4096),
            Err(Errno::NotImplemented)
        );
    }

    /// A wired holder with no database (the boot read refused the
    /// record, or no root volume is mounted) fails closed with
    /// `NotFound`, so a system without accounts refuses every login.
    #[test]
    fn users_db_read_with_no_database_is_not_found() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::USERS_READ], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let source: &'static AbsentUsersDb = Box::leak(Box::new(AbsentUsersDb));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_users_db(source);
        assert_eq!(h.users_db_read(&ctx, 0x1000, 4096), Err(Errno::NotFound));
    }

    /// `users_db_wait` with no holder wired returns `Ok(0)` immediately —
    /// an inert database is never *pending*, so `login` does not block and
    /// its subsequent `users_db_read` fails closed.
    #[test]
    fn users_db_wait_without_holder_returns_ok_immediately() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::USERS_READ], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        // `NULL_USERS_DB` is `NotImplemented`, not pending: wake immediately.
        assert_eq!(h.users_db_wait(&ctx, u64::MAX), Ok(0));
    }

    /// `users_db_wait` returns `Ok(0)` immediately once the database has
    /// resolved (absent or present) — only the in-flight *pending* state
    /// blocks.
    #[test]
    fn users_db_wait_returns_ok_when_already_resolved() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::USERS_READ], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let absent: &'static AbsentUsersDb = Box::leak(Box::new(AbsentUsersDb));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_users_db(absent);
        assert_eq!(h.users_db_wait(&ctx, u64::MAX), Ok(0));
    }

    /// `users_db_wait` with a zero timeout while the database is still
    /// pending returns `TimedOut` without busy-spinning.
    #[test]
    fn users_db_wait_times_out_while_pending() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::USERS_READ], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let pending: &'static PendingUsersDb = Box::leak(Box::new(PendingUsersDb));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_users_db(pending);
        // Still pending and a zero deadline: report the timeout rather than
        // parking forever in the host test (no live dispatch loop).
        assert_eq!(h.users_db_wait(&ctx, 0), Err(Errno::TimedOut));
    }

    /// An undersized buffer is refused whole with `BufferTooSmall` — a
    /// credential database is never truncated — and
    /// nothing is copied to the caller.
    #[test]
    fn users_db_read_undersized_buffer_is_buffer_too_small() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::USERS_READ], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let source: &'static StaticUsersDb = Box::leak(Box::new(StaticUsersDb(USERS_DB_TEXT)));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_users_db(source);

        assert_eq!(
            h.users_db_read(&ctx, 0x1000, USERS_DB_TEXT.len() - 1),
            Err(Errno::BufferTooSmall)
        );
        // Nothing was copied: the caller's page still reads zero.
        let untouched = h
            .with_caller_aspace(&ctx, |space, physmap| {
                let mut buf = [0u8; 8];
                copy_in(space, physmap, VirtAddr::new(0x1000), &mut buf).expect("readable");
                buf
            })
            .expect("caller has a registered space");
        assert_eq!(untouched, [0u8; 8]);
    }

    /// A caller with no registered address space fails closed with
    /// `BadAddress`, exactly like every other copy-out path.
    #[test]
    fn users_db_read_unregistered_caller_is_bad_address() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::USERS_READ], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let source: &'static StaticUsersDb = Box::leak(Box::new(StaticUsersDb(USERS_DB_TEXT)));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_users_db(source);
        assert_eq!(h.users_db_read(&ctx, 0x1000, 4096), Err(Errno::BadAddress));
    }

    /// A test [`HwTreeSource`] holding a fixed generation, a pre-encoded
    /// snapshot blob, and a record of every node published through
    /// [`HwTreeSource::publish`] so a `hw_emit_node` test can assert what
    /// reached the store.
    struct StaticHwTree {
        generation: u64,
        blob: alloc::vec::Vec<u8>,
        published: RwLock<alloc::vec::Vec<(u32, rustos_abi::HwNode)>>,
        // Every `(parent_id, node_id)` removed through `HwTreeSource::remove`,
        // so a `hw_remove_node` test can assert what the handler passed.
        removed: RwLock<alloc::vec::Vec<(u32, u32)>>,
        // Node ids this double rejects with `NotFound` (a node the caller does
        // not own / an absent node), so a test can drive the fail-closed arm.
        unremovable: RwLock<alloc::vec::Vec<u32>>,
    }

    impl StaticHwTree {
        fn new(generation: u64, blob: alloc::vec::Vec<u8>) -> Self {
            Self {
                generation,
                blob,
                published: RwLock::new(alloc::vec::Vec::new()),
                removed: RwLock::new(alloc::vec::Vec::new()),
                unremovable: RwLock::new(alloc::vec::Vec::new()),
            }
        }
    }

    impl HwTreeSource for StaticHwTree {
        fn generation(&self) -> Result<u64, Errno> {
            Ok(self.generation)
        }
        fn snapshot(&self) -> Result<alloc::vec::Vec<u8>, Errno> {
            Ok(self.blob.clone())
        }
        fn publish(&self, parent_id: u32, node: rustos_abi::HwNode) -> Result<u32, Errno> {
            // Record the kernel-resolved parent the handler passed alongside
            // the node, so a test can assert the child is parented under the
            // emitter's own loaded node. Model the real store's identity
            // assignment with a deterministic id (a fixed base plus the
            // publish count) so the handler's id-return path is exercised.
            let id = 100u32 + u32::try_from(self.published.read().len()).unwrap_or(0);
            self.published.write().push((parent_id, node));
            Ok(id)
        }
        fn remove(&self, parent_id: u32, node_id: u32) -> Result<(), Errno> {
            // Model the store's fail-closed ownership gate: a node listed as
            // unremovable (unknown / not owned by the caller) is `NotFound`
            // and is never recorded as removed.
            if self.unremovable.read().contains(&node_id) {
                return Err(Errno::NotFound);
            }
            self.removed.write().push((parent_id, node_id));
            Ok(())
        }
    }

    /// Build a wire-encoded `[HwTreeHeader][HwNode; n]` snapshot at
    /// `generation` from `nodes`, exactly as the production source does.
    fn encode_hw_snapshot(generation: u64, nodes: &[rustos_abi::HwNode]) -> alloc::vec::Vec<u8> {
        let header = rustos_abi::HwTreeHeader::new(generation, nodes.len() as u64).to_le_bytes();
        let mut blob = alloc::vec::Vec::with_capacity(header.len() + nodes.len() * 572);
        blob.extend_from_slice(&header);
        for node in nodes {
            blob.extend_from_slice(&node.to_le_bytes());
        }
        blob
    }

    /// `hw_tree_read` copies the wire-encoded snapshot out to the caller
    /// and returns its exact length.
    #[test]
    fn hw_tree_read_copies_the_snapshot_out_to_caller() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::SYSINFO_HW], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let nodes = [
            rustos_abi::HwNode::new(1, rustos_abi::HW_NODE_ROOT, rustos_abi::HwDeviceClass::Root),
            rustos_abi::HwNode::new(2, 1, rustos_abi::HwDeviceClass::Bus),
        ];
        let blob = encode_hw_snapshot(7, &nodes);
        let source: &'static StaticHwTree = Box::leak(Box::new(StaticHwTree::new(7, blob.clone())));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_hw_tree(source);

        assert_eq!(h.hw_tree_read(&ctx, 0x1000, 4096), Ok(blob.len() as u64));
        // The exact bytes landed at the caller's pointer, and the header
        // reports the same generation and node count.
        let delivered = h
            .with_caller_aspace(&ctx, |space, physmap| {
                let mut buf = alloc::vec![0u8; blob.len()];
                copy_in(space, physmap, VirtAddr::new(0x1000), &mut buf).expect("readable");
                buf
            })
            .expect("caller has a registered space");
        assert_eq!(delivered, blob);
        let header = rustos_abi::HwTreeHeader::from_bytes(&delivered).expect("header");
        assert_eq!(header.generation(), 7);
        assert_eq!(header.node_count(), 2);
    }

    /// With no store wired `hw_tree_read` keeps `NULL_HW_TREE` and fails
    /// closed with `NotImplemented`.
    #[test]
    fn hw_tree_read_without_store_is_not_implemented() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::SYSINFO_HW], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.hw_tree_read(&ctx, 0x1000, 4096),
            Err(Errno::NotImplemented)
        );
    }

    /// An undersized buffer is refused whole with `BufferTooSmall` — the
    /// inventory is never truncated — and nothing is
    /// copied to the caller.
    #[test]
    fn hw_tree_read_undersized_buffer_is_buffer_too_small() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::SYSINFO_HW], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let nodes = [rustos_abi::HwNode::new(
            1,
            rustos_abi::HW_NODE_ROOT,
            rustos_abi::HwDeviceClass::Root,
        )];
        let blob = encode_hw_snapshot(1, &nodes);
        let source: &'static StaticHwTree = Box::leak(Box::new(StaticHwTree::new(1, blob.clone())));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_hw_tree(source);

        assert_eq!(
            h.hw_tree_read(&ctx, 0x1000, blob.len() - 1),
            Err(Errno::BufferTooSmall)
        );
        // Nothing was copied: the caller's page still reads zero.
        let untouched = h
            .with_caller_aspace(&ctx, |space, physmap| {
                let mut buf = [0u8; 8];
                copy_in(space, physmap, VirtAddr::new(0x1000), &mut buf).expect("readable");
                buf
            })
            .expect("caller has a registered space");
        assert_eq!(untouched, [0u8; 8]);
    }

    /// A caller with no registered address space fails closed with
    /// `BadAddress`, like every other copy-out path.
    #[test]
    fn hw_tree_read_unregistered_caller_is_bad_address() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::SYSINFO_HW], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let blob = encode_hw_snapshot(1, &[]);
        let source: &'static StaticHwTree = Box::leak(Box::new(StaticHwTree::new(1, blob)));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_hw_tree(source);
        assert_eq!(h.hw_tree_read(&ctx, 0x1000, 4096), Err(Errno::BadAddress));
    }

    /// `hw_tree_wait` returns immediately with `Ok(0)` when the store's
    /// generation already differs from the one the caller observed — the
    /// tree changed, so re-read and re-match.
    #[test]
    fn hw_tree_wait_returns_ok_when_generation_already_advanced() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::SYSINFO_HW], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let source: &'static StaticHwTree =
            Box::leak(Box::new(StaticHwTree::new(5, encode_hw_snapshot(5, &[]))));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_hw_tree(source);
        // Last observed generation 4 != current 5: wake immediately.
        assert_eq!(h.hw_tree_wait(&ctx, 4, u64::MAX), Ok(0));
    }

    /// `hw_tree_wait` with a zero timeout and an unchanged generation
    /// returns `TimedOut` without busy-spinning.
    #[test]
    fn hw_tree_wait_times_out_when_generation_unchanged() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::SYSINFO_HW], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let source: &'static StaticHwTree =
            Box::leak(Box::new(StaticHwTree::new(3, encode_hw_snapshot(3, &[]))));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_hw_tree(source);
        // Caller already observed generation 3; with a zero deadline and no
        // change, the wait reports the timeout.
        assert_eq!(h.hw_tree_wait(&ctx, 3, 0), Err(Errno::TimedOut));
    }

    /// With no store wired `hw_tree_wait` fails closed with
    /// `NotImplemented` rather than spinning.
    #[test]
    fn hw_tree_wait_without_store_is_not_implemented() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::SYSINFO_HW], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.hw_tree_wait(&ctx, 0, u64::MAX),
            Err(Errno::NotImplemented)
        );
    }

    // ---- hw_emit_node ------------------------------------------------

    /// An emitted single-resource [`rustos_abi::HwNode`] (an MMIO child
    /// keyed by a USB-class match key), used by the publish tests.
    fn emit_child_node() -> rustos_abi::HwNode {
        let mut node = rustos_abi::HwNode::new(3, 2, rustos_abi::HwDeviceClass::Input);
        node.push_match_key(rustos_abi::HwMatchKey::usb(0x1234, 0x5678, 0x03_01_01))
            .expect("match key fits");
        node.push_resource(rustos_abi::HwResource::mmio(0xFE98_0000, 0x4000))
            .expect("resource fits");
        node
    }

    /// `hw_emit_node` rejects a `len` that is not exactly the node wire
    /// size before copying anything, so a hostile length cannot drive a
    /// large copy and a short one cannot decode.
    #[test]
    fn hw_emit_node_rejects_a_wrong_length() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::HW_EMIT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let source: &'static StaticHwTree =
            Box::leak(Box::new(StaticHwTree::new(0, encode_hw_snapshot(0, &[]))));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_hw_tree(source);
        assert_eq!(
            h.hw_emit_node(&ctx, 0x1000, rustos_abi::HwNode::WIRE_LEN - 1),
            Err(Errno::LengthOutOfRange)
        );
        assert!(source.published.read().is_empty(), "nothing was published");
    }

    /// An emitted node requesting a resource **not** covered by any of the
    /// calling driver's grants is refused with `PermissionDenied`, and
    /// nothing is published — a bus driver can never mint a child more
    /// authority than it holds.
    #[test]
    fn hw_emit_node_without_a_covering_grant_is_denied() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let node = emit_child_node();
        let bytes = node.to_le_bytes();
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &bytes);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        // The driver holds a grant for a *different* window, so the emitted
        // node's resource is not covered.
        aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::HwResource::mmio(0x3F20_0000, 0x1000),
        );
        // The caller is a driver loaded for a node, so it passes the
        // loaded-node gate and the test exercises the *coverage* refusal.
        aspaces.write().set_loaded_node(SecTaskId(2), 1);
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::HW_EMIT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let source: &'static StaticHwTree =
            Box::leak(Box::new(StaticHwTree::new(0, encode_hw_snapshot(0, &[]))));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_hw_tree(source);
        assert_eq!(
            h.hw_emit_node(&ctx, 0x1000, rustos_abi::HwNode::WIRE_LEN),
            Err(Errno::PermissionDenied)
        );
        assert!(
            source.published.read().is_empty(),
            "a denied node is never published"
        );
    }

    /// An emitter that is **not** an autoloaded driver bound to a node
    /// (no loaded node recorded) is refused with `PermissionDenied` and
    /// publishes nothing, even when it holds a covering grant: a task that
    /// cannot name its own position in the tree may not place a child there
    /// (identity is kernel-provided, never
    /// caller-supplied).
    #[test]
    fn hw_emit_node_without_a_loaded_node_is_denied() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let node = emit_child_node();
        let bytes = node.to_le_bytes();
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &bytes);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        // A grant that *would* cover the resource — but no loaded node is
        // recorded, so the emitter still cannot publish.
        aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::HwResource::mmio(0xFE98_0000, 0x1_0000),
        );
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::HW_EMIT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let source: &'static StaticHwTree =
            Box::leak(Box::new(StaticHwTree::new(0, encode_hw_snapshot(0, &[]))));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_hw_tree(source);
        assert_eq!(
            h.hw_emit_node(&ctx, 0x1000, rustos_abi::HwNode::WIRE_LEN),
            Err(Errno::PermissionDenied)
        );
        assert!(
            source.published.read().is_empty(),
            "a task with no loaded node never publishes"
        );
    }

    /// An emitted node whose every resource is covered by one of the
    /// driver's grants is published into the live tree (`Ok(0)`), and the
    /// exact node reaches the store.
    #[test]
    fn hw_emit_node_with_a_covering_grant_publishes() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let node = emit_child_node();
        let bytes = node.to_le_bytes();
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &bytes);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        // A grant whose window contains the emitted resource fully covers it.
        aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::HwResource::mmio(0xFE98_0000, 0x1_0000),
        );
        // The emitter is a driver loaded for node 9; its published child is
        // parented under exactly that node.
        aspaces.write().set_loaded_node(SecTaskId(2), 9);
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::HW_EMIT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let source: &'static StaticHwTree =
            Box::leak(Box::new(StaticHwTree::new(0, encode_hw_snapshot(0, &[]))));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_hw_tree(source);
        // The handler returns the store-assigned id (the test-double's first
        // assignment is 100), so the emitter can later retract the child.
        assert_eq!(
            h.hw_emit_node(&ctx, 0x1000, rustos_abi::HwNode::WIRE_LEN),
            Ok(100)
        );
        let published = source.published.read();
        assert_eq!(published.len(), 1, "the covered node was published");
        // The handler passes the node unchanged and the emitter's own loaded
        // node (9) as the parent; the store assigns the final id/parent.
        assert_eq!(published[0].0, 9, "parented under the emitter's own node");
        assert_eq!(published[0].1, node, "the exact node reached the store");
    }

    /// The central recursive-PCI(e) case: a bus driver
    /// holding its host bridge's outbound window as a `BusWindow` grant
    /// publishes an enumerated child whose register BAR is an `Mmio` window
    /// resolved to a CPU address *inside* that bridge window. The
    /// `BusWindow`→`Mmio` coverage rule admits it, so the device behind the
    /// bridge autoloads — without the bridge ever minting authority it does
    /// not already hold.
    #[test]
    fn hw_emit_node_covers_a_child_bar_under_a_bridge_window() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        // The child's BAR is `mmio(0xFE98_0000, 0x4000)` (see `emit_child_node`).
        let node = emit_child_node();
        let bytes = node.to_le_bytes();
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &bytes);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        // The bridge's outbound window — CPU side [0xFE00_0000, 0xFF00_0000) —
        // contains the child's BAR, so the bridge legitimately grants it.
        aspaces.write().mint_grant(
            SecTaskId(2),
            rustos_abi::HwResource::bus_window(0xFE00_0000, 0x100_0000, 0x6_0000_0000),
        );
        // The bridge driver is loaded for its own node; the child is parented
        // under it.
        aspaces.write().set_loaded_node(SecTaskId(2), 1);
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::HW_EMIT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let source: &'static StaticHwTree =
            Box::leak(Box::new(StaticHwTree::new(0, encode_hw_snapshot(0, &[]))));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_hw_tree(source);
        // The handler returns the store-assigned id (first assignment 100).
        assert_eq!(
            h.hw_emit_node(&ctx, 0x1000, rustos_abi::HwNode::WIRE_LEN),
            Ok(100)
        );
        assert_eq!(
            source.published.read().len(),
            1,
            "the child BAR inside the bridge window was published"
        );
    }

    /// With no store wired `hw_emit_node` fails closed with
    /// `NotImplemented` even for a resourceless node.
    #[test]
    fn hw_emit_node_without_store_is_not_implemented() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        // A resourceless node passes the (empty) grant check, so the publish
        // reaches the inert `NULL_HW_TREE`.
        let node = rustos_abi::HwNode::new(3, 2, rustos_abi::HwDeviceClass::Input);
        let bytes = node.to_le_bytes();
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &bytes);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        // A driver loaded for a node, so the publish reaches the store (which
        // here is the inert `NULL_HW_TREE`) rather than failing the
        // loaded-node gate first.
        aspaces.write().set_loaded_node(SecTaskId(2), 2);
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::HW_EMIT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.hw_emit_node(&ctx, 0x1000, rustos_abi::HwNode::WIRE_LEN),
            Err(Errno::NotImplemented)
        );
    }

    /// An autoloaded bus driver (a loaded node recorded) removes a child it
    /// owns: the handler resolves the caller's own loaded node as the parent
    /// and passes it with the target id to the store, which removes the
    /// subtree. `Ok(0)` on success.
    #[test]
    fn hw_remove_node_with_a_loaded_node_removes_the_child() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        // The caller is the bus driver loaded for node 9; it owns the
        // children parented under 9 and may retire them.
        aspaces.write().set_loaded_node(SecTaskId(2), 9);
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::HW_EMIT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let source: &'static StaticHwTree =
            Box::leak(Box::new(StaticHwTree::new(0, encode_hw_snapshot(0, &[]))));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_hw_tree(source);
        assert_eq!(h.hw_remove_node(&ctx, 42), Ok(0));
        let removed = source.removed.read();
        assert_eq!(removed.len(), 1, "the owned node was removed");
        // The handler passes the emitter's own loaded node (9) as the
        // ownership parent and the requested target id.
        assert_eq!(
            removed[0],
            (9, 42),
            "removal is scoped to the caller's own node (`AGENTS.md` §4)"
        );
    }

    /// A caller that is **not** an autoloaded driver bound to a node (no
    /// loaded node recorded) cannot remove anything: it owns no position in
    /// the tree, so the handler fails closed with `PermissionDenied` and the
    /// store is never reached (identity is
    /// kernel-provided, never caller-supplied). Mirrors the emit gate.
    #[test]
    fn hw_remove_node_without_a_loaded_node_is_denied() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        // Deliberately no `set_loaded_node`: the caller owns nothing.
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::HW_EMIT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let source: &'static StaticHwTree =
            Box::leak(Box::new(StaticHwTree::new(0, encode_hw_snapshot(0, &[]))));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_hw_tree(source);
        assert_eq!(h.hw_remove_node(&ctx, 42), Err(Errno::PermissionDenied));
        assert!(
            source.removed.read().is_empty(),
            "a task with no loaded node removes nothing"
        );
    }

    /// A node the caller does not own (or an absent id) is surfaced as the
    /// store's fail-closed `NotFound`, even though the caller passed the
    /// loaded-node gate.
    #[test]
    fn hw_remove_node_for_an_unowned_node_is_not_found() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        aspaces.write().set_loaded_node(SecTaskId(2), 9);
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::HW_EMIT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let source: &'static StaticHwTree =
            Box::leak(Box::new(StaticHwTree::new(0, encode_hw_snapshot(0, &[]))));
        // Node 7 is not removable (models a node the caller does not own).
        source.unremovable.write().push(7);
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_hw_tree(source);
        assert_eq!(h.hw_remove_node(&ctx, 7), Err(Errno::NotFound));
        assert!(
            source.removed.read().is_empty(),
            "a node the caller does not own is never removed"
        );
    }

    /// A `node_id` above the `u32` range names no node and fails closed
    /// `NotFound` before the store is reached.
    #[test]
    fn hw_remove_node_rejects_an_out_of_range_id() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        aspaces.write().set_loaded_node(SecTaskId(2), 9);
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::HW_EMIT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let source: &'static StaticHwTree =
            Box::leak(Box::new(StaticHwTree::new(0, encode_hw_snapshot(0, &[]))));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_hw_tree(source);
        assert_eq!(
            h.hw_remove_node(&ctx, u64::from(u32::MAX) + 1),
            Err(Errno::NotFound)
        );
        assert!(source.removed.read().is_empty(), "nothing was removed");
    }

    /// With no store wired `hw_remove_node` fails closed with
    /// `NotImplemented`, like every hardware-tree op.
    #[test]
    fn hw_remove_node_without_store_is_not_implemented() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        aspaces.write().set_loaded_node(SecTaskId(2), 2);
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::HW_EMIT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.hw_remove_node(&ctx, 2), Err(Errno::NotImplemented));
    }

    // ---- ipc_call ----------------------------------------------------

    /// First frame of the two-page address space the `ipc_call` round-trip
    /// tests use: page 1 (`0x1000`) holds the request, page 2 (`0x2000`)
    /// receives the reply. Chosen disjoint from [`SEND_FRAME`].
    const CALL_FRAME: usize = 20;

    /// Build a caller address space mapping user page 1 (`0x1000`, the
    /// request) and page 2 (`0x2000`, the reply) `RW|USER` onto two
    /// contiguous frames, with `request` seeded at the start of page 1.
    /// Returns the boxed pair the registry stores; read the reply back via
    /// [`read_reply_page`].
    fn call_aspace(
        request: &[u8],
    ) -> (
        Box<dyn UserAddressSpace + Send + Sync>,
        Box<dyn PhysMap + Send + Sync>,
    ) {
        let base = PhysAddr::new(CALL_FRAME as u64 * PAGE_SIZE as u64);
        let sim = SimPhysMap::new(base, PAGE_SIZE * 2);
        if !request.is_empty() {
            let ptr = sim.translate(base, request.len()).expect("seed request");
            // SAFETY: the window owns these bytes for the simulator's
            // lifetime and nothing else aliases them during the test.
            unsafe {
                core::ptr::copy_nonoverlapping(request.as_ptr(), ptr.as_ptr(), request.len());
            }
        }
        let mut space = AddressSpace::new(HostPageTable::new());
        space
            .map(
                page(1),
                Frame(CALL_FRAME),
                MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
            )
            .expect("map request page");
        space
            .map(
                page(2),
                Frame(CALL_FRAME + 1),
                MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
            )
            .expect("map reply page");
        (Box::new(space), Box::new(sim))
    }

    /// Read `len` bytes the handler copied out to page 2 (`0x2000`).
    fn read_reply_page(physmap: &dyn PhysMap, len: usize) -> alloc::vec::Vec<u8> {
        let reply_base = PhysAddr::new((CALL_FRAME as u64 + 1) * PAGE_SIZE as u64);
        let ptr = physmap.translate(reply_base, len).expect("reply window");
        let mut out = alloc::vec![0u8; len];
        // SAFETY: the window owns these bytes for the simulator's lifetime
        // and the handler has already finished writing them.
        unsafe {
            core::ptr::copy_nonoverlapping(ptr.as_ptr(), out.as_mut_ptr(), len);
        }
        out
    }

    /// Register an unrestricted call endpoint in the global registry under
    /// `id` and return it; the test must `crate::callreg::unregister(id)`
    /// when done so the global registry does not leak across tests.
    fn register_call_endpoint(id: u64, sink: &(dyn Sink + Sync)) -> Arc<CallEndpoint> {
        let creator = make_caps_record(1, &[], sink);
        let ep = Arc::new(
            CallEndpoint::create(
                EndpointId(id),
                &creator,
                CapabilitySet::empty(),
                CapabilitySet::empty(),
                CallEndpointLimits {
                    max_request: 128,
                    max_reply: 128,
                    capacity: 4,
                },
                sink,
            )
            .expect("unrestricted endpoint"),
        );
        crate::callreg::register(ep.clone()).expect("registered");
        ep
    }

    /// `ipc_call` to an unregistered endpoint fails closed with `NotFound`
    /// without touching the caller's buffers.
    #[test]
    fn ipc_call_to_unregistered_endpoint_is_not_found() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        // Endpoint id 0xDEAD0001 was never registered.
        assert_eq!(
            h.ipc_call(&ctx, 0xDEAD_0001, 0x1000, 4, 0x2000, 64),
            Err(Errno::NotFound)
        );
    }

    /// `ipc_call` with a request larger than the endpoint advertises fails
    /// closed with `MessageTooLarge` before any copy.
    #[test]
    fn ipc_call_oversize_request_is_message_too_large() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let id = 0xCA11_1001;
        let _ep = register_call_endpoint(id, sink);
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        // max_request is 128; ask to post 200 bytes.
        assert_eq!(
            h.ipc_call(&ctx, id, 0x1000, 200, 0x2000, 64),
            Err(Errno::MessageTooLarge)
        );
        crate::callreg::unregister(EndpointId(id));
    }

    /// `ipc_call` from a caller with no registered address space fails
    /// closed with `BadAddress` during the request copy-in — it never reaches the post/park.
    #[test]
    fn ipc_call_without_registered_aspace_is_bad_address() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let id = 0xCA11_1002;
        let _ep = register_call_endpoint(id, sink);
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.ipc_call(&ctx, id, 0x1000, 4, 0x2000, 64),
            Err(Errno::BadAddress)
        );
        crate::callreg::unregister(EndpointId(id));
    }

    /// `ipc_call` against a restricted-sender endpoint the caller lacks the
    /// send capability for fails closed with `PermissionDenied` at post
    /// time — after the request copy-in, before any reply.
    #[test]
    fn ipc_call_without_send_capability_is_permission_denied() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = call_aspace(b"ping");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink); // lacks NET_RAW
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // A restricted-sender endpoint requiring NET_RAW; its creator holds
        // IPC_BIND_PRIVILEGED (required to bind a restricted endpoint).
        let id = 0xCA11_1003;
        let binder = make_caps_record(
            1,
            &[CapabilityId::IPC_BIND_PRIVILEGED, CapabilityId::NET_RAW],
            sink,
        );
        let ep = Arc::new(
            CallEndpoint::create(
                EndpointId(id),
                &binder,
                caps_with(&[CapabilityId::NET_RAW]),
                CapabilitySet::empty(),
                CallEndpointLimits {
                    max_request: 128,
                    max_reply: 128,
                    capacity: 4,
                },
                sink,
            )
            .expect("restricted endpoint"),
        );
        crate::callreg::register(ep).expect("registered");

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.ipc_call(&ctx, id, 0x1000, 4, 0x2000, 64),
            Err(Errno::PermissionDenied)
        );
        crate::callreg::unregister(EndpointId(id));
    }

    /// End-to-end: `ipc_call` posts the request, parks the caller, and —
    /// once a server drains the call and replies — copies the reply out to
    /// the caller's buffer and returns its length. A background thread
    /// plays the server so the parked caller is woken with a real reply.
    #[test]
    fn ipc_call_round_trips_request_and_reply() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        // A live scheduler task so the park loop's cooperative-yield
        // fallback returns `InvalidState` (the task is Ready, not Running)
        // and keeps polling, rather than `NoSuchTask`.
        let tid = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn caller task");
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = call_aspace(b"ping");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(tid), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(tid, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(tid),
            caps: &caps,
        };

        let id = 0xCA11_2001;
        let ep = register_call_endpoint(id, sink);

        // The server: drain the one posted call and answer it.
        let server_ep = ep.clone();
        let handle = std::thread::spawn(move || loop {
            if let RecvCall::Received(call) = server_ep.recv_call(usize::MAX) {
                assert_eq!(call.request, b"ping");
                server_ep
                    .reply(call.ticket, b"pong", &TestSink::new())
                    .expect("reply");
                break;
            }
            std::thread::yield_now();
        });

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let written = h
            .ipc_call(&ctx, id, 0x1000, 4, 0x2000, 64)
            .expect("call completes");
        handle.join().expect("server thread joins");
        assert_eq!(written, 4);

        // The reply landed in the caller's page-2 buffer.
        let guard = aspaces.read();
        let (_space, physmap) = guard.resolve(SecTaskId(tid)).expect("aspace present");
        assert_eq!(read_reply_page(physmap, 4), b"pong");
        crate::callreg::unregister(EndpointId(id));
    }

    /// A reply larger than the caller's buffer fails closed with
    /// `BufferTooSmall` rather than truncating.
    #[test]
    fn ipc_call_reply_larger_than_buffer_is_buffer_too_small() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let tid = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn caller task");
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = call_aspace(b"ping");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(tid), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(tid, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(tid),
            caps: &caps,
        };

        let id = 0xCA11_2002;
        let ep = register_call_endpoint(id, sink);
        let server_ep = ep.clone();
        let handle = std::thread::spawn(move || loop {
            if let RecvCall::Received(call) = server_ep.recv_call(usize::MAX) {
                server_ep
                    .reply(call.ticket, b"a long reply", &TestSink::new())
                    .expect("reply");
                break;
            }
            std::thread::yield_now();
        });

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        // The reply is 12 bytes; the caller offers only 4.
        let result = h.ipc_call(&ctx, id, 0x1000, 4, 0x2000, 4);
        handle.join().expect("server thread joins");
        assert_eq!(result, Err(Errno::BufferTooSmall));
        crate::callreg::unregister(EndpointId(id));
    }

    // ---- call_create / call_recv / call_reply (server side) ----------

    /// First frame of the three-page server address space the `call_recv` /
    /// `call_reply` tests use: page 1 (`0x1000`) receives the request, page 2
    /// (`0x2000`) receives the ticket, page 3 (`0x3000`) holds the reply
    /// payload. Disjoint from [`CALL_FRAME`].
    const SERVER_FRAME: usize = 30;

    /// Build a server address space mapping pages 1–3 (`0x1000`/`0x2000`/
    /// `0x3000`) `RW|USER` onto three contiguous frames, seeding `reply` at
    /// the start of page 3 (the buffer `call_reply` reads from).
    fn server_aspace(
        reply: &[u8],
    ) -> (
        Box<dyn UserAddressSpace + Send + Sync>,
        Box<dyn PhysMap + Send + Sync>,
    ) {
        let base = PhysAddr::new(SERVER_FRAME as u64 * PAGE_SIZE as u64);
        let sim = SimPhysMap::new(base, PAGE_SIZE * 3);
        if !reply.is_empty() {
            let reply_base = PhysAddr::new((SERVER_FRAME as u64 + 2) * PAGE_SIZE as u64);
            let ptr = sim.translate(reply_base, reply.len()).expect("seed reply");
            // SAFETY: the window owns these bytes for the simulator's lifetime
            // and nothing else aliases them during the test.
            unsafe {
                core::ptr::copy_nonoverlapping(reply.as_ptr(), ptr.as_ptr(), reply.len());
            }
        }
        let mut space = AddressSpace::new(HostPageTable::new());
        for p in 0usize..3 {
            space
                .map(
                    page(1 + p as u64),
                    Frame(SERVER_FRAME + p),
                    MapFlags::READ | MapFlags::WRITE | MapFlags::USER,
                )
                .expect("map server page");
        }
        (Box::new(space), Box::new(sim))
    }

    /// Read `len` bytes the handler copied to page `n` (1-based) of a
    /// [`server_aspace`].
    fn read_server_page(physmap: &dyn PhysMap, n: usize, len: usize) -> alloc::vec::Vec<u8> {
        let base = PhysAddr::new((SERVER_FRAME as u64 + (n as u64 - 1)) * PAGE_SIZE as u64);
        let ptr = physmap.translate(base, len).expect("server window");
        let mut out = alloc::vec![0u8; len];
        // SAFETY: the window owns these bytes for the simulator's lifetime and
        // the handler has finished writing them.
        unsafe {
            core::ptr::copy_nonoverlapping(ptr.as_ptr(), out.as_mut_ptr(), len);
        }
        out
    }

    /// `call_create` binds an unrestricted endpoint the `ipc_call` handler can
    /// then resolve, and refuses to re-point a live id.
    #[test]
    fn call_create_registers_a_resolvable_endpoint_and_refuses_a_clash() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        // Pages 1 and 2 are the (empty) send/recv `CapabilitySet` wire images.
        let (space, physmap) = call_aspace(&[0u8; CapabilitySet::WIRE_LEN]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(5), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(5, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(5),
            caps: &caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );

        let id = 0xCA11_3001;
        assert!(!crate::callreg::contains(EndpointId(id)));
        assert_eq!(h.call_create(&ctx, id, 0x1000, 0x2000, 64, 64, 4), Ok(0));
        assert!(crate::callreg::contains(EndpointId(id)));
        // A second bind of the same live id fails closed.
        assert_eq!(
            h.call_create(&ctx, id, 0x1000, 0x2000, 64, 64, 4),
            Err(Errno::AlreadyExists)
        );
        crate::callreg::unregister(EndpointId(id));
    }

    /// Wire image of a one-capability send set, for seeding a `call_create`
    /// request page.
    fn one_cap_image(cap: CapabilityId) -> [u8; CapabilitySet::WIRE_LEN] {
        let mut set = CapabilitySet::empty();
        set.insert(cap);
        set.to_le_bytes()
    }

    /// Creating a grant-restricted endpoint (its required send capability is
    /// `CAP_IPC_ENDPOINT`) mints the creator the matching per-endpoint grant,
    /// so it may forward the endpoint onto a node it publishes and the
    /// autoloaded class driver inherits it — exactly as `msi_alloc` grants an
    /// allocated IRQ line.
    #[test]
    fn call_create_grant_restricted_mints_the_endpoint_grant() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        // Page 1 is the send-set image {CAP_IPC_ENDPOINT}; page 2 (recv) is
        // the zeroed empty set.
        let (space, physmap) = call_aspace(&one_cap_image(CapabilityId::IPC_ENDPOINT));
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(5), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        // A restricted-sender endpoint requires CAP_IPC_BIND_PRIVILEGED to bind.
        let caps = make_caps_record(5, &[CapabilityId::IPC_BIND_PRIVILEGED], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(5),
            caps: &caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );

        let id = 0xCA11_4001;
        // No grant for the endpoint before creation.
        assert!(!aspaces
            .read()
            .grant_covers(SecTaskId(5), &rustos_abi::HwResource::endpoint(id)));
        assert_eq!(h.call_create(&ctx, id, 0x1000, 0x2000, 64, 64, 4), Ok(0));
        // The creator now holds the per-endpoint grant for exactly this id.
        assert!(aspaces
            .read()
            .grant_covers(SecTaskId(5), &rustos_abi::HwResource::endpoint(id)));
        // …and not for a neighbouring id (the grant is scoped to one endpoint).
        assert!(!aspaces
            .read()
            .grant_covers(SecTaskId(5), &rustos_abi::HwResource::endpoint(id + 1)));
        crate::callreg::unregister(EndpointId(id));
    }

    /// Register a grant-restricted endpoint (required send cap
    /// `CAP_IPC_ENDPOINT`) owned by task 1, returning it for unregistration.
    fn register_grant_restricted_endpoint(id: u64, sink: &(dyn Sink + Sync)) -> Arc<CallEndpoint> {
        let creator = make_caps_record(1, &[CapabilityId::IPC_BIND_PRIVILEGED], sink);
        let mut send = CapabilitySet::empty();
        send.insert(CapabilityId::IPC_ENDPOINT);
        let ep = Arc::new(
            CallEndpoint::create(
                EndpointId(id),
                &creator,
                send,
                CapabilitySet::empty(),
                CallEndpointLimits {
                    max_request: 128,
                    max_reply: 128,
                    capacity: 4,
                },
                sink,
            )
            .expect("grant-restricted endpoint"),
        );
        crate::callreg::register(ep.clone()).expect("registered");
        ep
    }

    /// `ipc_call` to a grant-restricted endpoint by a caller that holds the
    /// class capability but **not** the per-endpoint grant fails closed with
    /// `PermissionDenied`, before any buffer is copied — so one class driver
    /// cannot reach another's transport endpoint.
    #[test]
    fn ipc_call_grant_restricted_without_grant_is_permission_denied() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        // Caller holds the class capability but was never granted the endpoint.
        let caps = make_caps_record(2, &[CapabilityId::IPC_ENDPOINT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let id = 0xCA11_4002;
        let _ep = register_grant_restricted_endpoint(id, sink);
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.ipc_call(&ctx, id, 0x1000, 4, 0x2000, 64),
            Err(Errno::PermissionDenied)
        );
        crate::callreg::unregister(EndpointId(id));
    }

    /// `ipc_call` to a grant-restricted endpoint by a caller that holds both
    /// the class capability and the per-endpoint grant round-trips normally —
    /// the grant is exactly what the autoloaded class driver inherits from its
    /// matched node.
    #[test]
    fn ipc_call_grant_restricted_with_grant_round_trips() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let tid = sched
            .spawn(0, Priority::Normal, |_| TaskAction::Exit)
            .expect("spawn caller task");
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = call_aspace(b"ping");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(tid), space, physmap)
            .expect("registration succeeds");
        let id = 0xCA11_4003;
        // The caller was granted exactly this endpoint (as an autoloaded class
        // driver would inherit it from its matched node).
        aspaces
            .write()
            .mint_grant(SecTaskId(tid), rustos_abi::HwResource::endpoint(id));
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(tid, &[CapabilityId::IPC_ENDPOINT], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(tid),
            caps: &caps,
        };
        let ep = register_grant_restricted_endpoint(id, sink);
        let server_ep = ep.clone();
        let handle = std::thread::spawn(move || loop {
            if let RecvCall::Received(call) = server_ep.recv_call(usize::MAX) {
                assert_eq!(call.request, b"ping");
                server_ep
                    .reply(call.ticket, b"pong", &TestSink::new())
                    .expect("reply");
                break;
            }
            std::thread::yield_now();
        });
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let written = h
            .ipc_call(&ctx, id, 0x1000, 4, 0x2000, 64)
            .expect("granted call completes");
        handle.join().expect("server thread joins");
        assert_eq!(written, 4);
        crate::callreg::unregister(EndpointId(id));
    }

    /// A recording [`crate::devres::SharedMemFacility`] double: it hands out
    /// a fixed physical base + VA and records the calls, so a handler test can
    /// assert `shm_create` mapped and granted without wiring a real
    /// `kernel/mem` producer or a published live space.
    struct RecordingSharedFacility {
        va: u64,
    }
    impl crate::devres::SharedMemFacility for RecordingSharedFacility {
        fn alloc_region(&self, _pages: u64) -> Result<(u64, u32), Errno> {
            Ok((0xAB00_0000, 0))
        }
        fn map_region(&self, _phys_base: u64, _pages: u64) -> Result<u64, Errno> {
            Ok(self.va)
        }
        fn unmap_region(&self, _base: u64, _len: usize) -> Result<(), Errno> {
            Ok(())
        }
        fn free_region(&self, _phys_base: u64, _order: u32, _pages: u64) {}
    }

    /// `shm_create` maps the region into the caller, mints the caller the
    /// per-region `HwResource::shared(id)` grant (so it can forward it onto a
    /// node it publishes), and writes the kernel-minted id out to `id_out`.
    #[test]
    fn shm_create_mints_the_region_grant_and_writes_the_id() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        // A two-page caller space; `id_out` is page 2 (`0x2000`) so the test
        // reads the written id back with `read_reply_page`.
        let (space, physmap) = call_aspace(b"");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(7), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(7, &[CapabilityId::SHM], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &caps,
        };
        let facility: &'static RecordingSharedFacility =
            Box::leak(Box::new(RecordingSharedFacility { va: 0x2_0000_1000 }));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_shared_mem_facility(facility);

        let va = h
            .shm_create(&ctx, 4096, 0x2000)
            .expect("shm_create succeeds");
        assert_eq!(
            va, 0x2_0000_1000,
            "the mapped base flows back to the caller"
        );
        // The kernel-minted region id was written to `id_out` (page 2).
        let id_bytes = read_reply_page(
            aspaces.read().resolve(SecTaskId(7)).expect("registered").1,
            8,
        );
        let id = u64::from_le_bytes(id_bytes.try_into().expect("8 bytes"));
        // The caller now holds the per-region grant for exactly that id, so it
        // can forward the region onto a node it emits; a neighbouring id is
        // not covered (the grant is scoped to one region).
        assert!(aspaces
            .read()
            .grant_covers(SecTaskId(7), &rustos_abi::HwResource::shared(id)));
        assert!(!aspaces
            .read()
            .grant_covers(SecTaskId(7), &rustos_abi::HwResource::shared(id + 1)));
        // Cleanup so the global region registry does not leak across tests.
        let _ = crate::sharedreg::unmap(facility, SecTaskId(7), va);
    }

    /// `shm_map` fails closed for a forged handle (`NotFound`) and for a grant
    /// of the wrong kind (`OutOfRange`) before mapping anything.
    #[test]
    fn shm_map_fails_closed_for_forged_and_wrong_kind_grants() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        // No registered aspace needed: both failures are decided from the
        // grant table before any region is mapped or any buffer touched.
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(8, &[CapabilityId::SHM], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(8),
            caps: &caps,
        };
        let facility: &'static RecordingSharedFacility =
            Box::leak(Box::new(RecordingSharedFacility { va: 0x2_0000_2000 }));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_shared_mem_facility(facility);

        // A handle the task was never granted resolves to nothing.
        assert_eq!(h.shm_map(&ctx, 0x9999), Err(Errno::NotFound));
        // A grant of the wrong kind (an IRQ line) is refused before mapping.
        let handle = aspaces
            .write()
            .mint_grant(SecTaskId(8), rustos_abi::HwResource::irq(33, 1));
        assert_eq!(h.shm_map(&ctx, handle), Err(Errno::OutOfRange));
    }

    /// `shm_create` on a build with no shared-memory facility wired fails
    /// closed with `NotImplemented`, never fabricating a region.
    #[test]
    fn shm_create_without_facility_is_not_implemented() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(9, &[CapabilityId::SHM], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(9),
            caps: &caps,
        };
        // No `with_shared_mem_facility`: the handler holds the fail-closed
        // `NULL_SHARED_MEM_FACILITY`.
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(h.shm_create(&ctx, 4096, 0x2000), Err(Errno::NotImplemented));
        // A zero-length region is rejected before the facility is consulted.
        assert_eq!(h.shm_create(&ctx, 0, 0x2000), Err(Errno::LengthOutOfRange));
    }

    /// Wire constants for the wait-set control arguments, named so the tests
    /// read as the syscall's caller would write them.
    const WS_OP_ADD: u32 = rustos_abi::WaitSetOp::Add as u32;
    const WS_OP_DEL: u32 = rustos_abi::WaitSetOp::Del as u32;
    const WS_KIND_ENDPOINT: u32 = rustos_abi::WaitSourceKind::Endpoint as u32;
    const WS_KIND_IRQ: u32 = rustos_abi::WaitSourceKind::Irq as u32;

    /// `waitset_create` mints a handle, and `waitset_ctl(Add)` owner-checks the
    /// named resource against the caller before recording it: a bound IRQ line
    /// is accepted, a forged IRQ handle and an unknown endpoint are refused, a
    /// duplicate `(kind,id)` is refused, and `Del` round-trips. A forged set
    /// handle and an unknown op/kind all fail closed.
    #[test]
    fn waitset_create_and_ctl_owner_check_resources() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(8, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(8),
            caps: &caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );

        let set = h.waitset_create(&ctx).expect("create mints a handle");
        assert!(set != 0, "handle is non-zero");

        // A line the caller bound is an acceptable IRQ member.
        let line = h.irq_bind(&ctx, 5).expect("bind");
        assert_eq!(
            h.waitset_ctl(&ctx, set, WS_OP_ADD, WS_KIND_IRQ, line, 0xAA),
            Ok(0)
        );
        // A forged IRQ handle the caller never bound is refused.
        assert_eq!(
            h.waitset_ctl(&ctx, set, WS_OP_ADD, WS_KIND_IRQ, 0xDEAD_BEEF, 0xBB),
            Err(Errno::NotFound)
        );
        // An unknown endpoint id is refused (the caller serves none).
        assert_eq!(
            h.waitset_ctl(&ctx, set, WS_OP_ADD, WS_KIND_ENDPOINT, 0x1234, 0xCC),
            Err(Errno::NotFound)
        );
        // A duplicate `(kind,id)` is refused even with a different token.
        assert_eq!(
            h.waitset_ctl(&ctx, set, WS_OP_ADD, WS_KIND_IRQ, line, 0x99),
            Err(Errno::AlreadyExists)
        );
        // `Del` removes the member; a second `Del` fails closed.
        assert_eq!(
            h.waitset_ctl(&ctx, set, WS_OP_DEL, WS_KIND_IRQ, line, 0),
            Ok(0)
        );
        assert_eq!(
            h.waitset_ctl(&ctx, set, WS_OP_DEL, WS_KIND_IRQ, line, 0),
            Err(Errno::NotFound)
        );
        // A handle that is not the caller's own wait-set fails closed.
        assert_eq!(
            h.waitset_ctl(&ctx, set + 999, WS_OP_ADD, WS_KIND_IRQ, line, 0),
            Err(Errno::NotFound)
        );
        // Unknown op / kind are rejected before any state is touched.
        assert_eq!(
            h.waitset_ctl(&ctx, set, 7, WS_KIND_IRQ, line, 0),
            Err(Errno::OutOfRange)
        );
        assert_eq!(
            h.waitset_ctl(&ctx, set, WS_OP_ADD, 7, line, 0),
            Err(Errno::OutOfRange)
        );

        // Cleanup so the global wait-set registry does not leak across tests.
        assert_eq!(crate::waitset::release_owned_by(8), 1);
    }

    /// `waitset_wait` with no member ready and a zero timeout returns
    /// `TimedOut` without writing `token_out` (so it needs no mapped page).
    #[test]
    fn waitset_wait_times_out_when_no_member_ready() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(0x5701, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(0x5701),
            caps: &caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let set = h.waitset_create(&ctx).expect("create");
        let line = h.irq_bind(&ctx, 5).expect("bind");
        h.waitset_ctl(&ctx, set, WS_OP_ADD, WS_KIND_IRQ, line, 0xAA)
            .expect("add irq member");
        // The line has not fired: a zero-timeout wait expires without writing.
        assert_eq!(h.waitset_wait(&ctx, set, 0, 0x2000), Err(Errno::TimedOut));
        assert_eq!(crate::waitset::release_owned_by(0x5701), 1);
    }

    /// A fired IRQ member makes `waitset_wait` report that member's token and
    /// consume the edge: the token is written to `token_out`, and a second
    /// wait times out (the fire was consumed, exactly like `irq_wait`).
    #[test]
    fn waitset_wait_reports_a_fired_irq_member_and_consumes_the_edge() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = call_aspace(b"");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(7), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let permissive = PermissiveController;
        let caps = make_caps_record(7, &[CapabilityId::IRQ_BIND], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(7),
            caps: &caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let set = h.waitset_create(&ctx).expect("create");
        let line = h.irq_bind(&ctx, 5).expect("bind");
        h.waitset_ctl(&ctx, set, WS_OP_ADD, WS_KIND_IRQ, line, 0x1234)
            .expect("add irq member");

        // Fire the line externally (the arch trap path would do this); the
        // ready flag is then set.
        irq.fire(5, &permissive).expect("fire");
        assert_eq!(h.waitset_wait(&ctx, set, 0, 0x2000), Ok(0));
        // The ready member's token was written to `token_out` (page 2).
        let token_bytes = read_reply_page(
            aspaces.read().resolve(SecTaskId(7)).expect("registered").1,
            8,
        );
        assert_eq!(
            u64::from_le_bytes(token_bytes.try_into().expect("8 bytes")),
            0x1234
        );
        // The edge was consumed: a second wait with no new fire times out.
        assert_eq!(h.waitset_wait(&ctx, set, 0, 0x2000), Err(Errno::TimedOut));
        assert_eq!(crate::waitset::release_owned_by(7), 1);
    }

    /// A pending request on a member endpoint makes `waitset_wait` report that
    /// member's token (a non-consuming peek), and `call_recv` is what drains
    /// it: after draining, a second wait times out.
    #[test]
    fn waitset_wait_reports_a_pending_endpoint_member_drained_by_recv() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = call_aspace(b"");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(0x5702), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(0x5702, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(0x5702),
            caps: &caps,
        };

        // An unrestricted endpoint owned by the caller (task 7), with a posted
        // request waiting to be received (so `has_pending()` is true).
        let id = 0xCA11_5001;
        let creator = make_caps_record(0x5702, &[], sink);
        let ep = Arc::new(
            CallEndpoint::create(
                EndpointId(id),
                &creator,
                CapabilitySet::empty(),
                CapabilitySet::empty(),
                CallEndpointLimits {
                    max_request: 64,
                    max_reply: 64,
                    capacity: 4,
                },
                sink,
            )
            .expect("endpoint"),
        );
        crate::callreg::register(ep.clone()).expect("registered");
        let poster = make_caps_record(99, &[], sink);
        ep.post(&poster, b"x", sink).expect("post a request");

        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        let set = h.waitset_create(&ctx).expect("create");
        h.waitset_ctl(&ctx, set, WS_OP_ADD, WS_KIND_ENDPOINT, id, 0x77)
            .expect("add endpoint member");

        // The pending request makes the endpoint member ready; its token is
        // reported.
        assert_eq!(h.waitset_wait(&ctx, set, 0, 0x2000), Ok(0));
        let token_bytes = read_reply_page(
            aspaces
                .read()
                .resolve(SecTaskId(0x5702))
                .expect("registered")
                .1,
            8,
        );
        assert_eq!(
            u64::from_le_bytes(token_bytes.try_into().expect("8 bytes")),
            0x77
        );
        // The wait did not consume the request (it only peeked); draining it
        // with `recv_call` is what clears readiness, after which a second wait
        // times out.
        assert!(matches!(ep.recv_call(usize::MAX), RecvCall::Received(_)));
        assert_eq!(h.waitset_wait(&ctx, set, 0, 0x2000), Err(Errno::TimedOut));

        crate::callreg::unregister(EndpointId(id));
        assert_eq!(crate::waitset::release_owned_by(0x5702), 1);
    }

    /// `call_recv` / `call_reply` against an unbound id fail closed with
    /// `NotFound` before touching any buffer.
    #[test]
    fn call_recv_and_reply_on_unknown_endpoint_are_not_found() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.call_recv(&ctx, 0xDEAD_3001, 0x1000, 64, 0x2000),
            Err(Errno::NotFound)
        );
        assert_eq!(
            h.call_reply(&ctx, 0xDEAD_3001, 1, 0x1000, 4),
            Err(Errno::NotFound)
        );
    }

    /// A server lacking the endpoint's required receive capability is denied
    /// at `call_recv` before any state is touched.
    #[test]
    fn call_recv_without_required_recv_cap_is_permission_denied() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;

        // The endpoint requires CAP_AUDIT_READ to serve; its creator holds it.
        let id = 0xCA11_3002;
        let creator = make_caps_record(1, &[CapabilityId::AUDIT_READ], sink);
        let mut recv_caps = CapabilitySet::empty();
        recv_caps.insert(CapabilityId::AUDIT_READ);
        let ep = Arc::new(
            CallEndpoint::create(
                EndpointId(id),
                &creator,
                CapabilitySet::empty(),
                recv_caps,
                CallEndpointLimits {
                    max_request: 64,
                    max_reply: 64,
                    capacity: 4,
                },
                sink,
            )
            .expect("restricted-recv endpoint"),
        );
        crate::callreg::register(ep).expect("registered");

        // The caller (task 2) does not hold CAP_AUDIT_READ.
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.call_recv(&ctx, id, 0x1000, 64, 0x2000),
            Err(Errno::PermissionDenied)
        );
        crate::callreg::unregister(EndpointId(id));
    }

    /// End-to-end server side: a posted request is delivered by `call_recv`
    /// (request bytes + ticket), and `call_reply` completes it for the
    /// client. The serving task is the endpoint owner.
    #[test]
    fn call_recv_and_reply_round_trip_the_server_side() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = server_aspace(b"pong");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        // The server task owns the endpoint, so the aspace and the owner id
        // are the same task (id 9).
        aspaces
            .write()
            .register(SecTaskId(9), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;

        // Build the endpoint owned by the server task (9).
        let id = 0xCA11_3003;
        let server_caps = make_caps_record(9, &[], sink);
        let ep = Arc::new(
            CallEndpoint::create(
                EndpointId(id),
                &server_caps,
                CapabilitySet::empty(),
                CapabilitySet::empty(),
                CallEndpointLimits {
                    max_request: 64,
                    max_reply: 64,
                    capacity: 4,
                },
                sink,
            )
            .expect("unrestricted endpoint"),
        );
        crate::callreg::register(ep.clone()).expect("registered");

        // A client (task 7) posts a request, awaiting its reply.
        let client_caps = make_caps_record(7, &[], sink);
        let ticket = ep.post(&client_caps, b"ping", sink).expect("posted");

        let ctx = CallerContext {
            task_id: SecTaskId(9),
            caps: &server_caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );

        // `call_recv` delivers the request to page 1 and the ticket to page 2.
        let got = h.call_recv(&ctx, id, 0x1000, 64, 0x2000).expect("received");
        assert_eq!(got, 4);
        let guard = aspaces.read();
        let (_space, physmap) = guard.resolve(SecTaskId(9)).expect("aspace present");
        assert_eq!(read_server_page(physmap, 1, 4), b"ping");
        let ticket_bytes = read_server_page(physmap, 2, 8);
        let recv_ticket = u64::from_le_bytes(ticket_bytes.try_into().expect("8 bytes"));
        assert_eq!(recv_ticket, ticket.0);
        drop(guard);

        // `call_reply` sends page 3's payload back and completes the ticket.
        assert_eq!(h.call_reply(&ctx, id, recv_ticket, 0x3000, 4), Ok(0));
        // The client claims its reply exactly once.
        assert_eq!(
            ep.take_reply(7, CallTicket(recv_ticket)),
            ReplyOutcome::Ready(b"pong".to_vec())
        );
        crate::callreg::unregister(EndpointId(id));
    }

    /// `call_peer_origin` hands the server the kernel-attested identity of
    /// the caller it is servicing, immune to the request payload, and fails
    /// closed for a foreign reader, a short buffer, or a ticket not in
    /// service.
    #[test]
    fn call_peer_origin_attests_the_caller_and_fails_closed() {
        use rustos_abi::{Origin, ProcId, TrustDomain, ORIGIN_WIRE_LEN};
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = server_aspace(b"unused");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        // Server task 9 owns both the aspace and the endpoint.
        aspaces
            .write()
            .register(SecTaskId(9), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;

        let id = 0xCA11_4004;
        let server_caps = make_caps_record(9, &[], sink);
        let ep = Arc::new(
            CallEndpoint::create(
                EndpointId(id),
                &server_caps,
                CapabilitySet::empty(),
                CapabilitySet::empty(),
                CallEndpointLimits {
                    max_request: 64,
                    max_reply: 64,
                    capacity: 4,
                },
                sink,
            )
            .expect("unrestricted endpoint"),
        );
        crate::callreg::register(ep.clone()).expect("registered");

        // A client (task 7) with a minted process instance and a real
        // capability posts a request.
        let client_caps = make_caps_record(7, &[CapabilityId::SYSINFO_GLOBAL], sink)
            .with_proc_id(ProcId::from_raw([0x71; 16]));
        let expected = client_caps.attest_origin();
        let ticket = ep.post(&client_caps, b"who-am-i", sink).expect("posted");

        let ctx = CallerContext {
            task_id: SecTaskId(9),
            caps: &server_caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );

        // Before the server receives the call, the ticket is not in service,
        // so the origin is not yet readable (fail closed).
        assert_eq!(
            h.call_peer_origin(&ctx, id, ticket.0, 0x1000, 64),
            Err(Errno::NotFound)
        );

        // Receive the call, moving it into service.
        let got = h.call_recv(&ctx, id, 0x2000, 64, 0x3000).expect("received");
        assert_eq!(got, 8);

        // A foreign reader (task 8, not the owner) is denied before any
        // state is touched.
        let foreign_caps = make_caps_record(8, &[], sink);
        let foreign_ctx = CallerContext {
            task_id: SecTaskId(8),
            caps: &foreign_caps,
        };
        assert_eq!(
            h.call_peer_origin(&foreign_ctx, id, ticket.0, 0x1000, 64),
            Err(Errno::PermissionDenied)
        );

        // A buffer too small for a whole origin fails closed.
        assert_eq!(
            h.call_peer_origin(&ctx, id, ticket.0, 0x1000, ORIGIN_WIRE_LEN - 1),
            Err(Errno::BufferTooSmall)
        );

        // An unknown ticket fails closed.
        assert_eq!(
            h.call_peer_origin(&ctx, id, 0xDEAD, 0x1000, 64),
            Err(Errno::NotFound)
        );

        // The happy path writes the attested origin to page 1.
        let wrote = h
            .call_peer_origin(&ctx, id, ticket.0, 0x1000, 64)
            .expect("origin written");
        assert_eq!(wrote, ORIGIN_WIRE_LEN as u64);

        let guard = aspaces.read();
        let (_space, physmap) = guard.resolve(SecTaskId(9)).expect("aspace present");
        let bytes = read_server_page(physmap, 1, ORIGIN_WIRE_LEN);
        let decoded = Origin::from_bytes(&bytes).expect("valid origin");
        drop(guard);

        // It is exactly what the client's own record attests — proving the
        // server reads kernel-attested state, never the request payload.
        assert_eq!(decoded, expected);
        assert_eq!(decoded.trust_domain(), TrustDomain::User);
        assert_eq!(decoded.uid(), 1000);
        assert_eq!(decoded.pid(), 7);
        assert_eq!(decoded.proc_id(), ProcId::from_raw([0x71; 16]));
        assert!(decoded
            .capabilities()
            .holds_cap(CapabilityId::SYSINFO_GLOBAL));

        crate::callreg::unregister(EndpointId(id));
    }

    /// `wall_time_get` / `wall_time_set` round-trip the kernel wall clock
    /// through the real handlers and the address-space copy paths, and fail
    /// closed on a short buffer, a non-settable state, and a malformed
    /// instant (P-D).
    #[test]
    fn wall_time_get_and_set_round_trip_and_fail_closed() {
        use rustos_abi::{Time64, WallClockReading, WallTimeState};
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        // Seed page 3 (`0x3000`) with the `Time64` the set path copies in.
        // Nanos are 0 so the few-ns monotonic elapsed a later read adds never
        // carries into the seconds, keeping the assertion deterministic.
        let wall = Time64::from_secs(1_700_000_000);
        let (space, physmap) = server_aspace(&wall.to_le_bytes());
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(9), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(9, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(9),
            caps: &caps,
        };
        // The boot path leaks the production clock; tests do the same so the
        // `'static` builder is satisfied and a set persists across reads.
        let clock: &'static crate::wallclock::KernelWallClock =
            Box::leak(Box::new(crate::wallclock::KernelWallClock::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_wall_clock(clock);

        // A buffer too small for a whole reading fails closed.
        assert_eq!(
            h.wall_time_get(&ctx, 0x1000, WallClockReading::WIRE_LEN - 1),
            Err(Errno::BufferTooSmall)
        );

        // Before any set, get reports the Unset epoch reading.
        let wrote = h.wall_time_get(&ctx, 0x1000, 64).expect("reading written");
        assert_eq!(wrote, WallClockReading::WIRE_LEN as u64);
        {
            let guard = aspaces.read();
            let (_s, pm) = guard.resolve(SecTaskId(9)).expect("aspace present");
            let bytes = read_server_page(pm, 1, WallClockReading::WIRE_LEN);
            let r = WallClockReading::from_bytes(&bytes).expect("valid reading");
            assert_eq!(r.state(), WallTimeState::Unset);
            assert_eq!(r.time(), Time64::UNIX_EPOCH);
        }

        // Setting to `Unset` is rejected (fail closed).
        assert_eq!(
            h.wall_time_set(
                &ctx,
                0x3000,
                Time64::WIRE_LEN,
                u32::from(WallTimeState::Unset.as_u8())
            ),
            Err(Errno::OutOfRange)
        );
        // An undefined state discriminant is rejected.
        assert_eq!(
            h.wall_time_set(&ctx, 0x3000, Time64::WIRE_LEN, 9),
            Err(Errno::OutOfRange)
        );
        // A short instant buffer fails closed.
        assert_eq!(
            h.wall_time_set(
                &ctx,
                0x3000,
                Time64::WIRE_LEN - 1,
                u32::from(WallTimeState::Trusted.as_u8())
            ),
            Err(Errno::BufferTooSmall)
        );

        // The happy path sets the clock to the seeded instant, Trusted.
        assert_eq!(
            h.wall_time_set(
                &ctx,
                0x3000,
                Time64::WIRE_LEN,
                u32::from(WallTimeState::Trusted.as_u8())
            ),
            Ok(0)
        );

        // Now get reflects the set instant and state. The seconds match
        // exactly; the few-ns monotonic elapsed only touches the nanos.
        let wrote = h.wall_time_get(&ctx, 0x1000, 64).expect("reading written");
        assert_eq!(wrote, WallClockReading::WIRE_LEN as u64);
        let guard = aspaces.read();
        let (_s, pm) = guard.resolve(SecTaskId(9)).expect("aspace present");
        let bytes = read_server_page(pm, 1, WallClockReading::WIRE_LEN);
        let r = WallClockReading::from_bytes(&bytes).expect("valid reading");
        assert_eq!(r.state(), WallTimeState::Trusted);
        assert_eq!(r.time().secs(), 1_700_000_000);
        assert!(r.time().subsec_nanos() < 1_000);
    }

    /// `boot_id_get` fails closed with `BufferTooSmall` for a buffer shorter
    /// than `BOOT_ID_LEN` and with `EntropyNotReady` while the boot id is
    /// unset — both *before* any address-space access, and even when a real id
    /// is installed (the short-buffer rejection is unconditional). Once a
    /// `BootId` is installed and the buffer is large enough the 16 bytes are
    /// copied out to the caller and the byte count returned.
    #[test]
    fn boot_id_get_fails_closed_then_copies_out_the_minted_id() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        // A writable caller page for the copy-out, registered for task 2.
        let (space, physmap) = send_aspace(MapFlags::WRITE | MapFlags::USER, &[]);
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };

        // Unset (default): a short buffer is rejected before anything else,
        // and a large-enough buffer fails closed `EntropyNotReady` rather than
        // ever emitting the all-zero sentinel.
        let unset = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            unset.boot_id_get(&ctx, 0x1000, 0),
            Err(Errno::BufferTooSmall)
        );
        assert_eq!(
            unset.boot_id_get(&ctx, 0x1000, BOOT_ID_LEN),
            Err(Errno::EntropyNotReady)
        );

        // With a real id installed: a short buffer is still rejected, and a
        // large-enough one copies the 16 bytes out and returns the count.
        let id = BootId::from_raw([0x5A; BOOT_ID_LEN]);
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_boot_id(id);
        assert_eq!(
            h.boot_id_get(&ctx, 0x1000, BOOT_ID_LEN - 1),
            Err(Errno::BufferTooSmall)
        );
        assert_eq!(
            h.boot_id_get(&ctx, 0x1000, BOOT_ID_LEN),
            Ok(BOOT_ID_LEN as u64)
        );
    }

    // --- filesystem syscalls (P-A) ----------------

    // `DirEntry`, `FileStat`, and `OpenFlags` are already in scope through
    // `use super::*`; only `FileKind` is additionally needed here.
    use rustos_abi::FileKind;

    /// A recording [`FilesystemService`] double: it logs each call's
    /// attested uid and arguments (so a test can prove the handler attested
    /// the caller and forwarded the right path/offset/append), serves
    /// canned read/readdir/stat results, and can be set to fail a chosen op
    /// closed. Recording goes through a [`std::sync::Mutex`] because the
    /// service must be `Send + Sync`.
    struct RecordingFs {
        read_data: Vec<u8>,
        entries: Vec<(FileKind, String)>,
        stat: FileStat,
        open_err: Option<Errno>,
        log: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingFs {
        fn new() -> Self {
            Self {
                read_data: Vec::new(),
                entries: Vec::new(),
                stat: FileStat {
                    kind: FileKind::Regular,
                    size: 0,
                    mode: 0o644,
                    uid: 1000,
                    gid: 1000,
                },
                open_err: None,
                log: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn record(&self, entry: alloc::string::String) {
            self.log.lock().expect("uncontended").push(entry);
        }

        fn calls(&self) -> Vec<String> {
            self.log.lock().expect("uncontended").clone()
        }
    }

    impl FilesystemService for RecordingFs {
        fn open(
            &self,
            uid: u32,
            _caps: &dyn rustos_abi::CapabilityQuery,
            path: &str,
            flags: OpenFlags,
        ) -> Result<(), Errno> {
            self.record(alloc::format!(
                "open uid={uid} path={path} flags={}",
                flags.bits()
            ));
            match self.open_err {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }

        fn read(
            &self,
            uid: u32,
            _caps: &dyn rustos_abi::CapabilityQuery,
            path: &str,
            offset: u64,
            buf: &mut [u8],
        ) -> Result<usize, Errno> {
            self.record(alloc::format!(
                "read uid={uid} path={path} off={offset} len={}",
                buf.len()
            ));
            let n = self.read_data.len().min(buf.len());
            buf[..n].copy_from_slice(&self.read_data[..n]);
            Ok(n)
        }

        fn write(
            &self,
            uid: u32,
            _caps: &dyn rustos_abi::CapabilityQuery,
            path: &str,
            offset: u64,
            append: bool,
            data: &[u8],
        ) -> Result<usize, Errno> {
            self.record(alloc::format!(
                "write uid={uid} path={path} off={offset} append={append} data={data:?}"
            ));
            Ok(data.len())
        }

        fn readdir(
            &self,
            uid: u32,
            _caps: &dyn rustos_abi::CapabilityQuery,
            path: &str,
        ) -> Result<Vec<(FileKind, String)>, Errno> {
            self.record(alloc::format!("readdir uid={uid} path={path}"));
            Ok(self.entries.clone())
        }

        fn stat(
            &self,
            uid: u32,
            _caps: &dyn rustos_abi::CapabilityQuery,
            path: &str,
        ) -> Result<FileStat, Errno> {
            self.record(alloc::format!("stat uid={uid} path={path}"));
            Ok(self.stat)
        }

        fn truncate(
            &self,
            uid: u32,
            _caps: &dyn rustos_abi::CapabilityQuery,
            path: &str,
            size: u64,
        ) -> Result<(), Errno> {
            self.record(alloc::format!("truncate uid={uid} path={path} size={size}"));
            Ok(())
        }

        fn sync(&self, uid: u32, _caps: &dyn rustos_abi::CapabilityQuery) -> Result<(), Errno> {
            self.record(alloc::format!("sync uid={uid}"));
            Ok(())
        }

        fn mkdir(
            &self,
            uid: u32,
            _caps: &dyn rustos_abi::CapabilityQuery,
            path: &str,
        ) -> Result<(), Errno> {
            self.record(alloc::format!("mkdir uid={uid} path={path}"));
            Ok(())
        }

        fn unlink(
            &self,
            uid: u32,
            _caps: &dyn rustos_abi::CapabilityQuery,
            path: &str,
        ) -> Result<(), Errno> {
            self.record(alloc::format!("unlink uid={uid} path={path}"));
            Ok(())
        }

        fn rename(
            &self,
            uid: u32,
            _caps: &dyn rustos_abi::CapabilityQuery,
            src: &str,
            dst: &str,
        ) -> Result<(), Errno> {
            self.record(alloc::format!("rename uid={uid} src={src} dst={dst}"));
            Ok(())
        }
    }

    /// `fs_open` resolves+authorises through the service under the caller's
    /// attested uid and records a descriptor at/above the standard streams.
    #[test]
    fn fs_open_attests_the_caller_and_allocates_a_descriptor() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, b"/System/Logs/a");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::FS_ACCESS], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let fs: &'static RecordingFs = Box::leak(Box::new(RecordingFs::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_filesystem(fs);

        let fd = h
            .fs_open(&ctx, 0x1000, "/System/Logs/a".len(), OpenFlags::READ)
            .expect("open succeeds");
        assert_eq!(fd, 4, "the first descriptor follows the standard streams");
        assert_eq!(
            fs.calls(),
            alloc::vec![alloc::string::String::from(
                "open uid=1000 path=/System/Logs/a flags=1"
            )],
            "the handler attested the caller's uid and forwarded the path/flags"
        );
    }

    /// With no filesystem wired the handler holds `NULL_FILESYSTEM` and
    /// `fs_open` fails closed with `NotImplemented`.
    #[test]
    fn fs_open_without_filesystem_is_not_implemented() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, b"/Storage/x");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::FS_ACCESS], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        );
        assert_eq!(
            h.fs_open(&ctx, 0x1000, "/Storage/x".len(), OpenFlags::READ),
            Err(Errno::NotImplemented)
        );
        // No descriptor was recorded for the caller.
        assert_eq!(aspaces.read().open_file_entry(SecTaskId(2), 4), None);
    }

    /// A refused open (e.g. exclusive-create clash) yields no descriptor.
    #[test]
    fn fs_open_refused_records_no_descriptor() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) = send_aspace(MapFlags::READ | MapFlags::USER, b"/Storage/x");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::FS_ACCESS], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let mut mock = RecordingFs::new();
        mock.open_err = Some(Errno::AlreadyExists);
        let fs: &'static RecordingFs = Box::leak(Box::new(mock));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_filesystem(fs);
        assert_eq!(
            h.fs_open(&ctx, 0x1000, "/Storage/x".len(), OpenFlags::READ),
            Err(Errno::AlreadyExists)
        );
        assert_eq!(aspaces.read().open_file_entry(SecTaskId(2), 4), None);
    }

    /// `fs_read` re-authorises through the service and copies the read bytes
    /// out to the caller; an unknown fd and a write-only handle fail closed.
    #[test]
    fn fs_read_copies_bytes_out_and_enforces_the_handle() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) =
            send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, b"/f");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::FS_ACCESS], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let mut mock = RecordingFs::new();
        mock.read_data = b"hello".to_vec();
        let fs: &'static RecordingFs = Box::leak(Box::new(mock));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_filesystem(fs);

        // An unknown descriptor fails closed before the service is touched.
        assert_eq!(h.fs_read(&ctx, 99, 0, 0x1000, 5), Err(Errno::NotFound));

        let fd = u32::try_from(
            h.fs_open(&ctx, 0x1000, "/f".len(), OpenFlags::READ)
                .expect("open"),
        )
        .unwrap();
        let n = h.fs_read(&ctx, fd, 0, 0x1000, 5).expect("read");
        assert_eq!(n, 5);
        let delivered = h
            .with_caller_aspace(&ctx, |space, physmap| {
                let mut buf = alloc::vec![0u8; 5];
                copy_in(space, physmap, VirtAddr::new(0x1000), &mut buf).expect("readable");
                buf
            })
            .expect("caller space");
        assert_eq!(delivered.as_slice(), b"hello");

        // A handle opened without READ refuses fs_read.
        let wo = u32::try_from(
            h.fs_open(&ctx, 0x1000, "/f".len(), OpenFlags::WRITE)
                .expect("open write-only"),
        )
        .unwrap();
        // Re-seed the path page is unnecessary; the handle keeps its own path.
        assert_eq!(
            h.fs_read(&ctx, wo, 0, 0x1000, 5),
            Err(Errno::PermissionDenied)
        );
    }

    /// `fs_write` stages the caller's bytes in and forwards them, honouring
    /// the append posture; a read-only handle is refused.
    #[test]
    fn fs_write_forwards_bytes_and_honours_append() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) =
            send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, b"AB");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::FS_ACCESS], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let fs: &'static RecordingFs = Box::leak(Box::new(RecordingFs::new()));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_filesystem(fs);

        // Open append+write; page 0x1000 holds the two payload bytes "AB".
        let fd = u32::try_from(
            h.fs_open(&ctx, 0x1000, 2, OpenFlags::WRITE.union(OpenFlags::APPEND))
                .expect("open append"),
        )
        .unwrap();
        // The path was 2 bytes ("AB" doubles as the path and the payload here).
        let n = h.fs_write(&ctx, fd, 0x999, 0x1000, 2).expect("write");
        assert_eq!(n, 2);
        let calls = fs.calls();
        assert!(
            calls.iter().any(|c| c.contains("write uid=1000")
                && c.contains("append=true")
                && c.contains("data=[65, 66]")),
            "the append write forwarded the staged bytes: {calls:?}"
        );

        // A read-only handle refuses fs_write.
        let ro = u32::try_from(
            h.fs_open(&ctx, 0x1000, 2, OpenFlags::READ)
                .expect("open ro"),
        )
        .unwrap();
        assert_eq!(
            h.fs_write(&ctx, ro, 0, 0x1000, 2),
            Err(Errno::PermissionDenied)
        );
    }

    /// `fs_readdir` packs the service's entries into the `DirEntry` stream;
    /// an undersized buffer fails closed without truncating.
    #[test]
    fn fs_readdir_packs_entries_and_rejects_a_small_buffer() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) =
            send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, b"/d");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::FS_ACCESS], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let mut mock = RecordingFs::new();
        mock.entries = alloc::vec![
            (FileKind::Directory, alloc::string::String::from("Logs")),
            (FileKind::Regular, alloc::string::String::from("motd")),
        ];
        let fs: &'static RecordingFs = Box::leak(Box::new(mock));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_filesystem(fs);

        let fd = u32::try_from(
            h.fs_open(&ctx, 0x1000, "/d".len(), OpenFlags::DIRECTORY)
                .expect("open dir"),
        )
        .unwrap();
        // Two records: (4 + 4) + (4 + 4) = 16 bytes.
        let total = usize::try_from(h.fs_readdir(&ctx, fd, 0x1000, 64).expect("readdir")).unwrap();
        assert_eq!(total, 16);
        let stream = h
            .with_caller_aspace(&ctx, |space, physmap| {
                let mut buf = alloc::vec![0u8; total];
                copy_in(space, physmap, VirtAddr::new(0x1000), &mut buf).expect("readable");
                buf
            })
            .expect("caller space");
        let (first, used) = DirEntry::decode(&stream).expect("first entry");
        assert_eq!(first.kind, FileKind::Directory);
        assert_eq!(first.name, b"Logs");
        let (second, _) = DirEntry::decode(&stream[used..]).expect("second entry");
        assert_eq!(second.name, b"motd");

        // A buffer too small for the whole listing fails closed.
        assert_eq!(
            h.fs_readdir(&ctx, fd, 0x1000, 4),
            Err(Errno::BufferTooSmall)
        );
    }

    /// `fs_stat` writes the service's `FileStat` to the caller; `fs_close`
    /// frees the descriptor and a double close fails closed.
    #[test]
    fn fs_stat_writes_metadata_and_close_frees_the_descriptor() {
        install_trace_filter();
        let sink = make_sink();
        let arch = Arc::new(TestArch::with_cpus(1));
        let sched = make_sched(arch.clone());
        let table = RwLock::new(CapTable::new());
        let ipc = RwLock::new(PortRegistry::new());
        let (space, physmap) =
            send_aspace(MapFlags::READ | MapFlags::WRITE | MapFlags::USER, b"/f");
        let aspaces = RwLock::new(AddressSpaceRegistry::new());
        let rng = unseeded_rng();
        aspaces
            .write()
            .register(SecTaskId(2), space, physmap)
            .expect("registration succeeds");
        let irq = IrqTable::new(31);
        let ctl = UnsupportedController;
        let caps = make_caps_record(2, &[CapabilityId::FS_ACCESS], sink);
        let ctx = CallerContext {
            task_id: SecTaskId(2),
            caps: &caps,
        };
        let mut mock = RecordingFs::new();
        mock.stat = FileStat {
            kind: FileKind::Regular,
            size: 0x1234,
            mode: 0o640,
            uid: 1000,
            gid: 1000,
        };
        let fs: &'static RecordingFs = Box::leak(Box::new(mock));
        let h = KernelSyscallHandlers::new(
            &sched, &table, &arch, sink, &irq, &ctl, &ipc, &aspaces, &rng,
        )
        .with_filesystem(fs);

        let fd = u32::try_from(
            h.fs_open(&ctx, 0x1000, "/f".len(), OpenFlags::READ)
                .expect("open"),
        )
        .unwrap();
        let n = usize::try_from(
            h.fs_stat(&ctx, fd, 0x1000, FileStat::WIRE_LEN)
                .expect("stat"),
        )
        .unwrap();
        assert_eq!(n, FileStat::WIRE_LEN);
        let decoded = h
            .with_caller_aspace(&ctx, |space, physmap| {
                let mut buf = alloc::vec![0u8; FileStat::WIRE_LEN];
                copy_in(space, physmap, VirtAddr::new(0x1000), &mut buf).expect("readable");
                FileStat::decode(&buf).expect("valid stat")
            })
            .expect("caller space");
        assert_eq!(decoded.size, 0x1234);
        assert_eq!(decoded.mode, 0o640);

        // An undersized stat buffer fails closed.
        assert_eq!(
            h.fs_stat(&ctx, fd, 0x1000, FileStat::WIRE_LEN - 1),
            Err(Errno::BufferTooSmall)
        );

        // Close frees the descriptor; a second close fails closed.
        assert_eq!(h.fs_close(&ctx, fd), Ok(0));
        assert_eq!(h.fs_close(&ctx, fd), Err(Errno::NotFound));
        assert_eq!(
            h.fs_stat(&ctx, fd, 0x1000, FileStat::WIRE_LEN),
            Err(Errno::NotFound)
        );
    }
}
