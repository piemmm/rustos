//! Capability-checked, audited process-spawn caller.
//!
//! [`tairix_kernel_mem::build_process_image`] is the architecture-neutral
//! *memory mechanism*: given a validated [`tairix_abi::rxe::LoadImage`] it
//! materialises a runnable user address space (segments mapped and filled,
//! a zeroed user stack, and the `tairix_abi::process` startup-vector block)
//! and reports the [`tairix_kernel_mem::ProcessImage`] register state. It is
//! deliberately capability-agnostic and never logs (`kernel/mem` does not depend on the security policy or `lib/log`).
//!
//! This module is the *policy* half: the one path that authorises a spawn,
//! audits the decision, builds the image, and drops the calling CPU into the
//! new program through the Arch HAL [`EnterUser`] primitive. Keeping the capability gate and the audit record
//! here — in the caller, not in `kernel/mem` — is what preserves the
//! layering while still satisfying (capability check before any state
//! touch) and (security-relevant decisions are audited).
//!
//! # Security
//!
//! Spawning a program is privileged: it materialises a new principal's
//! address space and hands it the CPU. [`spawn_and_enter`] therefore
//! requires the caller to hold [`CapabilityId::PROC_SPAWN`] and fails closed
//! (no ambient authority; — fail closed) — the check
//! happens *before* `build_process_image` touches any page table. The hosted
//! program still receives only the capabilities its own signed manifest
//! requests intersected with its user's grants; this gate
//! authorises the *act* of spawning, it does not widen the new program's
//! authority.

use alloc::boxed::Box;

use tairix_abi::hwtree::HwResource;
use tairix_abi::rxe::LoadImage;
use tairix_abi::{CapabilityId, CapabilityQuery, Errno};
use tairix_arch_api::{EnterUser, UserEntry};
use tairix_caps::CapabilitySet;
use tairix_kernel_mem::{
    build_process_image, AddressSpace, Frame, FrameAllocator, LiveUserSpace, PageTable, PhysMap,
    SpawnError, UserAddressSpace, UserStack,
};
use tairix_log::{Event, Field, Level, Sink};
use tairix_util::fmt::format_hex_u64;

use crate::aspace::StackSpan;
use crate::audit::AuditEvent;

/// The architecture-specific seam that spawns PID 1 (`init`) into user
/// mode once the kernel has finished booting.
///
/// Building a user address space and dropping the CPU into it is
/// irreducibly architecture-specific — it names the port's concrete page
/// table, its [`EnterUser`] primitive, and the direct physical map — none
/// of which `kernel/core` can spell. So
/// rather than teach [`crate::kernel_main`] those types, the arch port (or
/// the kernel binary that wires it) hands the core a `&'static dyn
/// InitSpawn` through [`crate::BootInfo::with_init`]. After every init
/// phase has succeeded and [`AuditEvent::BootCompleted`] has been emitted,
/// `kernel_main` invokes [`Self::spawn_init`], passing the
/// [`InitSpawnCtx`] the core implements so the seam can build the image
/// (arch-specific) and then register the new task (core-specific) through
/// one object-safe boundary.
///
/// The implementation builds the image through the production,
/// capability-checked, audited [`spawn_image`] caller — it is *not* a
/// privileged bypass: the spawned program still receives only the
/// authority its manifest requests intersected with its user's grants.
pub trait InitSpawn {
    /// Build PID 1's EL0 image and hand it to [`InitSpawnCtx::admit_init`]
    /// for registration + entry. Diverges into user mode on success;
    /// returns only when PID 1 could not be spawned, so the caller halts
    /// fail-closed.
    ///
    /// Called exactly once, on the boot CPU, after every init phase has
    /// succeeded — so the MMU is enabled and the user→kernel trap path is
    /// installed (the new program's first syscall is therefore handled
    /// rather than faulting).
    ///
    /// `ctx` is a `&'static (dyn InitSpawnCtx + Sync)`, not a borrowed
    /// `&dyn InitSpawnCtx`: `kernel_main` builds it over the kernel binary's
    /// leaked-`'static` `KernelState` and hands it here so an in-kernel service the
    /// seam admits **before** `admit_init` diverges (e.g. the aarch64
    /// root-unlock kthread) can capture it in its `'static + Send` body and
    /// later drive [`InitSpawnCtx::spawn_driver_process`] to autoload
    /// user-space drivers off the mounted root (`plans/PI.md` P11;). The `Sync` bound is what makes the shared reference `Send`
    /// into that body. The seam itself only needs a shared reference, so it
    /// reborrows `ctx` as a plain `&dyn InitSpawnCtx` for its own use.
    fn spawn_init(&self, ctx: &'static (dyn InitSpawnCtx + Sync));
}

/// The core-side capabilities an [`InitSpawn`] seam needs to spawn PID 1:
/// the live frame allocator and audit sink for building the image, and
/// [`admit_init`](Self::admit_init) to register the freshly built task
/// with the scheduler, capability table, and address-space registry and
/// drop into it.
///
/// Implemented by `kernel/core` (the concrete `KernelInitSpawner`) and
/// handed to the seam as a `&dyn InitSpawnCtx`, so the arch-specific
/// builder and the core-specific registries meet at one object-safe
/// boundary — neither names the other's generics.
pub trait InitSpawnCtx {
    /// The kernel's live physical-frame allocator, the source of the
    /// frames the image's pages are mapped to.
    fn frames(&self) -> &FrameAllocator;

    /// The boot audit sink the build path records `ProcessSpawn*` events
    /// through.
    fn audit(&self) -> &(dyn Sink + Sync);

