//! riscv64 (QEMU `virt` / SiFive) runtime `spawn` producer —
//! `plans/PI.md` RV-P3 / `plans/SPAWN.md` `SP3b`.
//!
//! [`RiscvProcessSpawn`] implements the architecture-neutral
//! [`tairix_kernel_core::ProcessSpawn`] seam the boot pipeline installs into
//! the [`tairix_kernel_core::BootInfo`] hand-off
//! (`boot_riscv64::try_boot` → `BootInfo::with_spawn`). When a task that
//! holds `CAP_PROC_SPAWN` issues the `spawn` syscall, the kernel resolves
//! the requested path against the shared
//! [`crate::spawn_layout::PROGRAM_REGISTRY`] and hands the
//! matching `rxe` bytes to [`ProcessSpawn::spawn`], which builds the child a
//! *fresh, hardware-isolated* Sv39 address space, populates it through the
//! production capability-checked, audited spawn caller ([`spawn_image`],
//! gated on `CAP_PROC_SPAWN`), and admits it **Ready** through
//! [`SpawnCtx::admit_process`]. Unlike the PID-1 [`tairix_kernel_core::InitSpawn`]
//! seam (`init_spawn_riscv64.rs`) it does **not** switch the active
//! translation regime (`satp`) or enter user mode: the spawning caller keeps
//! running under its own root, and the child runs when the scheduler next
//! steps it (a true concurrent spawn, not an `exec`-style hand-off) — the riscv64 sibling of the `Aarch64ProcessSpawn` /
//! x86_64 producers.
//!
//! The child's `rxe`, its relocation bias ([`CHILD_USER_BIAS`]), and the
//! kernel's syscall CFI tag are baked at build time (`build.rs` →
//! `tairix_itest_harness::elf2rxe`), exactly like PID 1, so there is one
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

use tairix_abi::rxe::LoadImage;
use tairix_abi::Errno;
use tairix_arch_api::mmu::AddressSpace as MmuAddressSpace;
use tairix_arch_api::EnterUser;
use tairix_arch_riscv64::paging::{activate_user_root, AddressSpace as ArchAddressSpace};
use tairix_arch_riscv64::userentry::UserMode;
use tairix_kernel_core::{
    refuse_build, spawn_caller_errno, spawn_image, ArchImageBuilder, BoxStack, BuiltImage,
    ImageBuildCtx, KernelStack, SpawnRequest,
};
use tairix_kernel_mem::{
    AddressSpace, DirectPhysMap, FrameAllocator, FrameTableSource, LiveSpace, LiveUserSpace,
    PhysMap, UserAddressSpace, UserStack, VirtAddr,
};
use tairix_kernel_syscall::SYSCALL_TABLE_HASH;
use tairix_sync::Once;

use crate::spawn_layout;
// Re-exported so out-of-crate consumers of this producer (the QEMU
// file-mapping vertical) can pin their fixture `rxe`'s relocation bias to
// the exact bias this producer maps every child at — the aarch64 port's
// `USER_IMAGE_BIAS` export, mirrored (one definition, no copied constant).
pub use crate::spawn_layout::CHILD_USER_BIAS;
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
/// [`CHILD_USER_BIAS`] (64 GiB) sits far above it, so the program's pages
/// land on freshly walked Sv39 tables rather than colliding with an identity
/// gigapage leaf — the same window PID 1 uses.
const IDENTITY_GIB: usize = 4;

/// Base of a spawned child's device-window virtual region
/// (`plans/PI.md` 5d-0-ii (b′)): the retained [`LiveSpace`]'s
/// [`tairix_kernel_mem::MmioWindowMap`] hands each `mmio_map` a
/// guard-bracketed window out of `[MMIO_WINDOW_BASE, MMIO_WINDOW_BASE +
/// MMIO_WINDOW_PAGES·4 KiB)`.
const MMIO_WINDOW_BASE: u64 = CHILD_USER_BIAS + spawn_layout::MMIO_WINDOW_OFFSET;
/// Base of a spawned child's non-`FIXED` anonymous-heap virtual region
/// (`plans/PI.md` 5d-0-ii (c)): the retained [`LiveSpace`]'s
/// [`tairix_kernel_mem::AnonWindowMap`] places each non-`FIXED` `mem_map`
/// out of `[ANON_WINDOW_BASE, ANON_WINDOW_BASE + anon_window_pages·4 KiB)`,
/// where the page count scales with discovered RAM (the window is the
/// topmost user region so it has room to grow up to `super::USER_VA_TOP`).
const ANON_WINDOW_BASE: u64 = CHILD_USER_BIAS + spawn_layout::ANON_WINDOW_OFFSET;
/// Base of a spawned child's guarded DMA-buffer virtual region
/// (`plans/PI.md` 5d-0-ii (c) DMA half): the retained [`LiveSpace`]'s
/// [`tairix_kernel_mem::DmaWindowMap`] carves each `dma_alloc` buffer out of
/// `[DMA_WINDOW_BASE, DMA_WINDOW_BASE + DMA_WINDOW_PAGES·4 KiB)`.
const DMA_WINDOW_BASE: u64 = CHILD_USER_BIAS + spawn_layout::DMA_WINDOW_OFFSET;
/// Base of a spawned child's cross-process shared-memory virtual region:
/// the retained [`LiveSpace`]'s shared-window allocator maps each granted
/// `shm_map` region out of `[SHARED_WINDOW_BASE, SHARED_WINDOW_BASE +
/// SHARED_WINDOW_PAGES * 4 KiB)`.
const SHARED_WINDOW_BASE: u64 = CHILD_USER_BIAS + spawn_layout::SHARED_WINDOW_OFFSET;

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
/// build fails closed with [`Errno::NoSpace`] (deterministic OOM, never a panic). The frames live exactly as long as the child: its
/// retained live space returns every table frame through
/// [`FrameTableSource::free_table`] when the task exits and the space is
/// dropped at reap (`plans/APPS.md` I2), so spawn/exit cycles hold the
/// allocator steady.
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
/// [`tairix_kernel_core::BootInfo`] hand-off by `boot_riscv64::try_boot`.
pub struct RiscvProcessSpawn;

