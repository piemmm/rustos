//! Capability-checked, audited process-spawn caller.
//!
//! [`rustos_kernel_mem::build_process_image`] is the architecture-neutral
//! *memory mechanism*: given a validated [`rustos_abi::rxe::LoadImage`] it
//! materialises a runnable user address space (segments mapped and filled,
//! a zeroed user stack, and the `rustos_abi::process` startup-vector block)
//! and reports the [`rustos_kernel_mem::ProcessImage`] register state. It is
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

use rustos_abi::hwtree::HwResource;
use rustos_abi::rxe::LoadImage;
use rustos_abi::{CapabilityId, CapabilityQuery, Errno};
use rustos_arch_api::{EnterUser, UserEntry};
use rustos_caps::CapabilitySet;
use rustos_kernel_mem::{
    build_process_image, AddressSpace, Frame, FrameAllocator, LiveUserSpace, PageTable, PhysMap,
    SpawnError, UserAddressSpace, UserStack,
};
use rustos_log::{Event, Field, Level, Sink};
use rustos_util::fmt::format_hex_u64;

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
    /// [`AddressSpace::freeze`](rustos_kernel_mem::AddressSpace::freeze)),
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
    #[allow(clippy::too_many_arguments)]
    unsafe fn admit_init(
        &self,
        caps: CapabilitySet,
        space: Box<dyn UserAddressSpace + Send + Sync>,
        physmap: Box<dyn PhysMap + Send + Sync>,
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
    /// [`TaskId`](rustos_kernel_sched_api::TaskId) so the
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
    ) -> Option<rustos_kernel_sched_api::TaskId> {
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
    /// handed `args` as its startup-argument vector (`rustos_rt::arg`). This
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
    /// and drives `spawn`'s [`ProcessSpawn::spawn_with`]. `spawn` is the
    /// architecture's process-spawn producer; it builds the isolated address
    /// space and re-asserts every kernel-side check (the spawn path re-checks
    /// `CAP_PROC_SPAWN` and re-parses the `rxe` against the kernel's syscall
    /// CFI tag), so spawning is not a privileged bypass.
    ///
    /// # Errors
    ///
    /// Fails closed with a stable [`Errno`] on any error — a malformed
    /// `rxe`, a build or page-table-frame exhaustion, an unrunnable context,
    /// or an admission failure — never a panic or a half-built task. The default returns [`Errno::NotImplemented`]: a
    /// context that wires no scheduler offers no driver spawn rather than
    /// pretending to, mirroring [`spawn_kernel_service`](Self::spawn_kernel_service)
    /// returning [`None`] and [`ProcessSpawn::spawn_with`]'s own default.
    ///
    /// `node_id` is the discovered hardware-tree node the driver was matched
    /// for; it is recorded against the child so the
    /// child's later `hw_emit_node` calls parent published children under
    /// exactly that node, and the emitter cannot forge its tree position. [`None`] when the spawn is not a node-matched
    /// driver load.
    fn spawn_driver_process(
        &self,
        spawn: &dyn ProcessSpawn,
        rxe: &[u8],
        caps: CapabilitySet,
        grants: &[HwResource],
        args: &[&[u8]],
        node_id: Option<u32>,
    ) -> Result<u64, Errno> {
        let _ = (spawn, rxe, caps, grants, args, node_id);
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
                value: rustos_log::FieldValue::Str(spawn_error_cause(error)),
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
            value: rustos_log::FieldValue::Str(format_hex_u64(image.entry, &mut entry_buf)),
        }],
    );

    Ok(UserEntry::new(
        image.entry,
        image.stack_top,
        image.start_block,
    ))
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
    /// (`rustos_rt::arg`), each entry a NUL-free byte string.
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
/// The [`ProcessSpawn`] producer maps each variant onto a stable
/// [`Errno`] for the `spawn` syscall's caller; the partially built
/// resources are reclaimed before returning.
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

