//! riscv64 (QEMU `virt` / SiFive) runtime `spawn` producer —
//! `plans/PI.md` RV-P3 / `plans/SPAWN.md` `SP3b`.
//!
//! [`RiscvProcessSpawn`] implements the architecture-neutral
//! [`rustos_kernel_core::ProcessSpawn`] seam the boot pipeline installs into
//! the [`rustos_kernel_core::BootInfo`] hand-off
//! (`boot_riscv64::try_boot` → `BootInfo::with_spawn`). When a task that
//! holds `CAP_PROC_SPAWN` issues the `spawn` syscall, the kernel resolves
//! the requested path against the shared
//! [`crate::spawn_layout::PROGRAM_REGISTRY`] and hands the
//! matching `rxe` bytes to [`ProcessSpawn::spawn`], which builds the child a
//! *fresh, hardware-isolated* Sv39 address space, populates it through the
//! production capability-checked, audited spawn caller ([`spawn_image`],
//! gated on `CAP_PROC_SPAWN`), and admits it **Ready** through
//! [`SpawnCtx::admit_process`]. Unlike the PID-1 [`rustos_kernel_core::InitSpawn`]
//! seam (`init_spawn_riscv64.rs`) it does **not** switch the active
//! translation regime (`satp`) or enter user mode: the spawning caller keeps
//! running under its own root, and the child runs when the scheduler next
//! steps it (a true concurrent spawn, not an `exec`-style hand-off) — the riscv64 sibling of the `Aarch64ProcessSpawn` /
//! x86_64 producers.
//!
//! The child's `rxe`, its relocation bias ([`SHELL_USER_BIAS`]), and the
//! kernel's syscall CFI tag are baked at build time (`build.rs` →
//! `rustos_itest_harness::elf2rxe`), exactly like PID 1, so there is one
//! conversion path. Spawning is *not* a privileged
//! bypass: the child receives only the authority its registered program
//! declares intersected with its user's grants;
//! this seam only authorises the *act* of spawning under `CAP_PROC_SPAWN`.
//!
//! Each spawned child's kernel stack is drawn from the boot-reserved guard
//! arena (`plans/PI.md` G3b-2): the producer re-expresses the coarse
//! identity block covering the stack's guard page at 4 KiB granularity in
//! the *child's own* Sv39 root — which it builds but never switches to — and
//! unmaps that single page, so an overrun of the child's kernel stack takes
//! a synchronous store page fault under the child's `satp`, the cross-port mirror of the aarch64/x86_64 producers. Where no
//! arena region is available, or the split/unmap fails, it falls back to a
//! heap-backed software-canary [`BoxStack`] (fail closed).

use alloc::boxed::Box;

use rustos_abi::rxe::LoadImage;
use rustos_abi::Errno;
use rustos_arch_api::mmu::AddressSpace as MmuAddressSpace;
use rustos_arch_api::EnterUser;
use rustos_arch_riscv64::paging::{activate_user_root, AddressSpace as ArchAddressSpace};
use rustos_arch_riscv64::userentry::UserMode;
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

/// Gigabytes of identity map each spawned child address space provides.
///
/// `[0, 4 GiB)` covers the QEMU `virt` board's low device MMIO and the RAM
/// base at `0x8000_0000` (GiB 2), where the kernel image, its boot heap,
/// the leaked `KernelState`, and the live frame allocator all live. The
/// producer never switches to this space (the spawning caller keeps its own
/// `satp` active), so the identity map exists only so the child itself
/// executes under a translation regime that maps the low kernel window when
/// the scheduler later resumes it through `activate_user_root`.
/// [`SHELL_USER_BIAS`] (64 GiB) sits far above it, so the program's pages
/// land on freshly walked Sv39 tables rather than colliding with an identity
/// gigapage leaf — the same window PID 1 uses.
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
/// `[0, IDENTITY_GIB GiB)` window each child space identity-maps, because
/// the Sv39 page-table walk recovers an existing child table by
/// dereferencing its physical address directly (`paging`: identity), so the
/// frame view the source hands the port must satisfy `virtual == physical`.
/// A frame the allocator draws from outside this window fails the translate
/// and the spawn fails closed rather than building tables
/// the walk cannot reach — the same window the child's image data frames
/// already use.
pub static SPAWN_TABLE_PHYSMAP: DirectPhysMap =
    DirectPhysMap::identity((IDENTITY_GIB as u64) << 30);