    /// Register the freshly built PID 1 with the scheduler (so
    /// `current_task` resolves the caller on its first syscall) and the
    /// capability table (`caps`, the effective set the manifest∩user grant
    /// produced), then dispatch it as a **resumable user kthread**
    /// (`plans/SPAWN.md` SP2): PID 1 runs on its own kernel stack, `enter`
    /// diverges it into EL0, and its rescheduling syscalls (`yield`/`exit`)
    /// suspend it back to the scheduler through the Arch-HAL context-switch
    /// slice. `kernel_main` drains the boot CPU's run queue until PID 1
    /// (and anything it spawns) has exited, then halts fail-closed.
    ///
    /// `enter` is the arch-specific user-mode transition boxed as a
    /// `FnMut()`: `EnterUser::enter_user` diverges, so the closure never
    /// truly returns (its `!` coerces to `()`); modelling it as `FnMut()`
    /// keeps this boundary free of a `dyn FnMut() -> !` bound. It becomes
    /// the kthread's work body, invoked once on the task's first dispatch.
    ///
    /// `pre_resume` is the arch-specific user-address-space reactivation
    /// hook (`plans/SPAWN.md` SP2): the runtime calls it on the
    /// dispatcher's context immediately before every switch into PID 1, so
    /// the task `eret`s back into EL0 under its own page-table root and
    /// stays hardware-isolated from any sibling process.
    /// It captures only the arch root word, so it is `Send`. Its presence
    /// also enrols PID 1 in the per-CPU resume table so its trap path can
    /// suspend it.
    ///
    /// `space` is the registry-storable, `Send + Sync` snapshot of PID 1's
    /// user mappings (an arch port's *live* `AddressSpace` is not `Sync`
    /// while it owns a `&'static mut` root table, so the seam freezes it —
    /// [`AddressSpace::freeze`](tairix_kernel_mem::AddressSpace::freeze)),
    /// and `physmap` is the kernel direct map backing it. They are
    /// registered with the kernel-wide [`crate::AddressSpaceRegistry`] under
    /// the *same* numeric id the dispatcher recovers, so PID 1's first
    /// syscall that copies from user memory (e.g. `stream_write` reading
    /// its banner) resolves the caller's address space instead of failing
    /// closed with `BadAddress` (`plans/PI.md` P6c-3 follow-up).
    ///
    /// `stack` is PID 1's kernel stack, built by the arch seam so the
    /// concrete stack source never leaks into this object-safe boundary. The seam supplies either the heap-backed
    /// software-canary [`crate::BoxStack`] or an arena-backed stack whose
    /// guard page it has **unmapped in PID 1's own page-table root**, so an
    /// overrun of PID 1's kernel stack takes a synchronous fault under PID
    /// 1's translation regime rather than corrupting a neighbour
    /// (`plans/PI.md` guard-page fault-form;). The
    /// runtime stores it in PID 1's control block and frees it when the task
    /// exits.
    ///
    /// `live` is PID 1's **retained live, mutable** user address space
    /// (`plans/PI.md` 5d-0-ii (b′)): when [`Some`], the runtime owns it in
    /// PID 1's control block and publishes it on the per-CPU live-space slot
    /// while PID 1 is switched in, so PID 1's `mem_map` / `mmio_map`
    /// syscalls mutate its own address space through
    /// [`crate::kthread::with_current_live_space`]. The seam builds it from
    /// the *same* arch [`AddressSpace`] it froze into `space`, so the
    /// snapshot and the live space describe one set of mappings. A seam that
    /// retains no live space passes [`None`] and PID 1's `mem_map` /
    /// `mmio_map` fail closed.
    ///
    /// # Safety
    ///
    /// The seam must have built PID 1's image into the **active** address
    /// space on the calling CPU and installed the user→kernel trap path
    /// before calling here, so PID 1's first syscall is handled rather than
    /// faulting. `space` must faithfully describe the mappings the active
    /// address space resolves, and `physmap` must back them, so the copy
    /// path reads exactly the memory the program sees. `stack` must be a
    /// region exclusive to PID 1 that stays mapped (its guard page aside)
    /// and valid for as long as the task lives. When `live` is [`Some`] it
    /// must wrap the same arch address space `space` was frozen from, so the
    /// live mutations and the frozen copy view stay consistent.
    /// `stack_span` must describe the user-stack span the seam's layout
    /// actually placed in `space` — the fault path backs growth pages
    /// inside it, so a span naming the wrong region would let a fault map
    /// memory the layout never reserved.
    #[allow(clippy::too_many_arguments)]
    unsafe fn admit_init(
        &self,
        caps: CapabilitySet,
        space: Box<dyn UserAddressSpace + Send + Sync>,
        physmap: Box<dyn PhysMap + Send + Sync>,
        stack_span: StackSpan,
        stack: Box<dyn crate::kthread::KernelStack + Send>,
        pre_resume: Box<dyn FnMut(u64) + Send>,
        live: Option<Box<dyn LiveUserSpace + Send>>,
        enter: Box<dyn FnMut() + Send>,
    );

    /// Admit a resumable **kernel-only** service kthread that runs
    /// alongside PID 1 on the boot CPU's run queue, returning whether it
    /// was admitted.
    ///
    /// This is the in-kernel counterpart of [`admit_init`](Self::admit_init):
    /// where that admits a user (EL0) task, this admits a pure-kernel
    /// coroutine (`plans/SPAWN.md` SP1, [`crate::spawn_kthread`]). Its sole
    /// production use is the aarch64 USB-keyboard service
    /// (`plans/PI.md` P10/P11): a driver loop that brings the VL805 chain up
    /// once and then polls it forever, injecting decoded key presses into
    /// the input-focus arbiter. Because the bring-up is slow (PCIe link
    /// training) and the poll is continuous, it cannot run on the boot path
    /// before user mode — it must be a scheduled task that yields between
    /// polls so PID 1 also runs.
    ///
    /// `body` is the service work: it owns its driver resources (mapped
    /// register windows, the DMA region, the keyboard chain — all `'static`
    /// because kernel state is never freed) and uses the object-safe
    /// [`crate::YieldHandle`] to suspend cooperatively, so it need not name
    /// the port's concrete context-switch type.
    /// A body that returns ends the service; a continuous service never
    /// returns.
    ///
    /// Returns [`Some`] with the admitted task's scheduler
    /// [`TaskId`](tairix_kernel_sched_api::TaskId) so the
    /// caller can wake the service by id — e.g. register it on a
    /// [`crate::waitq::WaitQueue`] (the driver-store server parks on
    /// [`crate::waitq::SERVE_WAITQ`] and is unparked by
    /// [`crate::waitq::serve_wake`] when an `ipc_call` request is posted — a real wake, never a busy-yield). [`None`] means
    /// the service was not admitted.
    ///
    /// The default implementation admits nothing and returns [`None`]
    /// (a context that wires no scheduler offers no
    /// service rather than pretending to). The production
    /// `KernelInitSpawner` admits the body as a [`crate::spawn_kthread`]
    /// task on the boot CPU; this adds **no** ambient authority — the body
    /// holds only the capabilities and resources the seam built into it.
    fn spawn_kernel_service(
        &self,
        body: crate::kthread::KernelServiceBody,
    ) -> Option<tairix_kernel_sched_api::TaskId> {
        let _ = body;
        None
    }

    /// The kernel's live physical-frame allocator as a `'static` borrow,
    /// when one is wired.
    ///
    /// A service spawned through [`spawn_kernel_service`](Self::spawn_kernel_service)
    /// holds its driver's DMA region for the whole lifetime of the running
    /// kernel (a device's DMA mapping lives for the driver
    /// load), so it needs a `'static` allocator, not the call-scoped borrow
    /// [`frames`](Self::frames) hands out. Mirrors
    /// [`SpawnCtx::page_table_allocator`].
    ///
    /// The default returns [`None`] — a context with no `'static` allocator
    /// (a host test double) makes a service spawner fall back / fail closed rather than allocating from a borrow it cannot
    /// keep. The production `KernelInitSpawner` returns the leaked-`'static`
    /// kernel allocator.
    fn static_frames(&self) -> Option<&'static FrameAllocator> {
        None
    }

