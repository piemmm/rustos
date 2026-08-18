//! riscv64 (QEMU `virt` / SiFive) runtime `spawn` producer —
//! `plans/PI.md` RV-P3 / `plans/SPAWN.md` `SP3b`.
//!
//! [`RiscvProcessSpawn`] implements the architecture-neutral
//! [`tairix_kernel_core::ArchImageBuilder`] seam the boot pipeline installs
//! into the [`tairix_kernel_core::BootInfo`] hand-off
//! (`boot_riscv64::try_boot` → `BootInfo::with_spawn`). When a task that
//! holds `CAP_PROC_SPAWN` issues the `spawn` syscall, the kernel resolves
//! the requested path against the shared
//! [`crate::spawn_layout::PROGRAM_REGISTRY`], admits a parked **loading**
//! child, and returns its PID at once. On the child's own first scheduled
//! slice this producer's
//! [`build`](tairix_kernel_core::ArchImageBuilder::build) builds it a *fresh,
//! hardware-isolated* Sv39 address space and populates it through the
//! production capability-checked, audited spawn caller ([`spawn_image`],
//! gated on `CAP_PROC_SPAWN`). Unlike the PID-1
//! [`tairix_kernel_core::InitSpawn`] seam (`init_spawn_riscv64.rs`) it does
//! **not** switch the active translation regime (`satp`) or enter user mode
//! from the caller: the spawning caller keeps running under its own root, and
//! the child enters user mode from its own loading body when the scheduler
//! next steps it (a true concurrent, non-blocking spawn, not an `exec`-style
//! hand-off) — the riscv64 sibling of the `Aarch64ProcessSpawn` / x86_64
//! producers.
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
//! a synchronous store page fault under the child's `satp` — the cross-port
//! mirror of the `aarch64`/`x86_64` producers. Where no arena region is
//! available, or the split/unmap fails, it falls back to a heap-backed
//! software-canary [`BoxStack`] (fail closed).

use alloc::boxed::Box;
use alloc::sync::Arc;

use tairix_abi::rxe::LoadImage;
use tairix_abi::Errno;
use tairix_arch_api::mmu::AddressSpace as MmuAddressSpace;
use tairix_arch_riscv64::paging::{activate_user_root, AddressSpace as ArchAddressSpace};
use tairix_arch_riscv64::userentry::USER_MODE;
use tairix_kernel_core::{
    refuse_build, spawn_caller_errno, spawn_image, ArchImageBuilder, BoxStack, BuiltImage,
    ImageBuildCtx, KernelStack, ProcessResume, ProcessSpace, SpawnMode, SpawnRequest,
    UserThreadEntry,
};
use tairix_kernel_mem::{
    AddressSpace, DirectPhysMap, FrameAllocator, FrameTableSource, LiveSpace, PhysMap,
    UserAddressSpace, UserStack, VirtAddr,
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

/// A spawned child's four fixed guarded-window bases (`plans/PI.md`
/// 5d-0-ii (b′)/(c)), derived from the one shared offset set the retained
/// [`LiveSpace`]'s window allocators are configured with.
const WINDOWS: spawn_layout::WindowBases = spawn_layout::window_bases(CHILD_USER_BIAS);

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
        //
        // The image and stack are *user* pages, so they draw through the
        // reserve-gated user path: a spawn cannot dip into the kernel
        // reserve, nor steal a frame a prior `mem_map`/stack reservation is
        // guaranteeing, so it fails closed under genuine memory pressure
        // rather than overcommitting. (The child's page-table frames are
        // kernel structures drawn separately from the reserve above.)
        let frames = ctx.frames();
        let entry = unsafe {
            spawn_image(
                &spawn_layout::SpawnAuthority,
                SpawnMode::General,
                ctx.audit(),
                &mut space,
                &physmap,
                &request,
                move || frames.alloc_user().ok(),
            )
        }
        .map_err(spawn_caller_errno)?;

        // The child's user-address-space reactivation hook (`plans/SPAWN.md`
        // SP2): the core runs it on the dispatcher's context immediately
        // before every switch into the child, so it `sret`s into U-mode under
        // its own `satp` root. It captures only the `u64` root, so it is
        // `Send`. The kernel-stack top it is handed is unused on riscv64
        // (`sscratch` is per-task hardware state armed by `enter_user`).
        let pre_resume: ProcessResume = Arc::new(move |_stack_top: u64, _tls_base: u64| {
            // SAFETY: paging is enabled and `child_root_phys` is the Sv39 root
            // of the child's space, which identity-maps the low kernel window
            // the running kernel executes from — exactly `activate_user_root`'s
            // contract.
            unsafe { activate_user_root(child_root_phys) };
        });

        // Freeze the just-built mappings into the registry-storable,
        // `Send + Sync` snapshot, then hand the child's threads the same arch
        // space as their process address space, so its `mem_map` / `mmio_map`
        // mutate exactly the mappings the snapshot describes. No `'static`
        // allocator, or a window the allocator rejects, retains none and
        // those syscalls fail closed.
        let frozen: Box<dyn UserAddressSpace + Send + Sync> = Box::new(space.freeze());
        let live: Option<Arc<ProcessSpace>> = match ctx.page_table_allocator() {
            Some(static_frames) => {
                let windows = crate::user_windows::user_windows(
                    static_frames.total_frames() as u64,
                    WINDOWS.anon,
                    super::USER_VA_TOP,
                );
                LiveSpace::new(
                    space,
                    DirectPhysMap::identity((IDENTITY_GIB as u64) << 30),
                    static_frames,
                    VirtAddr::new(WINDOWS.mmio),
                    spawn_layout::MMIO_WINDOW_PAGES,
                    VirtAddr::new(WINDOWS.anon),
                    windows.anon_pages,
                    VirtAddr::new(WINDOWS.dma),
                    spawn_layout::DMA_WINDOW_PAGES,
                    VirtAddr::new(WINDOWS.shared),
                    spawn_layout::SHARED_WINDOW_PAGES,
                    VirtAddr::new(windows.file_base),
                    windows.file_pages,
                )
                .ok()
                .map(|live| {
                    Arc::new(ProcessSpace::new(
                        Box::new(live),
                        Arc::clone(&pre_resume),
                        &USER_MODE,
                    ))
                })
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
            entry: UserThreadEntry {
                port: &USER_MODE,
                regs: entry,
            },
        })
    }
}
