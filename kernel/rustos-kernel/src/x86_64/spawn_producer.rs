//! x86_64 runtime `spawn` producer — `plans/PI.md` `X3b`.
//!
//! [`X86_64ProcessSpawn`] implements the architecture-neutral
//! [`rustos_kernel_core::ProcessSpawn`] seam the boot pipeline installs into
//! the [`rustos_kernel_core::BootInfo`] hand-off (`boot::try_boot` →
//! `BootInfo::with_spawn`). It is the cross-port sibling of the aarch64
//! `spawn_producer` (`plans/SPAWN.md` `SP3b`): when a task that holds
//! `CAP_PROC_SPAWN` issues the `spawn` syscall, the kernel resolves the
//! requested path against the shared
//! [`crate::spawn_layout::PROGRAM_REGISTRY`] and hands the matching
//! `rxe` bytes to [`ProcessSpawn::spawn`], which builds the child a *fresh,
//! hardware-isolated* PML4 hierarchy, populates it through the production
//! capability-checked, audited spawn caller ([`spawn_image`], gated on
//! `CAP_PROC_SPAWN`), and admits it **Ready** through
//! [`SpawnCtx::admit_process`]. Unlike the PID-1 [`InitSpawn`] seam
//! (`init_spawn_x86_64.rs`) it does **not** switch CR3 or enter ring 3: the
//! spawning caller keeps running under its own root, and the child runs when
//! the scheduler next steps it (a true concurrent spawn, not an `exec`-style
//! hand-off).
//!
//! # Building the child without switching CR3
//!
//! The runtime `spawn` syscall is issued by PID 1 `init`, so the producer runs
//! under PID 1's own root, which [`init_spawn`](crate::x86_64::init_spawn)
//! built with [`ArchAddressSpace::new_identity_first_gib`]: it identity-maps the
//! low [`IDENTITY_GIB`] GiB **and** mirrors the higher-half kernel window. The
//! x86_64 page-table walk recovers each existing intermediate table from its
//! **low physical address** (`paging::ensure_child`) and writes each new table
//! through its higher-half static pointer, and the image content is written
//! through a higher-half [`DirectPhysMap`] — all three are mapped under PID 1's
//! active root (the child pool is a `.bss` static and the live allocator's
//! frames are in the low identity window). So the producer builds the child's
//! tables *through the caller's active CR3*, never switching it, exactly as the
//! aarch64 producer builds through its identity window. The child's own CR3 is
//! reloaded by its `pre_resume` hook before the scheduler first resumes it
//! (`plans/SPAWN.md` SP2, `plans/PI.md` X1).
//!
//! Spawning is *not* a privileged bypass: the child receives only the authority
//! its registered program declares intersected with its user's grants; this seam only authorises the *act* of spawning
//! under `CAP_PROC_SPAWN`.

use alloc::boxed::Box;

use rustos_abi::rxe::LoadImage;
use rustos_abi::Errno;
use rustos_arch_api::mmu::AddressSpace as MmuAddressSpace;
use rustos_arch_api::EnterUser;
use rustos_arch_x86_64::paging::{
    activate_user_root, AddressSpace as ArchAddressSpace, KERNEL_VMA_BASE,
};
use rustos_arch_x86_64::syscall_entry;
use rustos_arch_x86_64::userentry::UserMode;
use rustos_kernel_core::{
    spawn_image, AdmitError, BoxStack, EmbeddedProgram, KernelStack, ProcessSpawn,
    SpawnCallerError, SpawnCtx, SpawnRequest,
};
use rustos_kernel_mem::{
    AddressSpace, DirectPhysMap, FrameAllocator, FrameTableSource, LiveSpace, LiveUserSpace,
    PhysMap, UserAddressSpace, UserStack, VirtAddr,
};
use rustos_kernel_syscall::SYSCALL_TABLE_HASH;
use rustos_sync::Once;

use crate::spawn_layout::{self, SHELL_USER_BIAS};
use crate::stack_arena::{FrameArenaGrow, KTHREAD_STACK_ARENA};