    /// The boot **audit sink** as a `'static` borrow, when one is wired.
    ///
    /// A service spawned through [`spawn_kernel_service`](Self::spawn_kernel_service)
    /// runs as a `'static` kthread, so to route its security-relevant
    /// decisions onto the audit channel (the root-unlock
    /// kthread's mount / install / give-up outcomes) it needs a `'static`
    /// sink, not the call-scoped [`audit`](Self::audit) borrow. Mirrors
    /// [`static_frames`](Self::static_frames).
    ///
    /// The default returns [`None`]; the production `KernelInitSpawner`
    /// returns the leaked-`'static` boot audit sink. A service handed [`None`]
    /// (a host test double) falls back to its own diagnostic sink rather than
    /// dropping the record.
    fn static_audit(&self) -> Option<&'static (dyn Sink + Sync)> {
        None
    }

    /// Spawn a verified user-space **driver** image into its own,
    /// hardware-isolated process and return its PID.
    ///
    /// This is the runtime-spawn counterpart of [`admit_init`](Self::admit_init):
    /// where that builds and *enters* PID 1, this admits a driver process
    /// **Ready** and returns, so the spawning boot path keeps running
    /// (`plans/SPAWN.md` SP3). It is the seam the bin crate's driver
    /// autoloader drives to turn a discovered,
    /// signature-verified driver bundle into a running user-space driver.
    ///
    /// The child is granted exactly `caps` — the manifest∩caller capability
    /// set the signed `drvhost::Host::load` gate already derived — plus one
    /// unforgeable, owner-checked device-resource grant per entry of
    /// `grants` (the matched hardware-tree node's requests), and is
    /// handed `args` as its startup-argument vector (`tairix_rt::arg`). This
    /// seam never widens authority beyond `caps` plus those grants
    /// (no ambient authority); the `grants` originate
    /// kernel-side, from the kernel's own discovered hardware tree, never
    /// from an untrusted caller.
    ///
    /// The point of routing through this object-safe boundary is: the
    /// production implementation builds the live
    /// [`KernelSpawnCtx`](crate::KernelSpawnCtx) over the feature-selected
    /// scheduler, capability table, and address-space registry — types a
    /// scheduler-agnostic caller (the bin crate) deliberately never names —
    /// and drives the deferred-load admit
    /// ([`KernelSpawnCtx::admit_loading`](crate::KernelSpawnCtx::admit_loading),
    /// `plans/FIX-DESKTOP.md` §2.6.5). The verified driver image is a
    /// *prebuilt* load plan, so the driver builds its own isolated address
    /// space on its own first slice — the autoloading boot service is never
    /// blocked on the build. Every kernel-side check still applies (the image
    /// was signature-verified by the load gate, and the build re-parses the
    /// `rxe` against the kernel's syscall CFI tag), so spawning is not a
    /// privileged bypass.
    ///
    /// # Errors
    ///
    /// Fails closed with a stable [`Errno`] on any *admission* error (launch
    /// services unwired, scheduler exhaustion) — never a panic or a
    /// half-built task. A load failure discovered on the driver's own task
    /// surfaces through its reserved-status exit, not this return value
    /// (`plans/FIX-DESKTOP.md` §2.3). The default returns
    /// [`Errno::NotImplemented`]: a context that wires no scheduler offers no
    /// driver spawn rather than pretending to, mirroring
    /// [`spawn_kernel_service`](Self::spawn_kernel_service) returning
    /// [`None`].
    ///
    /// `node_id` is the discovered hardware-tree node the driver was matched
    /// for; it is recorded against the child so the
    /// child's later `hw_emit_node` calls parent published children under
    /// exactly that node, and the emitter cannot forge its tree position. [`None`] when the spawn is not a node-matched
    /// driver load.
    ///
    /// `path` is the kernel-resolved driver-store path the signed load gate
    /// verified the image from (a plain `/System/Drivers/input/usb_kbd` or a
    /// store bundle's `/System/Drivers/input/usb_kbd/Run` entry point). The
    /// production implementation attests the child's process name from it
    /// through the one shared naming rule — a bundle's generic `Run` leaf
    /// names its owning driver directory, any other path its final component
    /// — so a process listing (`ps`, `top`) and the audit origin always name
    /// the driver, never from caller-supplied bytes.
    #[allow(clippy::too_many_arguments)]
    fn spawn_driver_process(
        &self,
        path: &str,
        rxe: &[u8],
        caps: CapabilitySet,
        grants: &[HwResource],
        args: &[&[u8]],
        node_id: Option<u32>,
    ) -> Result<u64, Errno> {
        let _ = (path, rxe, caps, grants, args, node_id);
        Err(Errno::NotImplemented)
    }

    /// Tear down a previously [`spawn_driver_process`](Self::spawn_driver_process)ed
    /// driver named by `handle` (its returned PID), reclaiming **all** of its
    /// kernel-held state, and return whether a live driver was found.
    ///
    /// This is the symmetric partner of
    /// [`spawn_driver_process`](Self::spawn_driver_process), the kernel
    /// *mechanism* the device manager's hot-removal *policy* drives: when a
    /// bound hardware-tree node vanishes the manager asks the kernel to
    /// unload the driver it loaded for it. The production implementation:
    ///
    /// * reaps the driver's scheduler task (so it is never dispatched again
    ///   and its kernel stack, live address space, and page-table frames are
    ///   reclaimed when its control block drops);
    /// * withdraws its address-space-registry entry — reclaiming its
    ///   device-resource grants, standard streams, resource limits, and
    ///   matched-node record (no stale grant can outlive the driver);
    /// * destroys every synchronous call endpoint it served, waking blocked
    ///   callers so they abandon fail-closed rather than park forever;
    ///   releases every IRQ binding it held; and drops its capability record.
    ///
    /// Teardown is **idempotent** and **fails closed**: tearing down a
    /// `handle` that names no live driver reclaims nothing and returns
    /// [`Errno::NotFound`], never a panic and never a partial state. It adds
    /// no ambient authority — it only reclaims state the kernel itself
    /// minted for the named task.
    ///
    /// # Errors
    ///
    /// [`Errno::NotFound`] if `handle` names no live driver (already gone, or
    /// never a driver). The default returns it unconditionally: a context
    /// that wired no scheduler holds no driver to unload, mirroring
    /// [`spawn_driver_process`](Self::spawn_driver_process)'s default.
    fn terminate_driver_process(&self, handle: u64) -> Result<(), Errno> {
        let _ = handle;
        Err(Errno::NotFound)
    }
}

/// Why a [`spawn_and_enter`] call did not transfer control to a new program.
///
/// On success the call diverges into user mode and never returns, so the
/// `Ok` variant carries [`core::convert::Infallible`]: a returning call is
/// always one of these failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SpawnCallerError {
    /// The caller does not hold [`CapabilityId::PROC_SPAWN`]; no address
    /// space was built (fail closed).
    Denied,
    /// Building the process image failed (see [`SpawnError`]); the partially
    /// built address space is discarded by the caller.
    Build(SpawnError),
}

/// What to spawn: the validated image, its backing bytes, and the user-space
/// layout [`build_process_image`] needs.
///
/// Bundled into one struct so [`spawn_and_enter`] keeps a small, readable
/// argument list rather than the ten positional parameters of the underlying
/// builder.
pub struct SpawnRequest<'a> {
    /// The validated `rxe` load image (holding one is proof the
    /// load-time invariants hold).
    pub image: &'a LoadImage,
    /// The whole `rxe` file the segments' `file_offset`s index into.
    pub image_bytes: &'a [u8],
    /// Relocation bias applied to the image's link addresses.
    pub bias: u64,
    /// Where, and how large, the initial user stack is.
    pub stack: UserStack,
    /// Page-aligned user virtual address the startup-vector block is written
    /// at (the value handed to the program in the first-argument register).
    pub start_block_base: u64,
    /// The argument vector, each entry a NUL-free byte string.
    pub args: &'a [&'a [u8]],
    /// The environment vector, each entry a NUL-free byte string.
    pub env: &'a [&'a [u8]],
    /// Per-process random seed for the stack canary.
    pub canary: u64,
}

/// A stable `&'static str` naming a [`SpawnError`] for the audit `cause`
/// field. The audit record never formats untrusted data; it names which
/// closed-fail branch the builder took.
const fn spawn_error_cause(error: SpawnError) -> &'static str {
    match error {
        SpawnError::Load(_) => "mapping_failed",
        SpawnError::Layout(_) => "layout_overflow",
        SpawnError::SegmentContentOutOfRange => "segment_content_out_of_range",
        SpawnError::PhysUnmapped => "phys_unmapped",
        SpawnError::EmptyStack => "empty_stack",
        SpawnError::Misaligned => "misaligned",
        SpawnError::StartBlock(_) => "startup_block",
        // `SpawnError` is `#[non_exhaustive]`; a future variant audits as a
        // generic build failure until it earns its own stable cause string.
        _ => "build_failed",
    }
}

