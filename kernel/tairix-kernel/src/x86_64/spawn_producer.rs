//! x86_64 runtime `spawn` producer — `plans/PI.md` `X3b`.
//!
//! [`X86_64ProcessSpawn`] implements the architecture-neutral
//! [`tairix_kernel_core::ArchImageBuilder`] seam the boot pipeline installs
//! into the [`tairix_kernel_core::BootInfo`] hand-off (`boot::try_boot` →
//! `BootInfo::with_spawn`). It is the cross-port sibling of the aarch64
//! `spawn_producer` (`plans/SPAWN.md` `SP3b`): when a task that holds
//! `CAP_PROC_SPAWN` issues the `spawn` syscall, the kernel resolves the
//! requested path against the shared
//! [`crate::spawn_layout::PROGRAM_REGISTRY`], admits a parked **loading**
//! child, and returns its PID at once. On the child's own first scheduled
//! slice this producer's
//! [`build`](tairix_kernel_core::ArchImageBuilder::build) builds it a *fresh,
//! hardware-isolated* PML4 hierarchy and populates it through the production
//! capability-checked, audited spawn caller ([`spawn_image`], gated on
//! `CAP_PROC_SPAWN`). Unlike the PID-1 [`tairix_kernel_core::InitSpawn`] seam
//! (`init_spawn_x86_64.rs`) it does **not** switch CR3 or enter ring 3 from
//! the caller: the spawning caller keeps running under its own root, and the
//! child enters ring 3 from its own loading body when the scheduler next steps
//! it (a true concurrent, non-blocking spawn, not an `exec`-style hand-off).
//!
//! # Building the child without switching CR3
//!
//! The runtime `spawn` syscall is issued by PID 1 `init`, so the producer runs
//! under PID 1's own root, which [`init_spawn`](crate::x86_64::init_spawn)
//! built with [`ArchAddressSpace::new_identity_first_gib`]: it identity-maps the
//! discovered-RAM window ([`paging::configured_identity_gigapages`]) **and**
//! mirrors the higher-half kernel window. The
//! x86_64 page-table walk recovers each existing intermediate table from its
//! **low physical address** (`paging::ensure_child`) and writes each new table
//! through its higher-half static pointer, and the image content is written
//! through the same identity [`ConfiguredIdentityPhysMap`] — all three are
//! mapped under PID 1's active root, since every table and image frame comes
//! from the live allocator inside that window. So the producer builds the child's
//! tables *through the caller's active CR3*, never switching it, exactly as the
//! aarch64 producer builds through its identity window. The child's own CR3 is
//! reloaded by its `pre_resume` hook before the scheduler first resumes it
//! (`plans/SPAWN.md` SP2, `plans/PI.md` X1).
//!
//! Spawning is *not* a privileged bypass: the child receives only the authority
//! its registered program declares intersected with its user's grants; this seam only authorises the *act* of spawning
//! under `CAP_PROC_SPAWN`.

use core::ptr::NonNull;

use alloc::boxed::Box;
use alloc::sync::Arc;

use tairix_abi::rxe::LoadImage;
use tairix_abi::Errno;
use tairix_arch_api::mmu::AddressSpace as MmuAddressSpace;
use tairix_arch_x86_64::paging::{self, activate_user_root, AddressSpace as ArchAddressSpace};
use tairix_arch_x86_64::syscall_entry;
use tairix_arch_x86_64::userentry::{set_user_thread_pointer, USER_MODE};
use tairix_kernel_core::{
    refuse_build, spawn_caller_errno, spawn_image, ArchImageBuilder, BoxStack, BuiltImage,
    ImageBuildCtx, KernelStack, ProcessResume, ProcessSpace, SpawnMode, SpawnRequest,
    UserThreadEntry,
};
use tairix_kernel_mem::{
    AddressSpace, DirectPhysMap, FrameAllocator, FrameTableSource, LiveSpace, PhysAddr, PhysMap,
    UserAddressSpace, UserStack, VirtAddr,
};
use tairix_kernel_syscall::SYSCALL_TABLE_HASH;
use tairix_sync::Once;