/// The core-side registration context a [`ProcessSpawn`] producer drives
/// to register a freshly built process and obtain its PID
/// (`plans/SPAWN.md` SP3).
///
/// It is the runtime-spawn analogue of [`InitSpawnCtx`]: the arch-specific
/// producer builds the isolated address space (naming the port's concrete
/// page table + [`EnterUser`] primitive, which `kernel/core` cannot spell) and hands it back through
/// [`admit_process`](Self::admit_process), which registers the task with
/// the scheduler, capability table, and address-space registry. Unlike
/// [`InitSpawnCtx::admit_init`] it admits the task **Ready** and returns
/// its PID **without** entering user mode or draining the scheduler: the
/// child runs when the scheduler next steps, and the spawning caller keeps
/// running (a true concurrent spawn, not an `exec`-style hand-off).
pub trait SpawnCtx {
    /// The kernel's live physical-frame allocator, the source of the
    /// frames the new image's pages are mapped to.
    fn frames(&self) -> &FrameAllocator;

    /// The kernel's live physical-frame allocator as a `'static` borrow,
    /// when one is wired, so an arch producer can build the child's
    /// **page tables** out of ordinary reclaimable RAM instead of a
    /// fixed-size `.bss` pool (a capacity scales with
    /// discovered RAM and grows on demand, never a hard-wired `const`
    /// ceiling on how many processes can be spawned).
    ///
    /// This is the *same* allocator as [`frames`](Self::frames); the
    /// distinct accessor exists only because a port's `AddressSpace`
    /// retains its page-table frame source as a `&'static dyn
    /// PageTableFrames`, so the source must be `'static` (the elided
    /// lifetime of [`frames`](Self::frames) is the call only). The default
    /// returns [`None`] — a build context with no `'static` allocator (a
    /// host test double) makes the producer fall back / fail closed rather than over-spawning. The production
    /// [`KernelSyscallHandlers`](crate::KernelSyscallHandlers) returns the
    /// leaked-`'static` kernel allocator.
    fn page_table_allocator(&self) -> Option<&'static FrameAllocator> {
        None
    }

    /// The boot audit sink the build path records `ProcessSpawn*` events
    /// through.
    fn audit(&self) -> &(dyn Sink + Sync);

    /// Register the freshly built process as a runnable (**Ready**) task
    /// with the scheduler (as a resumable user kthread, `plans/SPAWN.md`
    /// SP2), the capability table, and the address-space registry (`space` +
    /// `physmap`, under the same numeric id the dispatcher recovers so the
    /// child's first user-memory copy resolves its own mappings), and return
    /// its PID.
    ///
    /// `caps` is the program's **manifest request**. The production context
    /// derives the child's effective set as `caps ∩ the spawn credential's
    /// user ceiling` (`plans/CAPABILITY_USE.md` CU1); a system-principal
    /// credential carries no users-db ceiling, so its manifest stands as
    /// both sides and the effective set is exactly `caps`.
    ///
    /// `enter` is the arch-specific user-mode transition boxed as a
    /// `FnMut()` (it diverges, so its `!` coerces to `()`); it becomes the
    /// task's kthread work body, run once on the task's first dispatch.
    /// `pre_resume` reactivates the task's page-table root before every
    /// switch into it, keeping it hardware-isolated from its siblings. It is handed the task's own kernel-stack top so a
    /// port whose syscall entry does not implicitly resume on that stack
    /// (x86_64) can repoint its per-CPU entry stack at it (`plans/PI.md`
    /// §X); aarch64 reuses `SP_EL1` and ignores the argument.
    ///
    /// `stack` is the child's kernel stack, built by the arch seam so the
    /// concrete stack source never leaks into this object-safe boundary, exactly as [`InitSpawnCtx::admit_init`] takes it.
    /// The seam supplies either the heap-backed software-canary
    /// [`crate::BoxStack`] or an arena-backed stack whose guard page it has
    /// **unmapped in the child's own page-table root**, so an overrun of the
    /// child's kernel stack takes a synchronous fault under the child's
    /// translation regime rather than corrupting a neighbour (`plans/PI.md`
    /// guard-page fault-form;). The runtime stores it
    /// in the child's control block and frees it when the task exits.
    ///
    /// `live` is the child's **retained live, mutable** user address space
    /// (`plans/PI.md` 5d-0-ii (b′)), the runtime-spawn analogue of the
    /// parameter [`InitSpawnCtx::admit_init`] takes: when [`Some`], the
    /// runtime owns it in the child's control block and publishes it on the
    /// per-CPU live-space slot while the child is switched in, so the
    /// child's `mem_map` / `mmio_map` syscalls mutate its own address space
    /// through [`crate::kthread::with_current_live_space`]. The producer
    /// builds it from the *same* arch address space it froze into `space`. A
    /// producer that retains no live space passes [`None`] and the child's
    /// `mem_map` / `mmio_map` fail closed.
    ///
    /// This does **not** enter user mode or step the scheduler: it returns
    /// the new PID and the caller resumes. Every failure reclaims what it
    /// built and returns an [`AdmitError`].
    ///
    /// # Safety
    ///
    /// `space` must faithfully describe the isolated user mappings the
    /// producer just built and `physmap` must back them, so the copy path
    /// reads exactly the memory the program sees; `pre_resume` must
    /// activate that space's root before the task is first entered.
    /// `stack` must be a region exclusive to the child that stays mapped
    /// (its guard page aside) and valid for as long as the task lives.
    /// When `live` is [`Some`] it must wrap the same arch address space
    /// `space` was frozen from.
    #[allow(clippy::too_many_arguments)]
    unsafe fn admit_process(
        &self,
        caps: CapabilitySet,
        space: Box<dyn UserAddressSpace + Send + Sync>,
        physmap: Box<dyn PhysMap + Send + Sync>,
        stack: Box<dyn crate::kthread::KernelStack + Send>,
        pre_resume: Box<dyn FnMut(u64) + Send>,
        live: Option<Box<dyn LiveUserSpace + Send>>,
        enter: Box<dyn FnMut() + Send>,
    ) -> Result<u64, AdmitError>;
}