/// Logical CPU the boot processor runs as — the single core the (c7-bin)
/// bring-up initialises and the one [`syscall_entry::set_kernel_rsp0`]
/// repoints (mirrors `init_spawn_x86_64::BOOT_CPU`).
const BOOT_CPU: usize = 0;

/// Span of the direct physical map the spawn build writes frame contents
/// through: the `[0, 1 GiB)` physical window the boot trampoline mirrors at
/// [`KERNEL_VMA_BASE`] (`boot.s` SAFETY-INVARIANT 9), the same window PID 1
/// uses (`init_spawn_x86_64`).
const PHYSMAP_SPAN: u64 = 1 << 30;

/// Gigabytes of identity map each spawned child address space provides.
///
/// 4 GiB mirrors the boot trampoline's identity map and PID 1's space: it
/// covers all of the platform's RAM (so the page-table walk's low-physical
/// table dereferences and the live allocator's image frames resolve) and the
/// architectural LAPIC MMIO page at ~3.98 GiB (so the scheduler's LAPIC
/// accesses stay valid under the child's CR3 once it is resumed).
/// [`SHELL_USER_BIAS`] (64 GiB) sits far above it, so the program's pages land
/// on freshly walked tables rather than colliding with an identity huge page
/// — the same window PID 1 uses.
const IDENTITY_GIB: usize = 4;

/// User stack base: the shared [`spawn_layout::USER_STACK_OFFSET`] above
/// this image's bias, mirroring the PID-1 layout (the layout offsets and
/// sizes are shared across the ports in [`crate::spawn_layout`]).
const USER_STACK_BASE: u64 = SHELL_USER_BIAS + spawn_layout::USER_STACK_OFFSET;
/// User virtual address the startup-vector block is written at.
const USER_BLOCK_BASE: u64 = SHELL_USER_BIAS + spawn_layout::USER_BLOCK_OFFSET;
/// Base of a spawned child's device-window virtual region
/// (`plans/PI.md` 5d-0-ii (b′)): the retained [`LiveSpace`]'s
/// [`rustos_kernel_mem::MmioWindowMap`] hands each `mmio_map` a
/// guard-bracketed window out of `[MMIO_WINDOW_BASE, MMIO_WINDOW_BASE +
/// MMIO_WINDOW_PAGES·4 KiB)`.
const MMIO_WINDOW_BASE: u64 = SHELL_USER_BIAS + spawn_layout::MMIO_WINDOW_OFFSET;
/// Base of a spawned child's non-`FIXED` anonymous-heap virtual region
/// (`plans/PI.md` 5d-0-ii (c)): the retained [`LiveSpace`]'s
/// [`rustos_kernel_mem::AnonWindowMap`] places each non-`FIXED` `mem_map`
/// out of `[ANON_WINDOW_BASE, ANON_WINDOW_BASE + ANON_WINDOW_PAGES·4 KiB)`.
const ANON_WINDOW_BASE: u64 = SHELL_USER_BIAS + spawn_layout::ANON_WINDOW_OFFSET;
/// Base of a spawned child's guarded DMA-buffer virtual region
/// (`plans/PI.md` 5d-0-ii (c) DMA half): the retained [`LiveSpace`]'s
/// [`rustos_kernel_mem::DmaWindowMap`] carves each `dma_alloc` buffer out of
/// `[DMA_WINDOW_BASE, DMA_WINDOW_BASE + DMA_WINDOW_PAGES·4 KiB)`.
const DMA_WINDOW_BASE: u64 = SHELL_USER_BIAS + spawn_layout::DMA_WINDOW_OFFSET;
/// Base of a spawned child's cross-process shared-memory virtual region:
/// the retained [`LiveSpace`]'s shared-window allocator maps each granted
/// `shm_map` region out of `[SHARED_WINDOW_BASE, SHARED_WINDOW_BASE +
/// SHARED_WINDOW_PAGES * 4 KiB)`.
const SHARED_WINDOW_BASE: u64 = SHELL_USER_BIAS + spawn_layout::SHARED_WINDOW_OFFSET;