/// Authorise, audit, build, and enter a freshly spawned process.
///
/// The call:
///
/// 1. checks `caps` holds [`CapabilityId::PROC_SPAWN`], failing closed with
///    [`SpawnCallerError::Denied`] and an [`AuditEvent::ProcessSpawnDenied`]
///    record if not — *before* any page table is touched;
/// 2. calls [`build_process_image`] to materialise the user address space in
///    `space`, emitting [`AuditEvent::ProcessSpawnFailed`] and returning
///    [`SpawnCallerError::Build`] on failure;
/// 3. emits [`AuditEvent::ProcessSpawned`] (carrying the relocated entry
///    point), then transfers control to the new program through
///    [`EnterUser::enter_user`], which never returns.
///
/// # Safety
///
/// On the authorised, successful path this calls [`EnterUser::enter_user`],
/// whose contract the caller must uphold: `space` must already be the
/// **active** address space on the calling CPU and the kernel's user→kernel
/// trap path must be installed, so the new program's first syscall is handled
/// rather than faulting. (The image is built into `space`; activating it and
/// installing the trap vector are the caller's responsibility because they
/// are architecture-specific and live outside `kernel/core`.)
///
/// # Errors
///
/// Returns [`SpawnCallerError::Denied`] when the capability check fails and
/// [`SpawnCallerError::Build`] when image construction fails. On success the
/// function diverges and does not return.
#[allow(clippy::too_many_arguments)]
pub unsafe fn spawn_and_enter<P, A, E>(
    caps: &dyn CapabilityQuery,
    audit: &dyn Sink,
    enter: &E,
    space: &mut AddressSpace<P>,
    physmap: &dyn PhysMap,
    request: &SpawnRequest<'_>,
    alloc_frame: A,
) -> Result<core::convert::Infallible, SpawnCallerError>
where
    P: PageTable,
    A: FnMut() -> Option<Frame>,
    E: EnterUser,
{
    // Authorise, build the image, and emit `ProcessSpawned` — everything up
    // to (but not including) the user-mode transition.
    // SAFETY: `spawn_image`'s contract (the returned `UserEntry` is only
    // entered once `space` is active and the trap path installed) is upheld
    // by this function's own identical safety contract, discharged by the
    // `enter_user` call below.
    let entry = unsafe { spawn_image(caps, audit, space, physmap, request, alloc_frame)? };

    // SAFETY: the function's own safety contract requires `space` to be the
    // active address space with the trap path installed. `spawn_image`
    // mapped `entry.pc` as a user-accessible executable page and the stack
    // top as the exclusive top of a user-accessible writable stack in
    // `space`, so the `UserEntry` register state satisfies
    // `EnterUser::enter_user`'s precondition.
    unsafe { enter.enter_user(entry) }
}

/// Authorise, audit, and build a freshly spawned process **without**
/// entering user mode — the build half of [`spawn_and_enter`].
///
/// Performs the same steps 1–2 as [`spawn_and_enter`] (capability check
/// before any state touch; [`build_process_image`]; the
/// [`AuditEvent::ProcessSpawned`] record) and returns the [`UserEntry`]
/// register state the caller must hand to [`EnterUser::enter_user`] to
/// transfer control. It exists so a caller that must do work **between**
/// building the image and entering it — register the new task with the
/// scheduler, capability table, and address-space registry so its first
/// syscall resolves a caller context (`plans/PI.md` P6c-3) — can interpose
/// that work without duplicating the authorise/build/audit logic. [`spawn_and_enter`] is the no-interposition case:
/// it calls this and immediately enters.
///
/// # Safety
///
/// The returned [`UserEntry`] is only meaningful once `space` is the
/// **active** address space on the calling CPU and the user→kernel trap
/// path is installed; entering it otherwise is unsound. Building the image
/// itself is safe — the unsafety is deferred to the eventual
/// [`EnterUser::enter_user`] call the caller makes with the returned value.
///
/// # Errors
///
/// Returns [`SpawnCallerError::Denied`] when the capability check fails and
/// [`SpawnCallerError::Build`] when image construction fails.
pub unsafe fn spawn_image<P, A>(
    caps: &dyn CapabilityQuery,
    audit: &dyn Sink,
    space: &mut AddressSpace<P>,
    physmap: &dyn PhysMap,
    request: &SpawnRequest<'_>,
    alloc_frame: A,
) -> Result<UserEntry, SpawnCallerError>
where
    P: PageTable,
    A: FnMut() -> Option<Frame>,
{
    // Step 2 — capability check before any state touch.
    if !caps.holds(CapabilityId::PROC_SPAWN) {
        emit(audit, AuditEvent::ProcessSpawnDenied, Level::Error, &[]);
        return Err(SpawnCallerError::Denied);
    }

    let image = build_process_image(
        space,
        physmap,
        request.image,
        request.image_bytes,
        request.bias,
        &request.stack,
        request.start_block_base,
        request.args,
        request.env,
        request.canary,
        alloc_frame,
    )
    .map_err(|error| {
        emit(
            audit,
            AuditEvent::ProcessSpawnFailed,
            Level::Error,
            &[Field {
                key: "cause",
                value: tairix_log::FieldValue::Str(spawn_error_cause(error)),
            }],
        );
        SpawnCallerError::Build(error)
    })?;

    let mut entry_buf = [0u8; 16];
    emit(
        audit,
        AuditEvent::ProcessSpawned,
        Level::Info,
        &[Field {
            key: "entry",
            value: tairix_log::FieldValue::Str(format_hex_u64(image.entry, &mut entry_buf)),
        }],
    );

    Ok(UserEntry::new(
        image.entry,
        image.stack_top,
        image.start_block,
    ))
}

/// Audit one refusal an [`ArchImageBuilder::build`] takes *around*
/// [`spawn_image`] — the page-table, layout, and stack-span derivations the
/// build performs before the image is materialised — and return the stable
/// resource errno the load surfaces.
///
/// [`spawn_image`] audits the capability decision and the image build
/// itself, but the build steps around it used to refuse silently: the audit
/// log showed a rejected load with no cause, which is exactly the gap that
/// made a boot-service launch refusal undiagnosable from a serial
/// transcript. A refused build is a security-relevant decision, so it is
/// logged with a stable `cause` (the closed build-failure cause vocabulary)
/// plus the kernel frame allocator's remaining free-frame count, which tells
/// genuine RAM exhaustion apart from a derivation or translate failure at
/// the same site.
pub fn refuse_build(ctx: &dyn ImageBuildCtx, cause: &'static str) -> Errno {
    emit_refuse(ctx.audit(), ctx.frames().free_frames() as u64, cause);
    Errno::NoSpace
}

/// Emit the shared `ProcessSpawnFailed` audit record for [`refuse_build`].
fn emit_refuse(audit: &(dyn Sink + Sync), free_frames: u64, cause: &'static str) {
    emit(
        audit,
        AuditEvent::ProcessSpawnFailed,
        Level::Error,
        &[
            Field {
                key: "cause",
                value: tairix_log::FieldValue::Str(cause),
            },
            Field {
                key: "free_frames",
                value: tairix_log::FieldValue::UnsignedInt(free_frames),
            },
        ],
    );
}

/// Map an [`AdmitError`] onto its stable [`Errno`] for the `spawn` syscall's
/// caller (`plans/FIX-DESKTOP.md` §2.3).
///
/// A pure mapping: the asynchronous-launch admit path
/// ([`KernelSpawnCtx::admit_loading`](crate::KernelSpawnCtx::admit_loading))
/// fails synchronously only when the launch services are unwired or the
/// scheduler is exhausted — neither is a *load* refusal (those surface
/// through the child's reserved-status exit and are audited on the child's
/// task), so this maps the admit outcome without a second audit record.
#[must_use]
pub fn admit_errno(err: AdmitError) -> Errno {
    // `AdmitError` is `#[non_exhaustive]` only outside this crate; here the
    // match is exhaustive, so a future variant fails the build until it
    // declares its own stable errno.
    match err {
        AdmitError::SchedulerFull => Errno::NoSpace,
        AdmitError::AspaceConflict => Errno::AlreadyExists,
    }
}