/// The architecture-specific seam that builds a fresh, hardware-isolated
/// address space from a validated `rxe` and admits it as a runnable
/// process (`plans/SPAWN.md` SP3).
///
/// Installed into the syscall handler through
/// [`KernelSyscallHandlers::with_spawn`](crate::KernelSyscallHandlers::with_spawn),
/// exactly as the console list is installed through `with_consoles`. It
/// defaults to
/// [`NULL_PROCESS_SPAWN`], which fails closed with
/// [`Errno::NotImplemented`] until an arch port wires a
/// real producer. The producer builds the image through the production,
/// capability-checked, audited [`spawn_image`] caller — spawning is *not*
/// a privileged bypass: the child receives only the authority its manifest
/// requests intersected with its user's grants.
///
/// `Sync` because the installed producer is shared, immutably, by every
/// CPU's syscall dispatch path (the handler is held inside the `Sync`
/// [`crate::DispatchHook`]), exactly like the console device.
pub trait ProcessSpawn: Sync {
    /// Build `rxe` into a fresh isolated address space and admit it as a
    /// runnable process granted exactly `caps` and handed `args` and `env`
    /// as its startup argument vector and environment, returning its PID.
    ///
    /// This is the trait's single entry point, driven by two callers. The
    /// `spawn` syscall handler passes a registered [`EmbeddedProgram`]'s
    /// image and declared capability set with the effective startup strings
    /// it resolved (the caller-supplied block, or the program's registered
    /// defaults) — per-program authority, never the spawning caller's. A
    /// *driver* spawn passes the verified driver image's bytes, the
    /// manifest∩caller capability set the load gate already derived, and
    /// the argument vector the driver reads through `rustos_rt::arg`. The
    /// matched hardware-tree node's device-resource grants ride on `ctx`
    /// (the production context mints one owner-checked grant per requested
    /// resource as the child is registered); this seam never widens
    /// authority beyond `caps` plus those grants (no ambient authority) —
    /// `args` and `env` are data, not authority. The child's effective set
    /// is `caps` ∩ the spawn credential's user ceiling, derived at
    /// admission.
    ///
    /// Living on the trait lets a scheduler-agnostic caller (a generic
    /// `kernel_main` holding `&dyn ProcessSpawn`) spawn a process into its
    /// own hardware-isolated address space without naming the port's
    /// concrete spawn mechanism or the selected scheduler.
    ///
    /// The default fails closed with [`Errno::NotImplemented`]: a port that
    /// has not wired a spawn mechanism ([`NullProcessSpawn`]) refuses
    /// rather than pretending to spawn.
    ///
    /// # Errors
    ///
    /// Fails closed with a stable [`Errno`] on any error — a malformed
    /// `rxe`, a build or page-table-frame exhaustion, an unrunnable context,
    /// or an admission failure — never a panic or a half-built task.
    fn spawn_with(
        &self,
        rxe: &[u8],
        ctx: &dyn SpawnCtx,
        caps: CapabilitySet,
        args: &[&[u8]],
        env: &[&[u8]],
    ) -> Result<u64, Errno> {
        let _ = (rxe, ctx, caps, args, env);
        Err(Errno::NotImplemented)
    }
}