/// Identity direct map the page-table frame source translates a freshly
/// allocated frame's physical address through to a CPU-dereferenceable
/// pointer (`plans/WIRING.md` W5b-3).
///
/// It is the **identity** map (`offset == 0`) covering the same
/// `[0, IDENTITY_GIB GiB)` low window each child space identity-maps —
/// distinct from [`PHYSMAP_SPAN`]'s higher-half map used to write the
/// child's *image* contents — because the x86_64 page-table walk recovers
/// an existing child table by dereferencing its low physical address
/// directly (`paging::ensure_child`: `phys as *mut`, identity under the
/// active CR3's low-4 GiB identity map), so the frame view the source
/// hands the port must satisfy `virtual == physical`. A frame the
/// allocator draws from outside this window fails the translate and the
/// spawn fails closed — the same window the child's
/// image data frames resolve under.
static SPAWN_TABLE_PHYSMAP: DirectPhysMap = DirectPhysMap::identity((IDENTITY_GIB as u64) << 30);

/// The higher-half kernel direct map (`KERNEL_VMA_BASE + phys`, the same
/// `[KERNEL_VMA_BASE, KERNEL_VMA_BASE + PHYSMAP_SPAN)` window the spawn path
/// writes a child's image through) the kernel core hands the shared-memory
/// facility to scrub region frames through (`plans/USB.md`).
///
/// Unlike [`SPAWN_TABLE_PHYSMAP`] (the low identity window used only for the
/// page-table walk), this is the map through which the kernel reaches *any*
/// RAM frame the allocator hands out, so it is the correct view for the
/// region zero-on-free scrub.
pub static SHM_PHYSMAP: DirectPhysMap = DirectPhysMap::new(KERNEL_VMA_BASE, PHYSMAP_SPAN);

/// The single, `'static` allocator-backed page-table frame source every
/// spawned child's PML4 hierarchy is built from.
///
/// This replaces the former fixed `[PageTablePool; 8]` `.bss` reserve that
/// hard-capped the runtime `spawn` syscall at eight live processes — a
/// capacity ceiling that wasted RAM on a small machine and starved a
/// large one. Page-table frames now come from the kernel's live
/// [`FrameAllocator`] through [`FrameTableSource`], so the spawn capacity
/// **scales with discovered RAM and grows on demand**, failing closed with
/// [`Errno::NoSpace`] only when physical RAM is genuinely exhausted
/// (deterministic OOM, never a panic). The frames
/// are never freed while a child lives (the monotonic discipline the pool
/// used); reclaiming a dead process's page-table frames
/// is a later stage. Mirrors the aarch64 producer.
///
/// Initialised on the first `spawn` from the boot-threaded `'static`
/// allocator and reused thereafter — the source is stateless (its state
/// lives in the allocator), so one shared instance serves every CPU.
static SPAWN_FRAME_SOURCE: Once<FrameTableSource> = Once::new();