/// The single, `'static` [`RiscvProcessSpawn`] the boot path borrows.
pub static RISCV_PROCESS_SPAWN: RiscvProcessSpawn = RiscvProcessSpawn;

impl ArchImageBuilder for RiscvProcessSpawn {
    fn alloc_kernel_stack(
        &self,
        frames: &FrameAllocator,
        pt_frames: Option<&'static FrameAllocator>,
    ) -> (Box<dyn KernelStack + Send>, Option<u64>) {
        // Allocate the loading child's kernel stack synchronously at admit,
        // before its address space exists, so the child's own loading body
        // runs on it. An arena-backed guard-paged stack when a region is
        // available (its guard VA returned for [`build`] to unmap in the
        // child root), else the software-canary [`BoxStack`] fallback.
        let Some(pt_frames) = pt_frames else {
            return (Box::new(BoxStack::new()), None);
        };
        crate::stack_arena::publish_reclaim_frames(pt_frames);
        let grow = FrameArenaGrow::new(frames, (IDENTITY_GIB as u64) << 30);
        match KTHREAD_STACK_ARENA.alloc(&grow, &crate::stack_arena::IdentityBlockStore) {
            Some(stack) => {
                let guard = stack.guard_page();
                (Box::new(stack), Some(guard))
            }
            None => (Box::new(BoxStack::new()), None),
        }
    }

