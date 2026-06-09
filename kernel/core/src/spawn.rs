//! Capability-checked, audited process-spawn caller.
//!
//! [`rustos_kernel_mem::build_process_image`] is the architecture-neutral
//! *memory mechanism*: given a validated [`rustos_abi::rxe::LoadImage`] it
//! materialises a runnable user address space (segments mapped and filled,
//! a zeroed user stack, and the `rustos_abi::process` startup-vector block)
//! and reports the [`rustos_kernel_mem::ProcessImage`] register state. It is
//! deliberately capability-agnostic and never logs (`AGENTS.md` §17.4 —
//! `kernel/mem` does not depend on the security policy or `lib/log`).
//!
//! This module is the *policy* half: the one path that authorises a spawn,
//! audits the decision, builds the image, and drops the calling CPU into the
//! new program through the Arch HAL [`EnterUser`] primitive
//! (`AGENTS.md` §17.2). Keeping the capability gate and the audit record
//! here — in the caller, not in `kernel/mem` — is what preserves the §17.4
//! layering while still satisfying §5.4 (capability check before any state
//! touch) and §5.4.4 (security-relevant decisions are audited).
//!
//! # Security
//!
//! Spawning a program is privileged: it materialises a new principal's
//! address space and hands it the CPU. [`spawn_and_enter`] therefore
//! requires the caller to hold [`CapabilityId::PROC_SPAWN`] and fails closed
//! (`AGENTS.md` §4 — no ambient authority; §2.9 — fail closed) — the check
//! happens *before* `build_process_image` touches any page table. The hosted
//! program still receives only the capabilities its own signed manifest
//! requests intersected with its user's grants (`AGENTS.md` §16.5); this gate
//! authorises the *act* of spawning, it does not widen the new program's
//! authority.

use alloc::boxed::Box;