/// Map a [`SpawnCallerError`] onto a stable [`Errno`] for the `spawn`
/// syscall's caller. The precise cause is already on the audit log via the
/// `ProcessSpawn*` events [`spawn_image`] emits, so this mapping is pure —
/// one definition shared by every port's producer.
#[must_use]
pub fn spawn_caller_errno(err: SpawnCallerError) -> Errno {
    // `SpawnCallerError` is `#[non_exhaustive]` only *outside* this crate;
    // here the match is exhaustive, so a future variant fails the build
    // until it declares its own stable errno.
    match err {
        // A missing `CAP_PROC_SPAWN` (cannot occur — the dispatcher already
        // gated the syscall — but mapped for completeness).
        SpawnCallerError::Denied => Errno::PermissionDenied,
        // Image construction failed (a frame/pool exhaustion, a malformed
        // segment, an over-size startup block). One stable resource errno.
        SpawnCallerError::Build(_) => Errno::NoSpace,
    }
}

/// One embedded program the kernel can launch on demand: its absolute
/// path and the validated `rxe` bytes (`plans/SPAWN.md` SP3).
///
/// The bytes are the same `rxe` blob the host-only `elf2rxe` build glue
/// produces for PID 1 `init` (one conversion path);
/// holding a valid [`LoadImage`] parsed from them is proof the
/// load-time invariants hold, so the spawn producer re-parses against the
/// kernel's compiled-in syscall CFI tag and fails closed on a mismatch.
#[derive(Clone, Copy)]
pub struct EmbeddedProgram {
    /// Absolute path the program is registered (and looked up) under.
    pub path: &'static [u8],
    /// The validated `rxe` image bytes.
    pub rxe: &'static [u8],
    /// The capabilities the program's manifest **requests**. This is the
    /// manifest side of the intersection: the admitted child's effective set
    /// is this request ∩ the spawning credential's user ceiling
    /// (`plans/CAPABILITY_USE.md` CU1) — each program asks only for the
    /// authority its own entry declares (the shell requests the session
    /// baseline; login additionally requests `USERS_READ` +
    /// `SPAWN_AS_USER`, `plans/CAPABILITY_USE.md` CU2), never the
    /// spawning caller's set (no ambient authority), and the account's
    /// grant bounds what the request can yield.
    pub caps: &'static [CapabilityId],
    /// The startup-argument vector handed to the program
    /// (`tairix_rt::arg`), each entry a NUL-free byte string.
    pub args: &'static [&'static [u8]],
}

impl EmbeddedProgram {
    /// The program's manifest-requested capabilities as a
    /// [`CapabilitySet`] — the manifest-request side of the
    /// `ceiling ∩ manifest` intersection the admit path derives the child's
    /// effective set from.
    #[must_use]
    pub fn capability_set(&self) -> CapabilitySet {
        let mut set = CapabilitySet::empty();
        for cap in self.caps {
            set.insert(*cap);
        }
        set
    }
}

/// Capability-agnostic, path-keyed registry of the embedded programs the
/// kernel can spawn (`plans/SPAWN.md` SP3).
///
/// Threaded into the syscall handler like the [`crate::ConsoleWrite`]
/// console seam: it boots [`EMPTY`](Self::EMPTY), so a `spawn` of any path
/// fails closed with [`Errno::NotFound`] until the kernel binary registers
/// its embedded programs (the host-only `elf2rxe` build glue). It is pure data with no ambient authority and no audit sink of
/// its own — the `spawn` handler and the [`ProcessSpawn`] producer own the
/// security-relevant logging, exactly as the dispatcher audits IPC
/// endpoint lookups rather than the registry doing so internally.
pub struct ProgramRegistry {
    programs: &'static [EmbeddedProgram],
}

impl ProgramRegistry {
    /// Build a registry over `programs`.
    #[must_use]
    pub const fn new(programs: &'static [EmbeddedProgram]) -> Self {
        Self { programs }
    }

    /// The empty registry — the boot default. A `spawn` of any path
    /// resolves nothing and fails closed with [`Errno::NotFound`].
    pub const EMPTY: Self = Self::new(&[]);

    /// The embedded program registered under `path`, or [`None`] if no
    /// program bears that exact path.
    ///
    /// The match is exact (a byte-for-byte absolute path); there is no
    /// prefix or alias resolution, so a path either names exactly one
    /// registered program or nothing at all (fail closed).
    #[must_use]
    pub fn lookup(&self, path: &[u8]) -> Option<&'static EmbeddedProgram> {
        self.programs.iter().find(|p| p.path == path)
    }

    /// Number of registered programs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.programs.len()
    }

    /// Whether the registry holds no programs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.programs.is_empty()
    }
}

/// The shared empty [`ProgramRegistry`] the syscall handler defaults to
/// until the kernel binary installs a populated one (fail closed; mirrors [`crate::NULL_CONSOLE`]).
pub static EMPTY_PROGRAM_REGISTRY: ProgramRegistry = ProgramRegistry::EMPTY;

/// Why admitting a freshly built process as a runnable task failed.
///
/// [`admit_errno`] maps each variant onto a stable [`Errno`] for the
/// `spawn` syscall's caller; the partially built resources are reclaimed
/// before returning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AdmitError {
    /// The scheduler's home run queue could not admit the new task.
    SchedulerFull,
    /// An address space is already registered for the new task id — a
    /// fresh id is never already present, so this signals a kernel
    /// invariant violation and is refused rather than papered over.
    AspaceConflict,
}

/// A freshly built, not-yet-admitted user image an [`ArchImageBuilder`]
/// hands the core after materialising a program into a hardware-isolated
/// address space (`plans/FIX-DESKTOP.md` §5 item 1).
///
/// This is the deferred-load counterpart of the eager `admit_process`
/// hand-off: the arch seam still owns the irreducibly architecture-specific
/// work (spelling the port's concrete page table, [`EnterUser`] primitive,
/// and direct physical map), but it now runs on the **loading child's own
/// kernel stack** and returns the built image *as a value* for the core to
/// register, rather than admitting the task itself. Keeping admission in the
/// core (one definition, all ports) is what lets the load run off the
/// spawning caller's task so an interactive loop never freezes behind it.
///
/// It carries **no** kernel stack: the loading kthread already owns the
/// stack it runs on (allocated at admit through
/// [`ArchImageBuilder::alloc_kernel_stack`]), and [`ArchImageBuilder::build`]
/// only re-expresses that stack's guard page in the child's own (inactive)
/// root ([`ImageBuildCtx::kernel_stack_guard`]).
pub struct BuiltImage {
    /// The registry-storable, `Send + Sync` frozen snapshot of the child's
    /// user mappings (an arch port's *live* address space is not `Sync`),
    /// registered under the child's id so its first user-memory copy
    /// resolves its own mappings.
    pub frozen: Box<dyn UserAddressSpace + Send + Sync>,
    /// The kernel direct map backing `frozen`, so the copy path reads
    /// exactly the memory the program sees.
    pub physmap: Box<dyn PhysMap + Send + Sync>,
    /// The user-stack span the seam's layout placed in `frozen`; the fault
    /// path backs growth pages inside it.
    pub stack_span: StackSpan,
    /// The retained live, mutable address space (`plans/PI.md`
    /// 5d-0-ii (b′)) built from the *same* arch space `frozen` was frozen
    /// from, so the child's `mem_map` / `mmio_map` mutate its own space.
    /// [`None`] retains no live space and those syscalls fail closed.
    pub live: Option<Box<dyn LiveUserSpace + Send>>,
    /// The child's page-table-root reactivation hook, run on the
    /// dispatcher's context before every switch into the child so it enters
    /// user mode under its own isolated root. Handed the task's kernel-stack
    /// top (x86_64 repoints its per-CPU entry stack; aarch64/riscv64 ignore
    /// it).
    pub pre_resume: Box<dyn FnMut(u64) + Send>,
    /// The arch-specific user-mode transition; it diverges, so its `!`
    /// coerces to `()`. The loading body invokes it once, after
    /// [`crate::kthread::Yielder::become_user`] has installed `pre_resume`
    /// and `live`.
    pub enter: Box<dyn FnMut() + Send>,
}