    fn build(
        &self,
        rxe: &[u8],
        ctx: &dyn ImageBuildCtx,
        args: &[&[u8]],
        env: &[&[u8]],
    ) -> Result<BuiltImage, Errno> {
        // The child's Sv39 hierarchy is drawn from the kernel's live frame
        // allocator: there is no fixed page-table reserve and so no hard cap
        // on how many processes can be spawned — the capacity scales with
        // discovered RAM and grows on demand. A build with no `'static`
        // allocator wired fails closed, as does genuine RAM exhaustion below.
        let pt_frames = ctx
            .page_table_allocator()
            .ok_or_else(|| refuse_build(ctx, "page_table_allocator_unwired"))?;
        let table_frames = page_table_source(pt_frames)?;

        // Build an Sv39 address space identity-mapping the kernel + MMIO, and
        // capture its root *without* switching to it: the loading child runs
        // on its own kernel stack under the kernel's identity regime, so the
        // running task is never moved out from under itself. The child's own
        // root is reactivated by its `pre_resume` hook before the scheduler
        // first resumes it as a user task (`plans/SPAWN.md` SP2). An
        // allocator exhausted of even the root table fails closed.
        let mut arch = ArchAddressSpace::new_identity_gigapages(table_frames, IDENTITY_GIB)
            .ok_or_else(|| refuse_build(ctx, "page_table_frames_exhausted"))?;
        let child_root_phys = arch.root_phys();

        // Re-express the loading kthread's kernel-stack guard page in the
        // *child's own* (inactive) Sv39 root: split the coarse identity block
        // covering it to 4 KiB granularity and unmap the single guard page,
        // so an overrun of the child's kernel stack takes a synchronous store
        // page fault under the child's `satp` rather than corrupting the
        // lower-addressed neighbour. Doing it on `arch` — never switched to
        // here — disturbs no live access and needs no TLB maintenance. A
        // `Some(guard)` whose split+unmap fails fails the build closed rather
        // than running on an unguarded stack; `None` is the software-canary
        // `BoxStack` (self-guarded, nothing to unmap).
        if let Some(guard) = ctx.kernel_stack_guard() {
            arch.split_block(guard)
                .and_then(|()| arch.unmap(guard).map(|_| ()))
                .map_err(|_| refuse_build(ctx, "kernel_stack_guard_unmap_failed"))?;
        }

        let mut space = AddressSpace::new(arch);
        let physmap = DirectPhysMap::identity((IDENTITY_GIB as u64) << 30);

        // Parse the build-time `rxe` blob against the kernel's own compiled-in
        // syscall CFI tag. A mismatch fails closed.
        let image = LoadImage::parse(rxe, &SYSCALL_TABLE_HASH).map_err(|_| Errno::BadMagic)?;

        // Place the stack and startup block above the image's mapped top
        // through the shared per-spawn derivation (one definition across
        // the ports); an image too large for the user region fails closed.
        let layout = spawn_layout::user_layout(&image, CHILD_USER_BIAS)
            .ok_or_else(|| refuse_build(ctx, "user_layout_unfit"))?;
        let stack_span = spawn_layout::stack_span(&layout)
            .ok_or_else(|| refuse_build(ctx, "stack_span_malformed"))?;

        let request = SpawnRequest {
            image: &image,
            image_bytes: rxe,
            bias: CHILD_USER_BIAS,
            stack: UserStack {
                base: layout.stack_base,
                page_count: spawn_layout::USER_STACK_COMMIT_PAGES,
            },
            start_block_base: layout.block_base,
            args,
            env,
            canary: spawn_layout::CHILD_CANARY,
        };

        // Authorise + build the child's U-mode image (emits `ProcessSpawned`).
        // SAFETY: building the image is itself safe; the returned `UserEntry`
        // is only entered later, once the child is dispatched and its
        // `pre_resume` hook has made `space` active (the `spawn_image`
        // contract). The frame source draws identity-mapped RAM frames from
        // the kernel's live allocator; the retained live space below owns the
        // whole footprint and returns it when the task exits. A returning
        // `Err` maps to a stable errno; the cause is already audited by
        // `spawn_image`.
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
        // its own `satp` root. It captures only the `u64` root, so it is
        // `Send`. The kernel-stack top it is handed is unused on riscv64
        // (`sscratch` is per-task hardware state armed by `enter_user`).
        let pre_resume: Box<dyn FnMut(u64) + Send> = Box::new(move |_stack_top: u64| {
            // SAFETY: paging is enabled and `child_root_phys` is the Sv39 root
            // of the child's space, which identity-maps the low kernel window
            // the running kernel executes from — exactly `activate_user_root`'s
            // contract.
            unsafe { activate_user_root(child_root_phys) };
        });

        // The user-mode transition, boxed for the loading body to invoke once
        // `become_user` has installed the hook. `enter_user` diverges into
        // U-mode, so its `!` coerces to `()`.
        let user_mode = UserMode::new();
        let enter: Box<dyn FnMut() + Send> = Box::new(move || {
            // SAFETY: by the time this body runs the child has been dispatched,
            // so its `pre_resume` hook has activated `space` and the S-mode
            // trap vector + production dispatch callback are installed; the
            // child's first `ecall` is handled.
            unsafe { user_mode.enter_user(entry) }
        });

        // Freeze the just-built mappings into the registry-storable,
        // `Send + Sync` snapshot, then retain the live, mutable arch space
        // (the same one frozen above) behind `LiveUserSpace` so the child's
        // `mem_map` / `mmio_map` mutate its own space. A build context with
        // no `'static` allocator, or a window the allocator rejects, retains
        // no live space and those syscalls fail closed.
        let frozen: Box<dyn UserAddressSpace + Send + Sync> = Box::new(space.freeze());
        let live: Option<Box<dyn LiveUserSpace + Send>> = match ctx.page_table_allocator() {
            Some(static_frames) => {
                let windows = crate::user_windows::user_windows(
                    static_frames.total_frames() as u64,
                    ANON_WINDOW_BASE,
                    super::USER_VA_TOP,
                );
                LiveSpace::new(
                    space,
                    DirectPhysMap::identity((IDENTITY_GIB as u64) << 30),
                    static_frames,
                    VirtAddr::new(MMIO_WINDOW_BASE),
                    spawn_layout::MMIO_WINDOW_PAGES,
                    VirtAddr::new(ANON_WINDOW_BASE),
                    windows.anon_pages,
                    VirtAddr::new(DMA_WINDOW_BASE),
                    spawn_layout::DMA_WINDOW_PAGES,
                    VirtAddr::new(SHARED_WINDOW_BASE),
                    spawn_layout::SHARED_WINDOW_PAGES,
                    VirtAddr::new(windows.file_base),
                    windows.file_pages,
                )
                .ok()
                .map(|live| Box::new(live) as Box<dyn LiveUserSpace + Send>)
            }
            None => None,
        };

        let physmap: Box<dyn PhysMap + Send + Sync> = Box::new(physmap);

        Ok(BuiltImage {
            frozen,
            physmap,
            stack_span,
            live,
            pre_resume,
            enter,
        })
    }
}