use rustos_abi::rxe::LoadImage;
use rustos_abi::{CapabilityId, CapabilityQuery, Errno};
use rustos_arch_api::{EnterUser, UserEntry};
use rustos_caps::CapabilitySet;
use rustos_kernel_mem::{
    build_process_image, AddressSpace, Frame, FrameAllocator, PageTable, PhysMap, SpawnError,
    UserAddressSpace, UserStack,
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
/// of which `kernel/core` can spell (`AGENTS.md` §17.2 / §17.4). So
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
/// authority its manifest requests intersected with its user's grants
/// (`AGENTS.md` §4, §16.5).
pub trait InitSpawn {
    /// Build PID 1's EL0 image and hand it to [`InitSpawnCtx::admit_init`]
    /// for registration + entry. Diverges into user mode on success;
    /// returns only when PID 1 could not be spawned, so the caller halts
    /// fail-closed (`AGENTS.md` §2.9).
    ///
    /// Called exactly once, on the boot CPU, after every init phase has
    /// succeeded — so the MMU is enabled and the user→kernel trap path is
    /// installed (the new program's first syscall is therefore handled
    /// rather than faulting).
    fn spawn_init(&self, ctx: &dyn InitSpawnCtx);
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
/// boundary — neither names the other's generics (`AGENTS.md` §17.2 /
/// §17.4).
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
    /// stays hardware-isolated from any sibling process (`AGENTS.md` §4).
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
    /// concrete stack source never leaks into this object-safe boundary
    /// (`AGENTS.md` §17.4). The seam supplies either the heap-backed
    /// software-canary [`crate::BoxStack`] or an arena-backed stack whose
    /// guard page it has **unmapped in PID 1's own page-table root**, so an
    /// overrun of PID 1's kernel stack takes a synchronous fault under PID
    /// 1's translation regime rather than corrupting a neighbour
    /// (`plans/PI.md` guard-page fault-form; `AGENTS.md` §4 / §2.17). The
    /// runtime stores it in PID 1's control block and frees it when the task
    /// exits.
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
    /// and valid for as long as the task lives.
    unsafe fn admit_init(
        &self,
        caps: CapabilitySet,
        space: Box<dyn UserAddressSpace + Send + Sync>,
        physmap: Box<dyn PhysMap + Send + Sync>,
        stack: Box<dyn crate::kthread::KernelStack + Send>,
        pre_resume: Box<dyn FnMut(u64) + Send>,
        enter: Box<dyn FnMut() + Send>,
    );
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
    /// space was built (`AGENTS.md` §5.4 — fail closed).
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
    /// The validated `rxe` load image (holding one is proof the §19.2
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
    /// Per-process random seed for the §19.2 stack canary.
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
///    record if not — *before* any page table is touched (`AGENTS.md` §5.4);
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
/// that work without duplicating the authorise/build/audit logic
/// (`AGENTS.md` §2.2). [`spawn_and_enter`] is the no-interposition case:
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
    // Step 2 (AGENTS.md §5.4) — capability check before any state touch.
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
                value: spawn_error_cause(error),
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
            value: format_hex_u64(image.entry, &mut entry_buf),
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
/// produces for PID 1 `init` (`AGENTS.md` §2.2 — one conversion path);
/// holding a valid [`LoadImage`] parsed from them is proof the §19.2
/// load-time invariants hold, so the spawn producer re-parses against the
/// kernel's compiled-in syscall CFI tag and fails closed on a mismatch.
#[derive(Clone, Copy)]
pub struct EmbeddedProgram {
    /// Absolute path the program is registered (and looked up) under.
    pub path: &'static [u8],
    /// The validated `rxe` image bytes.
    pub rxe: &'static [u8],
}

/// Capability-agnostic, path-keyed registry of the embedded programs the
/// kernel can spawn (`plans/SPAWN.md` SP3).
///
/// Threaded into the syscall handler like the [`crate::ConsoleWrite`]
/// console seam: it boots [`EMPTY`](Self::EMPTY), so a `spawn` of any path
/// fails closed with [`Errno::NotFound`] until the kernel binary registers
/// its embedded programs (the host-only `elf2rxe` build glue, `AGENTS.md`
/// §2.2). It is pure data with no ambient authority and no audit sink of
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

    /// The validated `rxe` bytes registered under `path`, or [`None`] if
    /// no embedded program bears that exact path.
    ///
    /// The match is exact (a byte-for-byte absolute path); there is no
    /// prefix or alias resolution, so a path either names exactly one
    /// registered program or nothing at all (fail closed, `AGENTS.md`
    /// §2.1).
    #[must_use]
    pub fn lookup(&self, path: &[u8]) -> Option<&'static [u8]> {
        self.programs.iter().find(|p| p.path == path).map(|p| p.rxe)
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
/// until the kernel binary installs a populated one (`AGENTS.md` §2.9 —
/// fail closed; mirrors [`crate::NULL_CONSOLE`]).
pub static EMPTY_PROGRAM_REGISTRY: ProgramRegistry = ProgramRegistry::EMPTY;

/// Why admitting a freshly built process as a runnable task failed.
///
/// The [`ProcessSpawn`] producer maps each variant onto a stable
/// [`Errno`] for the `spawn` syscall's caller; the partially built
/// resources are reclaimed before returning (`AGENTS.md` §2.9).
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
/// page table + [`EnterUser`] primitive, which `kernel/core` cannot spell,
/// `AGENTS.md` §17.2 / §17.4) and hands it back through
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

    /// The boot audit sink the build path records `ProcessSpawn*` events
    /// through.
    fn audit(&self) -> &(dyn Sink + Sync);

    /// Register the freshly built process as a runnable (**Ready**) task
    /// with the scheduler (as a resumable user kthread, `plans/SPAWN.md`
    /// SP2), the capability table (`caps`, the manifest∩user-grant set),
    /// and the address-space registry (`space` + `physmap`, under the same
    /// numeric id the dispatcher recovers so the child's first user-memory
    /// copy resolves its own mappings), and return its PID.
    ///
    /// `enter` is the arch-specific user-mode transition boxed as a
    /// `FnMut()` (it diverges, so its `!` coerces to `()`); it becomes the
    /// task's kthread work body, run once on the task's first dispatch.
    /// `pre_resume` reactivates the task's page-table root before every
    /// switch into it, keeping it hardware-isolated from its siblings
    /// (`AGENTS.md` §4). It is handed the task's own kernel-stack top so a
    /// port whose syscall entry does not implicitly resume on that stack
    /// (x86_64) can repoint its per-CPU entry stack at it (`plans/PI.md`
    /// §X); aarch64 reuses `SP_EL1` and ignores the argument.
    ///
    /// `stack` is the child's kernel stack, built by the arch seam so the
    /// concrete stack source never leaks into this object-safe boundary
    /// (`AGENTS.md` §17.4), exactly as [`InitSpawnCtx::admit_init`] takes it.
    /// The seam supplies either the heap-backed software-canary
    /// [`crate::BoxStack`] or an arena-backed stack whose guard page it has
    /// **unmapped in the child's own page-table root**, so an overrun of the
    /// child's kernel stack takes a synchronous fault under the child's
    /// translation regime rather than corrupting a neighbour (`plans/PI.md`
    /// guard-page fault-form; `AGENTS.md` §4 / §2.17). The runtime stores it
    /// in the child's control block and frees it when the task exits.
    ///
    /// This does **not** enter user mode or step the scheduler: it returns
    /// the new PID and the caller resumes. Every failure reclaims what it
    /// built and returns an [`AdmitError`] (`AGENTS.md` §2.9).
    ///
    /// # Safety
    ///
    /// `space` must faithfully describe the isolated user mappings the
    /// producer just built and `physmap` must back them, so the copy path
    /// reads exactly the memory the program sees; `pre_resume` must
    /// activate that space's root before the task is first entered.
    /// `stack` must be a region exclusive to the child that stays mapped
    /// (its guard page aside) and valid for as long as the task lives.
    unsafe fn admit_process(
        &self,
        caps: CapabilitySet,
        space: Box<dyn UserAddressSpace + Send + Sync>,
        physmap: Box<dyn PhysMap + Send + Sync>,
        stack: Box<dyn crate::kthread::KernelStack + Send>,
        pre_resume: Box<dyn FnMut(u64) + Send>,
        enter: Box<dyn FnMut() + Send>,
    ) -> Result<u64, AdmitError>;
}

/// The architecture-specific seam that builds a fresh, hardware-isolated
/// address space from a validated `rxe` and admits it as a runnable
/// process (`plans/SPAWN.md` SP3).
///
/// Installed into the syscall handler through
/// [`KernelSyscallHandlers::with_spawn`](crate::KernelSyscallHandlers::with_spawn),
/// exactly as the console device is installed through `with_console`. It
/// defaults to
/// [`NULL_PROCESS_SPAWN`], which fails closed with
/// [`Errno::NotImplemented`] (`AGENTS.md` §2.9) until an arch port wires a
/// real producer. The producer builds the image through the production,
/// capability-checked, audited [`spawn_image`] caller — spawning is *not*
/// a privileged bypass: the child receives only the authority its manifest
/// requests intersected with its user's grants (`AGENTS.md` §4, §16.5).
///
/// `Sync` because the installed producer is shared, immutably, by every
/// CPU's syscall dispatch path (the handler is held inside the `Sync`
/// [`crate::DispatchHook`]), exactly like the console device.
pub trait ProcessSpawn: Sync {
    /// Build the program in `rxe` into a fresh isolated address space and
    /// admit it as a runnable process through `ctx`, returning its PID.
    ///
    /// # Errors
    ///
    /// Fails closed with a stable [`Errno`] on any error — a malformed
    /// `rxe`, a build failure, an unrunnable context, or an admission
    /// failure — never a panic or a half-built task (`AGENTS.md` §2.9).
    fn spawn(&self, rxe: &[u8], ctx: &dyn SpawnCtx) -> Result<u64, Errno>;
}

/// The fail-closed default [`ProcessSpawn`] producer: every build with no
/// real spawn service wired returns [`Errno::NotImplemented`]
/// (`AGENTS.md` §2.9), exactly as [`crate::NULL_CONSOLE`] does for the
/// `stream_write` syscall.
pub struct NullProcessSpawn;

impl ProcessSpawn for NullProcessSpawn {
    fn spawn(&self, _rxe: &[u8], _ctx: &dyn SpawnCtx) -> Result<u64, Errno> {
        Err(Errno::NotImplemented)
    }
}

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
        AddressSpace, HostPageTable, PhysAddr, SimPhysMap, UserStack, PAGE_SIZE,
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
}