/// The fail-closed default [`ProcessSpawn`] producer: every build with no
/// real spawn service wired returns [`Errno::NotImplemented`], exactly as [`crate::NULL_CONSOLE`] does for the
/// `stream_write` syscall.
pub struct NullProcessSpawn;

impl ProcessSpawn for NullProcessSpawn {}

/// The shared [`NullProcessSpawn`] the syscall handler defaults to until
/// an arch port installs a real producer through
/// [`KernelSyscallHandlers::with_spawn`](crate::KernelSyscallHandlers::with_spawn).
pub static NULL_PROCESS_SPAWN: NullProcessSpawn = NullProcessSpawn;

/// Emit one structured audit record for `event` with `fields`.
fn emit(audit: &dyn Sink, event: AuditEvent, level: Level, fields: &[Field<'_>]) {
    rustos_log::log(
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
    use rustos_abi::rxe::{LoadHeader, RxePermission, Segment, LOAD_FLAG_PIE};
    use rustos_abi::{ABI_VERSION_CURRENT, LOAD_MAGIC, SYSCALL_TABLE_HASH_LEN};
    use rustos_kernel_mem::{
        AddressSpace, BootMemoryMap, HostPageTable, MemoryRegion, PhysAddr, RegionKind, SimPhysMap,
        UserStack, PAGE_SIZE,
    };
    use rustos_log::{set_max_level, Level};

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
        use rustos_kernel_mem::{LiveSpace, VirtAddr};

        /// A `PhysMap` view over the one leaked [`SimPhysMap`], so the
        /// image build and the teardown scrub touch the same simulated
        /// memory (each `sim()` owns disjoint storage).
        struct SharedSim(&'static SimPhysMap);
        impl PhysMap for SharedSim {
            fn translate(&self, phys: PhysAddr, len: usize) -> Option<core::ptr::NonNull<u8>> {
                self.0.translate(phys, len)
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

    impl SpawnCtx for StubCtx {
        fn frames(&self) -> &FrameAllocator {
            &self.frames
        }

        fn audit(&self) -> &(dyn Sink + Sync) {
            self.sink
        }

        unsafe fn admit_process(
            &self,
            _caps: CapabilitySet,
            _space: Box<dyn UserAddressSpace + Send + Sync>,
            _physmap: Box<dyn PhysMap + Send + Sync>,
            _stack: Box<dyn crate::kthread::KernelStack + Send>,
            _pre_resume: Box<dyn FnMut(u64) + Send>,
            _live: Option<Box<dyn LiveUserSpace + Send>>,
            _enter: Box<dyn FnMut() + Send>,
        ) -> Result<u64, AdmitError> {
            unreachable!("the default spawn_with returns before admitting a process")
        }
    }

    #[test]
    fn spawn_with_default_fails_closed_through_a_trait_object() {
        // A port that has not wired a driver-spawn mechanism (here the
        // shared `NullProcessSpawn`) must refuse `spawn_with` with
        // `NotImplemented` rather than pretending to spawn — reached through
        // `&dyn ProcessSpawn`, the path the generic boot wiring uses.
        let ctx = StubCtx::new();
        let producer: &dyn ProcessSpawn = &NULL_PROCESS_SPAWN;
        let result = producer.spawn_with(b"unused-rxe", &ctx, CapabilitySet::empty(), &[], &[]);
        assert_eq!(result, Err(Errno::NotImplemented));
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
            &NULL_PROCESS_SPAWN,
            b"unused-rxe",
            CapabilitySet::empty(),
            &[],
            &[],
            None,
        );
        assert_eq!(result, Err(Errno::NotImplemented));
    }
}