/// Borrow the `'static` allocator-backed page-table frame source,
/// initialising it from `frames` on the first call.
///
/// Fails closed with [`Errno::NotImplemented`] if the one-shot initialiser
/// was poisoned by a panicking earlier attempt — [`FrameTableSource::new`]
/// cannot panic, so this is unreachable in practice, but it is never
/// papered over.
fn page_table_source(frames: &'static FrameAllocator) -> Result<&'static FrameTableSource, Errno> {
    SPAWN_FRAME_SOURCE
        .call_once_infallible(|| FrameTableSource::new(frames, &SPAWN_TABLE_PHYSMAP))
        .map_err(|_| Errno::NotImplemented)
}

/// The x86_64 runtime `spawn` producer installed into the
/// [`rustos_kernel_core::BootInfo`] hand-off by `boot::try_boot`.
pub struct X86_64ProcessSpawn;

/// The single, `'static` [`X86_64ProcessSpawn`] the boot path borrows.
pub static X86_64_PROCESS_SPAWN: X86_64ProcessSpawn = X86_64ProcessSpawn;

impl ProcessSpawn for X86_64ProcessSpawn {
    fn spawn(&self, program: &EmbeddedProgram, ctx: &dyn SpawnCtx) -> Result<u64, Errno> {
        // The child's PML4 hierarchy is drawn from the kernel's live frame
        // allocator: there is no fixed page-table reserve
        // and so no hard cap on how many processes can be spawned — the
        // capacity scales with discovered RAM and grows on demand. A build
        // with no `'static` allocator wired fails closed,
        // as does genuine RAM exhaustion below.
        let pt_frames = ctx.page_table_allocator().ok_or(Errno::NoSpace)?;
        let table_frames = page_table_source(pt_frames)?;
        // Publish the same `'static` allocator the kthread-stack arena returns
        // idle chained blocks to when a spawned task later exits and its
        // `ArenaStack` is dropped (the capacity shrinks as
        // well as grows). Idempotent (set-once); the boot path threads one
        // allocator, so every spawn publishes the same handle (mirrors the
        // aarch64 producer).
        crate::stack_arena::publish_reclaim_frames(pt_frames);

        // Build a PML4 identity-mapping the low `IDENTITY_GIB` GiB (RAM + the
        // LAPIC MMIO page) and the higher-half kernel window, and capture its
        // root *without* switching CR3: the spawning caller (PID 1) stays
        // active under its own root, so the running parent is never moved out
        // from under itself. The child's tables and image are written through
        // the caller's active root — which identity-maps the live allocator's
        // page-table and image frames in the low window and mirrors the
        // higher-half kernel window the page-table walk and `DirectPhysMap`
        // use — so the build does not require the child space to be active.
        // The child's own CR3 is reloaded by its `pre_resume` hook before the
        // scheduler first resumes it (`plans/SPAWN.md` SP2, `plans/PI.md` X1).
        let mut arch = ArchAddressSpace::new_identity_first_gib(table_frames, IDENTITY_GIB)
            .ok_or(Errno::NoSpace)?;
        let child_root_phys = arch.pml4_phys();

        // Build the child's kernel stack (`plans/PI.md` G3b-2-ii, mirroring
        // the PID-1 `init_spawn_x86_64` seam). The boot path carved a guard
        // arena out of firmware-usable RAM and published it to the kthread-
        // stack allocator; if a region is available, re-express the coarse
        // identity block covering its guard page at 4 KiB granularity in the
        // *child's own* PML4 and unmap that single page, so an overrun of the
        // child's kernel stack takes a synchronous page fault under the
        // child's CR3 rather than corrupting the lower-addressed neighbour
        // (the real guard-page fault-form). Doing it on
        // `arch` — which is *never switched to* here (the spawning caller
        // keeps its own CR3) — disturbs no live access: the child tables live
        // in the caller's low identity window, so `split_block` reads/writes
        // them through identity addresses, only adds table levels reproducing
        // the existing translation, and needs no TLB maintenance (the child's
        // root is not active). The arena grows on demand by chaining fresh
        // 2 MiB blocks out of the kernel's live frame allocator when its
        // boot-carved block is exhausted, so the number of
        // hardware-guarded child stacks scales with discovered RAM; a chained
        // block is bounded to the identity window so the stack stays mapped in
        // the child's own root. If no arena region is available, or the
        // split/unmap could not be applied, fall back to a heap-backed
        // software-canary `BoxStack` rather than ever running on an unguarded
        // stack (fail closed).
        let grow = FrameArenaGrow::new(ctx.frames(), (IDENTITY_GIB as u64) << 30);
        let kernel_stack: Box<dyn KernelStack + Send> =
            match KTHREAD_STACK_ARENA.alloc(&grow, &crate::stack_arena::IdentityBlockStore) {
                Some(stack) => {
                    let guard = stack.guard_page();
                    match arch
                        .split_block(guard)
                        .and_then(|()| arch.unmap(guard).map(|_| ()))
                    {
                        Ok(()) => Box::new(stack),
                        Err(_) => Box::new(BoxStack::new()),
                    }
                }
                None => Box::new(BoxStack::new()),
            };

        let mut space = AddressSpace::new(arch);
        let physmap = DirectPhysMap::new(KERNEL_VMA_BASE, PHYSMAP_SPAN);

        // Parse the build-time `rxe` blob against the kernel's own compiled-in
        // syscall CFI tag. A mismatch fails closed; the registry
        // holds bytes that already parsed once at build time, so reaching this
        // is a kernel build defect, surfaced as a stable errno.
        let image =
            LoadImage::parse(program.rxe, &SYSCALL_TABLE_HASH).map_err(|_| Errno::BadMagic)?;

        let request = SpawnRequest {
            image: &image,
            image_bytes: program.rxe,
            bias: SHELL_USER_BIAS,
            stack: UserStack {
                base: USER_STACK_BASE,
                page_count: spawn_layout::USER_STACK_PAGES,
            },
            start_block_base: USER_BLOCK_BASE,
            args: program.args,
            env: &[],
            canary: spawn_layout::CHILD_CANARY,
        };

        // Authorise + build the child's ring-3 image (emits `ProcessSpawned`).
        // SAFETY: building the image is itself safe; the returned `UserEntry`
        // is only entered later, once the child is dispatched and its
        // `pre_resume` hook has made `space` active (the `spawn_image`
        // contract). The frame source draws first-GiB RAM frames from the
        // kernel's live allocator, written through the higher-half `physmap`
        // mapped under the caller's active root. A returning `Err` reclaims
        // nothing user-visible (the page-table + image frames are handed out
        // monotonically and not reclaimed this stage) and maps to a stable
        // errno; the cause is already audited by `spawn_image`.
        let frames = ctx.frames();
        let entry = unsafe {
            spawn_image(
                &spawn_layout::SpawnAuthority,
                ctx.audit(),
                &mut space,
                &physmap,
                &request,
                move || frames.alloc().ok(),
            )
        }
        .map_err(spawn_caller_errno)?;

        // The child's user-address-space reactivation hook (`plans/SPAWN.md`
        // SP2, `plans/PI.md` X1): the core runs it on the dispatcher's context
        // immediately before every switch into the child. It reloads CR3 to
        // the child's own root (isolation) and repoints the per-CPU
        // `syscall` entry stack at the child's own kernel stack (the value the
        // runtime hands it). It captures only the `u64` root, so it is `Send`.
        let pre_resume: Box<dyn FnMut(u64) + Send> = Box::new(move |stack_top: u64| {
            // `set_kernel_rsp0` repoints **both** the child's `syscall` entry
            // stack (`gs:0`) and its trap entry stack (`TSS.RSP0`) at the
            // child's own kernel stack — the latter is what makes an involuntary
            // LAPIC-timer preemption (P-1c), delivered through the IDT interrupt
            // gate which reads `TSS.RSP0`, land on the child's own stack rather
            // than corrupt a concurrently parked task's frame (
            // — one per-task kernel stack for both entry kinds). A rejected
            // value (validated canonical/aligned/kernel-half) leaves the slots
            // unchanged and the next entry faults loudly (fail closed).
            let _ = syscall_entry::set_kernel_rsp0(BOOT_CPU, stack_top);
            // SAFETY: paging is enabled and `child_root_phys` is the PML4 of
            // the child's space, which identity-maps the low kernel window the
            // running dispatcher executes from and mirrors the higher-half
            // kernel window — exactly `activate_user_root`'s contract.
            unsafe { activate_user_root(child_root_phys) };
        });

        // The user-mode transition, boxed for the scheduler task body the core
        // wraps the child in. `enter_user` diverges into ring 3, so the
        // closure never truly returns (its `!` coerces to `()`).
        let user_mode = UserMode::new();
        let enter: Box<dyn FnMut() + Send> = Box::new(move || {
            // SAFETY: by the time this body runs the child has been
            // dispatched, so its `pre_resume` hook has activated `space` and
            // the `syscall` entry + production dispatch callback are installed;
            // the child's first `syscall` is handled. `build_process_image`
            // mapped the entry/stack as user pages.
            unsafe { user_mode.enter_user(entry) }
        });

        // Freeze the just-built mappings into the registry-storable,
        // `Send + Sync` snapshot the kernel-wide address-space registry holds
        // (the live arch `space` is not `Sync`), and box the direct map that
        // backs it, so the child's `stream_write` can copy its banner out of
        // its own user memory. Freezing *after* `spawn_image` captures every
        // mapped page — segments, stack, and the startup-vector block.
        let frozen: Box<dyn UserAddressSpace + Send + Sync> = Box::new(space.freeze());

        // Retain the live, mutable arch space behind the object-safe
        // `LiveUserSpace` boundary so the child's `mem_map` / `mmio_map` /
        // `dma_alloc` syscalls mutate *its own* address space (`plans/PI.md`
        // 5d-0-ii (b′)), the cross-port sibling of the aarch64 producer. The `LiveSpace` composes the audited
        // anonymous-map mechanism (over the kernel's `'static` frame
        // allocator) and the guarded device-window allocator (over the
        // `[MMIO_WINDOW_BASE, …)` region); it carries the *same* arch space
        // the snapshot above was frozen from, and zeroes anonymous frames
        // through a fresh higher-half [`DirectPhysMap`] identical to the one
        // the image build used (both the low identity and the higher-half
        // kernel window are mapped under the child's CR3). A build context
        // with no `'static` allocator, or a window the allocator rejects,
        // retains no live space and the child's `mem_map` / `mmio_map` fail
        // closed.
        let live: Option<Box<dyn LiveUserSpace + Send>> = match ctx.page_table_allocator() {
            Some(static_frames) => LiveSpace::new(
                space,
                DirectPhysMap::new(KERNEL_VMA_BASE, PHYSMAP_SPAN),
                static_frames,
                VirtAddr::new(MMIO_WINDOW_BASE),
                spawn_layout::MMIO_WINDOW_PAGES,
                VirtAddr::new(ANON_WINDOW_BASE),
                spawn_layout::ANON_WINDOW_PAGES,
                VirtAddr::new(DMA_WINDOW_BASE),
                spawn_layout::DMA_WINDOW_PAGES,
                VirtAddr::new(SHARED_WINDOW_BASE),
                spawn_layout::SHARED_WINDOW_PAGES,
            )
            .ok()
            .map(|live| Box::new(live) as Box<dyn LiveUserSpace + Send>),
            None => None,
        };

        let physmap: Box<dyn PhysMap + Send + Sync> = Box::new(physmap);

        // Register the child's caps + frozen address space and admit it Ready,
        // returning its PID. The spawning caller keeps running.
        // SAFETY: `frozen` faithfully describes the mappings just built into
        // `space`, `physmap` backs them, `pre_resume` activates the child's
        // root before it is first entered, `kernel_stack` is a region
        // exclusive to this child that stays mapped (in the low identity
        // window) for the task's lifetime, and `live` retains the same arch
        // space `frozen` was taken from — the `admit_process` contract.
        // The child receives exactly its registered program's declared
        // capability set — never the spawning
        // caller's authority.
        unsafe {
            ctx.admit_process(
                program.capability_set(),
                frozen,
                physmap,
                kernel_stack,
                pre_resume,
                live,
                enter,
            )
        }
        .map_err(admit_errno)
    }
}

/// Map a [`SpawnCallerError`] onto a stable [`Errno`] for the `spawn` syscall's
/// caller. The precise cause is already on the audit log via
/// the `ProcessSpawn*` events `spawn_image` emits. Identical to the aarch64
/// producer.
fn spawn_caller_errno(err: SpawnCallerError) -> Errno {
    match err {
        // A missing `CAP_PROC_SPAWN` (cannot occur — the dispatcher already
        // gated the syscall — but mapped for completeness).
        SpawnCallerError::Denied => Errno::PermissionDenied,
        // Image construction failed (a frame/pool exhaustion, a malformed
        // segment, an over-size startup block). One stable resource errno.
        SpawnCallerError::Build(_) => Errno::NoSpace,
        // `SpawnCallerError` is `#[non_exhaustive]`: any future variant fails
        // closed to the same stable resource errno.
        _ => Errno::NoSpace,
    }
}

/// Map an [`AdmitError`] onto a stable [`Errno`].
fn admit_errno(err: AdmitError) -> Errno {
    match err {
        AdmitError::SchedulerFull => Errno::NoSpace,
        AdmitError::AspaceConflict => Errno::AlreadyExists,
        // `AdmitError` is `#[non_exhaustive]`: any future variant fails closed
        // to a stable resource errno.
        _ => Errno::NoSpace,
    }
}