/// The build-only subset of the spawn context an [`ArchImageBuilder`] reads
/// to materialise a child image (`plans/FIX-DESKTOP.md` §5 item 1).
///
/// It exposes exactly the frame sources and audit sink the build path
/// needs, plus the loading kthread's kernel-stack guard VA so
/// [`ArchImageBuilder::build`] can re-express that guard page in the child's
/// own root — the same split+unmap the eager producer did at admit, now on
/// the child's own (inactive) space. Admission is **not** part of this
/// boundary: the core owns it.
pub trait ImageBuildCtx {
    /// The kernel's live physical-frame allocator — the source of the
    /// frames the image's pages are mapped to.
    fn frames(&self) -> &FrameAllocator;

    /// The live physical-frame allocator as a `'static` borrow, when one is
    /// wired, so the seam can build the child's page tables out of
    /// reclaimable RAM that scales with the machine. [`None`] makes the
    /// producer fail closed rather than over-spawn.
    fn page_table_allocator(&self) -> Option<&'static FrameAllocator>;

    /// The audit sink the build path records `ProcessSpawn*` events through.
    fn audit(&self) -> &(dyn Sink + Sync);

    /// The guard virtual address of the kernel stack the loading kthread
    /// runs on (from [`ArchImageBuilder::alloc_kernel_stack`]), so
    /// [`ArchImageBuilder::build`] can split the coarse identity block
    /// covering it and unmap the single guard page in the *child's own*
    /// root — turning an overrun of the child's kernel stack into a
    /// synchronous fault under the child's translation regime rather than
    /// corrupting a neighbour.
    ///
    /// [`None`] when the stack is the heap-backed software-canary
    /// [`crate::BoxStack`] fallback (which guards itself with a poison
    /// canary and needs no page unmapped in the child root).
    fn kernel_stack_guard(&self) -> Option<u64>;
}

/// The architecture-specific seam that builds a fresh, hardware-isolated
/// user image from a validated `rxe` **without admitting it**
/// (`plans/FIX-DESKTOP.md` §5 item 1), the deferred-load replacement for
/// [`ProcessSpawn`].
///
/// Installed into the syscall handler exactly as [`ProcessSpawn`] was, and
/// captured by the boot-installed `SpawnServices` handle so the child's
/// loading body — running on its own kernel stack, off the spawning
/// caller's task — can drive the build. Splitting the old `spawn_with` into
/// [`alloc_kernel_stack`](Self::alloc_kernel_stack) (run synchronously at
/// admit, before the child exists) and [`build`](Self::build) (run in the
/// loading body) is what moves the disk read + verification + image build
/// off the caller's task.
///
/// `Sync` because the installed builder is shared, immutably, by every
/// CPU's dispatch path and captured in the `'static` `SpawnServices` handle.
pub trait ArchImageBuilder: Send + Sync {
    /// Allocate the loading child's kernel stack, returning it boxed behind
    /// the object-safe [`crate::kthread::KernelStack`] boundary together
    /// with its guard VA (`Some` for an arena-backed stack whose guard page
    /// [`build`](Self::build) will unmap in the child root, `None` for the
    /// heap-backed software-canary [`crate::BoxStack`] fallback).
    ///
    /// Run **synchronously at admit**, before the child's address space
    /// exists, so the loading kthread has a stack to run its own build on.
    /// The runtime stores the stack in the child's control block and frees
    /// it when the task exits.
    fn alloc_kernel_stack(
        &self,
        frames: &FrameAllocator,
        pt_frames: Option<&'static FrameAllocator>,
    ) -> (Box<dyn crate::kthread::KernelStack + Send>, Option<u64>);

    /// Build `rxe` into a fresh, hardware-isolated address space and return
    /// it as a [`BuiltImage`] for the core to admit — never admitting it
    /// here.
    ///
    /// Runs in the loading child's body, on the stack
    /// [`alloc_kernel_stack`](Self::alloc_kernel_stack) allocated. It builds
    /// the image through the production, capability-checked, audited
    /// [`spawn_image`] caller (spawning is *not* a privileged bypass), then
    /// re-expresses the loading stack's guard page
    /// ([`ImageBuildCtx::kernel_stack_guard`]) in the child's own inactive
    /// root. A `Some(guard)` whose split+unmap fails in the child root fails
    /// the build **closed** rather than silently downgrading to an unguarded
    /// stack.
    ///
    /// # Errors
    ///
    /// A stable [`Errno`] on any failure — a malformed `rxe`, a build or
    /// page-table-frame exhaustion, or a guard-unmap failure — never a panic
    /// or a half-built image.
    fn build(
        &self,
        rxe: &[u8],
        ctx: &dyn ImageBuildCtx,
        args: &[&[u8]],
        env: &[&[u8]],
    ) -> Result<BuiltImage, Errno>;
}

/// The fail-closed default [`ArchImageBuilder`]: a build with no real image
/// builder wired fails the deferred load closed with
/// [`Errno::NotImplemented`], the deferred-launch counterpart of the former
/// `NullProcessSpawn`.
///
/// A port that has not wired a runtime image builder leaves this default, so
/// a `spawn` admits the child but the child's own load then exits with the
/// reserved [`tairix_abi::LOAD_MALFORMED`] status
/// ([`tairix_abi::load_failure_status`] of [`Errno::NotImplemented`]) rather
/// than the boot path half-building a task. `alloc_kernel_stack` hands back
/// the software-canary [`crate::BoxStack`] (which self-guards, so it returns
/// `None` for the guard VA).
pub struct NullArchImageBuilder;

impl ArchImageBuilder for NullArchImageBuilder {
    fn alloc_kernel_stack(
        &self,
        _frames: &FrameAllocator,
        _pt_frames: Option<&'static FrameAllocator>,
    ) -> (Box<dyn crate::kthread::KernelStack + Send>, Option<u64>) {
        (Box::new(crate::kthread::BoxStack::new()), None)
    }

    fn build(
        &self,
        _rxe: &[u8],
        _ctx: &dyn ImageBuildCtx,
        _args: &[&[u8]],
        _env: &[&[u8]],
    ) -> Result<BuiltImage, Errno> {
        Err(Errno::NotImplemented)
    }
}

/// The shared [`NullArchImageBuilder`] the boot handover defaults to until
/// an arch port installs a real image builder through
/// [`crate::BootInfo::with_spawn`].
pub static NULL_ARCH_IMAGE_BUILDER: NullArchImageBuilder = NullArchImageBuilder;