use crate::spawn_layout::{self, CHILD_USER_BIAS};
use crate::stack_arena::{FrameArenaGrow, KTHREAD_STACK_ARENA};

/// Logical CPU the boot processor runs as — the single core the (c7-bin)
/// bring-up initialises and the one [`syscall_entry::set_kernel_rsp0`]
/// repoints (mirrors `init_spawn_x86_64::BOOT_CPU`).
const BOOT_CPU: usize = 0;

/// A spawned child's four fixed guarded-window bases (`plans/PI.md`
/// 5d-0-ii (b′)/(c)), derived from the one shared offset set the retained
/// [`LiveSpace`]'s window allocators are configured with.
const WINDOWS: spawn_layout::WindowBases = spawn_layout::window_bases(CHILD_USER_BIAS);

/// The kernel's direct physical map: the low identity window
/// (`virtual == physical`) the boot path sized from the discovered memory
/// map, mirroring the aarch64 and riscv64 ports.
///
/// It has to be the **identity** map because the x86_64 page-table walk
/// recovers an existing child table by dereferencing its physical address
/// directly (`paging::ensure_child`), so the frame view the page-table
/// source hands the port must satisfy `virtual == physical`. It is also the
/// view every other kernel path that reaches a frame by pointer uses — the
/// child image write, the shared-region zero-on-free scrub, the remap
/// window's record store, the slab page supply — so there is one map, not a
/// separate higher-half one that could cover different RAM.
///
/// The limit is re-derived from the live window on every call
/// ([`paging::configured_identity_bytes`]) rather than frozen at a
/// build-time gigabyte count a real machine outgrows: the fixed 1 GiB
/// higher-half map this replaced left every frame above it unreachable
/// while the allocator kept handing them out. A frame outside the window
/// still fails the translate and its consumer fails closed rather than
/// fabricating a pointer.
pub struct ConfiguredIdentityPhysMap;

impl PhysMap for ConfiguredIdentityPhysMap {
    fn translate(&self, phys: PhysAddr, len: usize) -> Option<NonNull<u8>> {
        DirectPhysMap::identity(paging::configured_identity_bytes()).translate(phys, len)
    }

    fn reverse(&self, virt: usize) -> Option<PhysAddr> {
        DirectPhysMap::identity(paging::configured_identity_bytes()).reverse(virt)
    }

    fn clean_invalidate(&self, _phys: PhysAddr, _len: usize) {
        // Deliberate no-op: x86_64 DMA is I/O-coherent, so a device sees the
        // kernel's cacheable writes without maintenance.
    }

    fn sync_instruction_cache(&self, _phys: PhysAddr, _len: usize) {
        // Deliberate no-op: the x86_64 instruction cache is coherent with
        // kernel data writes, so freshly loaded code needs no maintenance.
    }
}

/// The single, `'static` [`ConfiguredIdentityPhysMap`] the page-table frame
/// source borrows.
///
/// Also handed to the kernel core as the arch direct physical map
/// (`plans/USB.md`): it covers the same RAM the allocator draws from, so
/// any frame the kernel must reach by pointer is reachable.
pub static SPAWN_TABLE_PHYSMAP: ConfiguredIdentityPhysMap = ConfiguredIdentityPhysMap;

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
/// (deterministic OOM, never a panic). The frames live exactly as long as
/// the child: its retained live space returns every table frame through
/// [`FrameTableSource::free_table`] when the task exits and the space is
/// dropped at reap (`plans/APPS.md` I2), so spawn/exit cycles hold the
/// allocator steady. Mirrors the aarch64 producer.
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
pub(crate) fn page_table_source(
    frames: &'static FrameAllocator,
) -> Result<&'static FrameTableSource, Errno> {
    SPAWN_FRAME_SOURCE
        .call_once_infallible(|| FrameTableSource::new(frames, &SPAWN_TABLE_PHYSMAP))
        .map_err(|_| Errno::NotImplemented)
}