/// The single, `'static` allocator-backed page-table frame source every
/// spawned child's Sv39 hierarchy is built from.
///
/// Page-table frames come from the kernel's live [`FrameAllocator`] through
/// [`FrameTableSource`], so the spawn capacity **scales with discovered RAM
/// and grows on demand**: each child draws only the handful of Sv39 tables
/// it needs, and the system spawns processes until physical RAM is genuinely
/// exhausted, when [`FrameTableSource::alloc_table`] returns `None` and the
/// build fails closed with [`Errno::NoSpace`] (deterministic OOM, never a panic). The frames are never freed while a
/// child lives (the monotonic discipline); reclaiming a
/// dead process's page-table frames is a later stage.
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

/// The riscv64 runtime `spawn` producer installed into the
/// [`rustos_kernel_core::BootInfo`] hand-off by `boot_riscv64::try_boot`.
pub struct RiscvProcessSpawn;

/// The single, `'static` [`RiscvProcessSpawn`] the boot path borrows.
pub static RISCV_PROCESS_SPAWN: RiscvProcessSpawn = RiscvProcessSpawn;

impl ProcessSpawn for RiscvProcessSpawn {
    fn spawn(&self, program: &EmbeddedProgram, ctx: &dyn SpawnCtx) -> Result<u64, Errno> {
        // The child's Sv39 hierarchy is drawn from the kernel's live frame
        // allocator: there is no fixed page-table reserve
        // and so no hard cap on how many processes can be spawned — the
        // capacity scales with discovered RAM and grows on demand. A build
        // with no `'static` allocator wired fails closed,
        // as does genuine RAM exhaustion below.
        let pt_frames = ctx.page_table_allocator().ok_or(Errno::NoSpace)?;
        let table_frames = page_table_source(pt_frames)?;
        // Publish the same `'static` allocator the kthread-stack arena
        // returns idle chained blocks to when a spawned task later exits and
        // its `ArenaStack` is dropped (the capacity
        // shrinks as well as grows). Idempotent (set-once); the boot path
        // threads one allocator, so every spawn publishes the same handle.
        crate::stack_arena::publish_reclaim_frames(pt_frames);

        // Build an Sv39 address space identity-mapping the kernel + MMIO, and
        // capture its root *without* switching to it: the spawning caller
        // stays active under its own `satp`, so the running parent is never
        // moved out from under itself. The child's mappings below are written
        // through the identity `physmap` (physical frame addresses the
        // caller's active identity space already maps), so the build does not
        // require the child space to be active. The child's own root is
        // reactivated by its `pre_resume` hook before the scheduler first
        // resumes it (`plans/SPAWN.md` SP2). An allocator exhausted of even
        // the root table fails closed with `NoSpace`.
        let mut arch = ArchAddressSpace::new_identity_gigapages(table_frames, IDENTITY_GIB)
            .ok_or(Errno::NoSpace)?;
        let child_root_phys = arch.root_phys();

        // Build the child's kernel stack (`plans/PI.md` G3b-2, mirroring
        // the PID-1 `init_spawn_riscv64` seam). The boot path published the
        // reserved guard arena to the kthread-stack allocator; if a region
        // is available, re-express the coarse identity block covering its
        // guard page at 4 KiB granularity in the *child's own* root and
        // unmap that single page, so an overrun of the child's kernel stack
        // takes a synchronous store page fault under the child's `satp`
        // rather than corrupting the lower-addressed neighbour (the real
        // guard-page fault-form). Doing it on `arch`
        // — which is *never switched to* here (the spawning caller keeps its
        // own `satp`) — disturbs no live access: the child pool is in the
        // caller's identity window, so `split_block` only reads/writes the
        // child's tables through identity addresses, only adds table levels
        // reproducing the existing translation, and needs no TLB maintenance
        // (the child's root is not active). If no arena region is available,
        // or the split/unmap could not be applied, fall back to a heap-backed
        // software-canary `BoxStack` rather than ever running on an unguarded
        // stack (fail closed).
        // The arena grows on demand by chaining fresh 2 MiB blocks out of
        // the kernel's live frame allocator when its boot-carved block is
        // exhausted, so the number of hardware-guarded
        // child stacks scales with discovered RAM rather than capping at
        // the boot block. A chained block is bounded to the identity window
        // so the stack stays mapped in the child's own root.
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
        let physmap = DirectPhysMap::identity((IDENTITY_GIB as u64) << 30);

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

        // Authorise + build the child's U-mode image (emits `ProcessSpawned`).
        // SAFETY: building the image is itself safe; the returned `UserEntry`
        // is only entered later, once the child is dispatched and its
        // `pre_resume` hook has made `space` active (the `spawn_image`
        // contract). The frame source draws identity-mapped RAM frames from
        // the kernel's live allocator. When the child space is not active the
        // build reaches the child's tables + frames through the caller's
        // identity window (the pool and frames are in `[0, 4 GiB)`), so the
        // child's space need not be active to be built (the cross-port
        // producer discipline). A returning `Err` reclaims nothing
        // user-visible (the page-table + image frames are handed out
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
        // SP2): the core runs it on the dispatcher's context immediately
        // before every switch into the child, so it `sret`s into U-mode under
        // its own `satp` root and stays hardware-isolated from the spawning
        // parent and every sibling. It captures only the
        // `u64` root, so it is `Send`. The kernel-stack top it is handed is
        // unused on riscv64: `sscratch` is per-task hardware state armed by
        // `userentry::enter_user` and re-armed by the trap vector from each
        // task's own kernel-stack frame on every U-return.
        let pre_resume: Box<dyn FnMut(u64) + Send> = Box::new(move |_stack_top: u64| {
            // SAFETY: paging is enabled and `child_root_phys` is the Sv39 root
            // of the child's space, which identity-maps the low kernel window
            // the running kernel executes from — exactly `activate_user_root`'s
            // contract.
            unsafe { activate_user_root(child_root_phys) };
        });

        // The user-mode transition, boxed for the scheduler task body the core
        // wraps the child in. `enter_user` diverges into U-mode, so the
        // closure never truly returns (its `!` coerces to `()`).
        let user_mode = UserMode::new();
        let enter: Box<dyn FnMut() + Send> = Box::new(move || {
            // SAFETY: by the time this body runs the child has been dispatched,
            // so its `pre_resume` hook has activated `space` and the S-mode
            // trap vector + production dispatch callback are installed; the
            // child's first `ecall` is handled. `build_process_image` mapped
            // the entry/stack as user pages.
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
        // through a fresh identity [`DirectPhysMap`] over the same identity
        // window the child runs under. A build context with no `'static`
        // allocator, or a window the allocator rejects, retains no live space
        // and the child's `mem_map` / `mmio_map` fail closed.
        let live: Option<Box<dyn LiveUserSpace + Send>> = match ctx.page_table_allocator() {
            Some(static_frames) => LiveSpace::new(
                space,
                DirectPhysMap::identity((IDENTITY_GIB as u64) << 30),
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
        // exclusive to this child that stays mapped for the task's lifetime,
        // and `live` retains the same arch space `frozen` was taken from —
        // the `admit_process` contract.
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

/// Map a [`SpawnCallerError`] onto a stable [`Errno`] for the `spawn`
/// syscall's caller. The precise cause is already on the
/// audit log via the `ProcessSpawn*` events `spawn_image` emits.
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
        // `AdmitError` is `#[non_exhaustive]`: any future variant fails
        // closed to a stable resource errno.
        _ => Errno::NoSpace,
    }
}