/// Emit one structured audit record for `event` with `fields`.
fn emit(audit: &dyn Sink, event: AuditEvent, level: Level, fields: &[Field<'_>]) {
    tairix_log::log(
        audit,
        &Event {
            level,
            id: event.id(),
            message: event.message(),
            fields,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_sink::TestSink;
    use tairix_abi::rxe::{LoadHeader, RxePermission, Segment, LOAD_FLAG_PIE};
    use tairix_abi::{ABI_VERSION_CURRENT, LOAD_MAGIC, SYSCALL_TABLE_HASH_LEN};
    use tairix_kernel_mem::{
        AddressSpace, BootMemoryMap, HostPageTable, MemoryRegion, PhysAddr, RegionKind, SimPhysMap,
        UserStack, PAGE_SIZE,
    };
    use tairix_log::{set_max_level, Level};

    extern crate std;
    use std::boxed::Box;

    const TAG: [u8; SYSCALL_TABLE_HASH_LEN] = [0x33; SYSCALL_TABLE_HASH_LEN];

    /// A `CapabilityQuery` granting exactly the capabilities in its slice.
    struct Granted(&'static [CapabilityId]);
    impl CapabilityQuery for Granted {
        fn holds(&self, cap: CapabilityId) -> bool {
            self.0.contains(&cap)
        }
    }

    /// An `EnterUser` that must never be reached on the host. The deny and
    /// build-failure paths return before the transition, so a test that
    /// drives them never calls this; if one ever did, the panic flags the
    /// test bug rather than silently passing.
    struct NeverEnter;
    impl EnterUser for NeverEnter {
        unsafe fn enter_user(&self, _regs: UserEntry) -> ! {
            unreachable!("enter_user is only meaningful on the bare-metal target")
        }
    }

    /// A minimal valid single-segment PIE `rxe` blob plus the parsed image.
    fn tiny_image() -> (std::vec::Vec<u8>, LoadImage) {
        let seg = Segment {
            vaddr: 0x1000,
            file_offset: (LoadHeader::WIRE_LEN + Segment::WIRE_LEN) as u64,
            file_size: 4,
            mem_size: PAGE_SIZE as u64,
            permission: RxePermission::ReadExecute,
        };
        let header = LoadHeader {
            magic: LOAD_MAGIC,
            abi_version: ABI_VERSION_CURRENT,
            flags: LOAD_FLAG_PIE,
            segment_count: 1,
            needed_count: 0,
            entry: 0x1000,
            cfi_tag: TAG,
        };
        let mut rxe = std::vec::Vec::new();
        rxe.extend_from_slice(&header.to_le_bytes());
        rxe.extend_from_slice(&seg.to_le_bytes());
        rxe.extend_from_slice(&[0x13, 0x00, 0x00, 0x00]); // 4 bytes of "code"
        let image = LoadImage::parse(&rxe, &TAG).expect("valid tiny image");
        (rxe, image)
    }

    fn host_space() -> AddressSpace<HostPageTable> {
        AddressSpace::new(HostPageTable::new())
    }

    fn sim() -> SimPhysMap {
        SimPhysMap::new(PhysAddr::new((PAGE_SIZE * 16) as u64), 64 * PAGE_SIZE)
    }

    fn request<'a>(image: &'a LoadImage, bytes: &'a [u8], stack_pages: u64) -> SpawnRequest<'a> {
        SpawnRequest {
            image,
            image_bytes: bytes,
            bias: 0,
            stack: UserStack {
                base: 0x20_0000,
                page_count: stack_pages,
            },
            start_block_base: 0x30_0000,
            args: &[],
            env: &[],
            canary: 0,
        }
    }

    #[test]
    fn spawn_exit_cycles_return_every_frame_to_the_allocator() {
        // The I2 host spawn/exit cycle (`plans/APPS.md`): each cycle builds
        // a full process image (code, stack, startup block) out of the
        // kernel allocator, retains it as the task's live space, and drops
        // it exactly as the reap does — the allocator must return to its
        // pre-spawn level every time, never marching downward (the
        // login/logout leak this closes).
        use tairix_kernel_mem::{LiveSpace, VirtAddr};

        /// A `PhysMap` view over the one leaked [`SimPhysMap`], so the
        /// image build and the teardown scrub touch the same simulated
        /// memory (each `sim()` owns disjoint storage).
        struct SharedSim(&'static SimPhysMap);
        impl PhysMap for SharedSim {
            fn translate(&self, phys: PhysAddr, len: usize) -> Option<core::ptr::NonNull<u8>> {
                self.0.translate(phys, len)
            }

            fn clean_invalidate(&self, phys: PhysAddr, len: usize) {
                self.0.clean_invalidate(phys, len);
            }
        }

        set_max_level(Level::Trace);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let (bytes, image) = tiny_image();
        let simmap: &'static SimPhysMap = Box::leak(Box::new(sim()));
        // An allocator over exactly the simulated RAM window, so every
        // frame the build draws is reachable for the zero-on-free scrub.
        let mut map = BootMemoryMap::new();
        map.push(MemoryRegion {
            kind: RegionKind::Usable,
            start: PhysAddr::new((PAGE_SIZE * 16) as u64),
            length: (64 * PAGE_SIZE) as u64,
        });
        let frames: &'static FrameAllocator =
            Box::leak(Box::new(FrameAllocator::new(&map).expect("allocator")));
        let caps = Granted(&[CapabilityId::PROC_SPAWN]);
        let before = frames.free_frames();

        for _ in 0..8 {
            let mut space = host_space();
            let req = request(&image, &bytes, 2);
            // SAFETY: building the image is safe; the returned `UserEntry`
            // is dropped, never entered, on the host.
            unsafe {
                spawn_image(&caps, sink, &mut space, simmap, &req, || {
                    frames.alloc().ok()
                })
            }
            .expect("image builds");
            assert!(frames.free_frames() < before, "the build consumed frames");

            // Retain the built space exactly as the spawn producers do,
            // then drop it — the reap-time teardown path.
            let live = LiveSpace::new(
                space,
                SharedSim(simmap),
                frames,
                VirtAddr::new(0x4000_0000),
                8,
                VirtAddr::new(0x5000_0000),
                8,
                VirtAddr::new(0x6000_0000),
                8,
                VirtAddr::new(0x7000_0000),
                8,
                VirtAddr::new(0x8000_0000),
                8,
            )
            .expect("windows are valid");
            drop(live);
            assert_eq!(
                frames.free_frames(),
                before,
                "every frame the cycle drew returned to the allocator"
            );
        }
    }

    #[test]
    fn denied_without_proc_spawn_capability_touches_no_state() {
        set_max_level(Level::Trace);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let (bytes, image) = tiny_image();
        let mut space = host_space();
        let physmap = sim();
        let caps = Granted(&[]); // no capabilities
        let req = request(&image, &bytes, 1);

        // SAFETY: the deny path returns before `enter_user`, so the
        // never-entering port is never invoked and the (inactive) host
        // address space is never entered.
        let result = unsafe {
            spawn_and_enter(&caps, sink, &NeverEnter, &mut space, &physmap, &req, || {
                None
            })
        };
        assert_eq!(result.err(), Some(SpawnCallerError::Denied));
        // Nothing was mapped: the check fails closed before building.
        assert_eq!(space.mapped_pages(), 0);
        let ids = sink.event_ids();
        assert!(ids.contains(&AuditEvent::ProcessSpawnDenied.id().0));
        assert!(!ids.contains(&AuditEvent::ProcessSpawned.id().0));
    }

    #[test]
    fn build_failure_is_audited_and_reported() {
        set_max_level(Level::Trace);
        let sink: &'static TestSink = Box::leak(Box::new(TestSink::new()));
        let (bytes, image) = tiny_image();
        let mut space = host_space();
        let physmap = sim();
        let caps = Granted(&[CapabilityId::PROC_SPAWN]);
        // A zero-page stack makes `build_process_image` fail closed with
        // `SpawnError::EmptyStack` before mapping anything.
        let req = request(&image, &bytes, 0);

        // SAFETY: `build_process_image` fails before the function reaches
        // `enter_user`, so the never-entering port is never invoked.
        let result = unsafe {
            spawn_and_enter(&caps, sink, &NeverEnter, &mut space, &physmap, &req, || {
                None
            })
        };
        assert_eq!(
            result.err(),
            Some(SpawnCallerError::Build(SpawnError::EmptyStack))
        );
        let ids = sink.event_ids();
        assert!(ids.contains(&AuditEvent::ProcessSpawnFailed.id().0));
        assert!(!ids.contains(&AuditEvent::ProcessSpawned.id().0));
    }

    /// A minimal [`SpawnCtx`] for exercising the [`ProcessSpawn::spawn_with`]
    /// default: the default returns before touching the context, so
    /// [`admit_process`](SpawnCtx::admit_process) is unreachable. It owns a
    /// one-region [`FrameAllocator`] so [`frames`](SpawnCtx::frames) can hand
    /// out a reference, and audits through a leaked [`TestSink`].
    struct StubCtx {
        frames: FrameAllocator,
        sink: &'static TestSink,
    }

    impl StubCtx {
        fn new() -> Self {
            // A single usable region the allocator can describe; the
            // default `spawn_with` never allocates from it.
            static mut REGION: [u8; PAGE_SIZE * 4] = [0u8; PAGE_SIZE * 4];
            let mut map = BootMemoryMap::new();
            map.push(MemoryRegion {
                start: PhysAddr::new(core::ptr::addr_of!(REGION) as u64),
                length: (PAGE_SIZE * 4) as u64,
                kind: RegionKind::Usable,
            });
            let frames = FrameAllocator::new(&map).expect("one-region allocator");
            Self {
                frames,
                sink: Box::leak(Box::new(TestSink::new())),
            }
        }
    }

    impl ImageBuildCtx for StubCtx {
        fn frames(&self) -> &FrameAllocator {
            &self.frames
        }

        fn page_table_allocator(&self) -> Option<&'static FrameAllocator> {
            None
        }

        fn audit(&self) -> &(dyn Sink + Sync) {
            self.sink
        }

        fn kernel_stack_guard(&self) -> Option<u64> {
            None
        }
    }

    #[test]
    fn null_arch_image_builder_fails_closed_through_a_trait_object() {
        // A port that has not wired an image builder (here the shared
        // `NULL_ARCH_IMAGE_BUILDER`) must refuse `build` with
        // `NotImplemented` rather than pretending to build — reached through
        // `&dyn ArchImageBuilder`, the path the boot handover default uses.
        let ctx = StubCtx::new();
        let builder: &dyn ArchImageBuilder = &NULL_ARCH_IMAGE_BUILDER;
        let result = builder.build(b"unused-rxe", &ctx, &[], &[]);
        assert_eq!(result, Err(Errno::NotImplemented));
        // Its kernel-stack allocation hands back the software-canary
        // `BoxStack` with no guard page to unmap in the child root.
        let (_stack, guard) = builder.alloc_kernel_stack(&ctx.frames, None);
        assert!(guard.is_none());
    }

    /// A minimal [`InitSpawnCtx`] exercising the default
    /// [`spawn_driver_process`](InitSpawnCtx::spawn_driver_process): the
    /// default returns before touching the producer or the context, so
    /// [`admit_init`](InitSpawnCtx::admit_init) is unreachable.
    struct StubInitCtx {
        frames: FrameAllocator,
        sink: &'static TestSink,
    }

    impl StubInitCtx {
        fn new() -> Self {
            static mut REGION: [u8; PAGE_SIZE * 4] = [0u8; PAGE_SIZE * 4];
            let mut map = BootMemoryMap::new();
            map.push(MemoryRegion {
                start: PhysAddr::new(core::ptr::addr_of!(REGION) as u64),
                length: (PAGE_SIZE * 4) as u64,
                kind: RegionKind::Usable,
            });
            let frames = FrameAllocator::new(&map).expect("one-region allocator");
            Self {
                frames,
                sink: Box::leak(Box::new(TestSink::new())),
            }
        }
    }

    impl InitSpawnCtx for StubInitCtx {
        fn frames(&self) -> &FrameAllocator {
            &self.frames
        }

        fn audit(&self) -> &(dyn Sink + Sync) {
            self.sink
        }

        unsafe fn admit_init(
            &self,
            _caps: CapabilitySet,
            _space: Box<dyn UserAddressSpace + Send + Sync>,
            _physmap: Box<dyn PhysMap + Send + Sync>,
            _stack_span: StackSpan,
            _stack: Box<dyn crate::kthread::KernelStack + Send>,
            _pre_resume: Box<dyn FnMut(u64) + Send>,
            _live: Option<Box<dyn LiveUserSpace + Send>>,
            _enter: Box<dyn FnMut() + Send>,
        ) {
            unreachable!("the default spawn_driver_process returns before admitting a process")
        }
    }

    #[test]
    fn spawn_driver_process_default_fails_closed_through_a_trait_object() {
        // A context that has not wired a scheduler (here the stub) must
        // refuse `spawn_driver_process` with `NotImplemented` rather than
        // pretending to spawn a driver — reached through `&dyn InitSpawnCtx`,
        // the path the bin crate's driver autoloader uses.
        let ctx = StubInitCtx::new();
        let init: &dyn InitSpawnCtx = &ctx;
        let result = init.spawn_driver_process(
            "/System/Drivers/input/usb_kbd",
            b"unused-rxe",
            CapabilitySet::empty(),
            &[],
            &[],
            None,
        );
        assert_eq!(result, Err(Errno::NotImplemented));
    }

    /// The one captured `ProcessSpawnFailed` record on `sink`, asserted to
    /// carry the expected stable `cause` plus a `free_frames` count.
    fn assert_refusal_audited(sink: &TestSink, cause: &str) {
        let records: std::vec::Vec<_> = sink
            .snapshot()
            .into_iter()
            .filter(|e| e.id == AuditEvent::ProcessSpawnFailed.id())
            .collect();
        assert_eq!(records.len(), 1, "exactly one refusal record");
        let record = &records[0];
        assert_eq!(record.level, Level::Error);
        assert!(
            record
                .fields
                .iter()
                .any(|(key, value)| key == "cause" && value == cause),
            "record names the refusing site: {:?}",
            record.fields
        );
        assert!(
            record.fields.iter().any(|(key, _)| key == "free_frames"),
            "record carries the allocator margin: {:?}",
            record.fields
        );
    }

    /// A build refusal taken before the image is materialised audits its
    /// cause and the allocator margin, and maps onto the stable resource
    /// errno — a refused build is never silent (regression: the x86_64
    /// spawn-session CI failure showed a rejected `spawn` with no cause on
    /// the log).
    #[test]
    fn refuse_build_audits_cause_and_returns_no_space() {
        set_max_level(Level::Trace);
        let ctx = StubCtx::new();
        assert_eq!(
            refuse_build(&ctx, "page_table_frames_exhausted"),
            Errno::NoSpace
        );
        assert_refusal_audited(ctx.sink, "page_table_frames_exhausted");
    }

    /// The synchronous admit-outcome mapping keeps each [`AdmitError`]'s
    /// stable errno (a scheduler-exhaustion / launch-services-unwired admit
    /// failure is `NoSpace`; an id conflict `AlreadyExists`). The load
    /// refusals are audited on the child's own task, so this mapping is pure.
    #[test]
    fn admit_errno_maps_stable_codes() {
        assert_eq!(admit_errno(AdmitError::SchedulerFull), Errno::NoSpace);
        assert_eq!(admit_errno(AdmitError::AspaceConflict), Errno::AlreadyExists);
    }

    /// The shared caller-errno mapping keeps the stable codes: a denied
    /// spawn is `PermissionDenied`, a failed build the resource errno.
    #[test]
    fn spawn_caller_errno_maps_stable_codes() {
        assert_eq!(
            spawn_caller_errno(SpawnCallerError::Denied),
            Errno::PermissionDenied
        );
        assert_eq!(
            spawn_caller_errno(SpawnCallerError::Build(SpawnError::EmptyStack)),
            Errno::NoSpace
        );
    }
}