/// The x86_64 runtime `spawn` producer installed into the
/// [`tairix_kernel_core::BootInfo`] hand-off by `boot::try_boot`.
pub struct X86_64ProcessSpawn;

/// The single, `'static` [`X86_64ProcessSpawn`] the boot path borrows.
pub static X86_64_PROCESS_SPAWN: X86_64ProcessSpawn = X86_64ProcessSpawn;

impl ArchImageBuilder for X86_64ProcessSpawn {
    fn alloc_kernel_stack(
        &self,
        frames: &FrameAllocator,
        pt_frames: Option<&'static FrameAllocator>,
    ) -> (Box<dyn KernelStack + Send>, Option<u64>) {
        // Allocate the loading child's kernel stack synchronously at admit,
        // before its address space exists, so the child's own loading body
        // runs on it. An arena-backed guard-paged stack when a region is
        // available (its guard VA returned for `build` to unmap in the child
        // root), else the software-canary `BoxStack` fallback.
        let Some(pt_frames) = pt_frames else {
            return (Box::new(BoxStack::new()), None);
        };
        crate::stack_arena::publish_reclaim_frames(pt_frames);
        let grow = FrameArenaGrow::new(frames, paging::configured_identity_bytes());
        match KTHREAD_STACK_ARENA.alloc(&grow, &crate::stack_arena::IdentityArenaMemory) {
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
        // The child's PML4 hierarchy is drawn from the kernel's live frame
        // allocator: there is no fixed page-table reserve
        // and so no hard cap on how many processes can be spawned — the
        // capacity scales with discovered RAM and grows on demand. A build
        // with no `'static` allocator wired fails closed,
        // as does genuine RAM exhaustion below.
        let pt_frames = ctx
            .page_table_allocator()
            .ok_or_else(|| refuse_build(ctx, "page_table_allocator_unwired"))?;
        let table_frames = page_table_source(pt_frames)?;

        // Build a PML4 identity-mapping the discovered-RAM window (RAM + the
        // LAPIC MMIO page) and the higher-half kernel window, and capture its
        // root *without* switching CR3: the spawning caller (PID 1) stays
        // active under its own root, so the running parent is never moved out
        // from under itself. The child's tables and image are written through
        // the caller's active root — which identity-maps the live allocator's
        // page-table and image frames and mirrors the higher-half kernel
        // window — so the build does not require the child space to be
        // active.
        // The child's own CR3 is reloaded by its `pre_resume` hook before the
        // scheduler first resumes it (`plans/SPAWN.md` SP2, `plans/PI.md` X1).
        let mut arch = ArchAddressSpace::new_identity_first_gib(
            table_frames,
            paging::configured_identity_gigapages(),
        )
        .ok_or_else(|| refuse_build(ctx, "page_table_frames_exhausted"))?;
        let child_root_phys = arch.pml4_phys();

        // Re-express the loading kthread's kernel-stack guard page in the
        // *child's own* (inactive) PML4: split the coarse identity block
        // covering it to 4 KiB granularity and unmap the single guard page,
        // so an overrun of the child's kernel stack takes a synchronous page
        // fault under the child's CR3 rather than corrupting the
        // lower-addressed neighbour. Doing it on `arch` — never switched to
        // here — disturbs no live access (the child tables live in the
        // caller's low identity window) and needs no TLB maintenance (the
        // child's root is not active). A `Some(guard)` whose split+unmap
        // fails fails the build closed rather than running on an unguarded
        // stack; `None` is the software-canary `BoxStack` (self-guarded,
        // nothing to unmap).
        if let Some(guard) = ctx.kernel_stack_guard() {
            arch.split_block(guard)
                .and_then(|()| arch.unmap(guard).map(|_| ()))
                .map_err(|_| refuse_build(ctx, "kernel_stack_guard_unmap_failed"))?;
        }

        let mut space = AddressSpace::new(arch);
        let physmap = ConfiguredIdentityPhysMap;

        // Parse the build-time `rxe` blob against the kernel's own compiled-in
        // syscall CFI tag. A mismatch fails closed; the registry
        // holds bytes that already parsed once at build time, so reaching this
        // is a kernel build defect, surfaced as a stable errno.
        let image = LoadImage::parse(rxe, &SYSCALL_TABLE_HASH).map_err(|_| Errno::BadMagic)?;

        // Place the stack and startup block above the image's mapped top
        // through the shared per-spawn derivation (one definition across
        // the ports); an image too large for the user region fails closed.
        let layout = spawn_layout::user_layout(&image, CHILD_USER_BIAS)
            .ok_or_else(|| refuse_build(ctx, "user_layout_unfit"))?;
        // The span record the admission path stores so the stack-growth
        // fault path can back pages inside it (one shared derivation
        // across the ports; a malformed span refuses the spawn closed).
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

        // Authorise + build the child's ring-3 image (emits `ProcessSpawned`).
        // SAFETY: building the image is itself safe; the returned `UserEntry`
        // is only entered later, once the child is dispatched and its
        // `pre_resume` hook has made `space` active (the `spawn_image`
        // contract). The frame source draws RAM frames from the kernel's live
        // allocator, written through the identity `physmap` mapped under the
        // caller's active root. The retained live space
        // below owns the whole footprint and returns it (frames zeroed,
        // tables freed) when the task exits. A returning `Err` maps to a
        // stable errno; the cause is already audited by `spawn_image`.
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

        // The child's switch-in hook (`plans/SPAWN.md` SP2, `plans/PI.md` X1):
        // the core runs it on the dispatcher's context immediately before every
        // switch into any thread of the child. It reloads CR3 to the child's
        // own root (isolation), repoints the per-CPU `syscall` entry stack at
        // the switching-in thread's own kernel stack, and reinstalls that
        // thread's thread pointer. It captures only the `u64` root, so it is
        // `Send`.
        let pre_resume: ProcessResume = Arc::new(move |stack_top: u64, tls_base: u64| {
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
            // The `FS` base is privileged on this port, so the kernel — not
            // the thread — maintains it: every switch-in reinstalls the
            // switching-in thread's own value (`plans/THREADS.md` decision 7).
            // SAFETY: this runs at CPL 0 on the dispatcher's context, on the
            // CPU about to enter that thread, which is exactly
            // `set_user_thread_pointer`'s contract.
            unsafe { set_user_thread_pointer(tls_base) };
            // SAFETY: paging is enabled and `child_root_phys` is the PML4 of
            // the child's space, which identity-maps the low kernel window the
            // running dispatcher executes from and mirrors the higher-half
            // kernel window — exactly `activate_user_root`'s contract.
            unsafe { activate_user_root(child_root_phys) };
        });

        // Freeze the just-built mappings into the registry-storable,
        // `Send + Sync` snapshot the kernel-wide address-space registry holds
        // (the live arch `space` is not `Sync`), and box the direct map that
        // backs it, so the child's `stream_write` can copy its banner out of
        // its own user memory. Freezing *after* `spawn_image` captures every
        // mapped page — segments, stack, and the startup-vector block.
        let frozen: Box<dyn UserAddressSpace + Send + Sync> = Box::new(space.freeze());

        // The child's process address space (`plans/PI.md` 5d-0-ii (b′)): the
        // *same* arch space the snapshot above was frozen from, zeroing
        // anonymous frames through the same identity direct map the image
        // build used (the child's CR3 carries it). No
        // `'static` allocator, or a window the allocator rejects, retains none
        // and the child's `mem_map` / `mmio_map` fail closed.
        let live: Option<Arc<ProcessSpace>> = match ctx.page_table_allocator() {
            Some(static_frames) => {
                let windows = crate::user_windows::user_windows(
                    static_frames.total_frames() as u64,
                    WINDOWS.anon,
                    super::USER_VA_TOP,
                );
                LiveSpace::new(
                    space,
                    ConfiguredIdentityPhysMap,
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
