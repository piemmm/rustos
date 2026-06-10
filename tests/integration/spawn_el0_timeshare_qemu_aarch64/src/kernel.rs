//! The freestanding aarch64 test kernel: build two isolated EL0 programs and
//! timeshare them under the live scheduler (`plans/SPAWN.md` `SP2c`).

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use alloc::sync::Arc;

use rustos_abi::rxe::LoadImage;
use rustos_abi::{CapabilityId, CapabilityQuery, SyscallNumber, SYSCALL_MAX_ARGS};
use rustos_arch_aarch64::context_hal::ContextSwitchHal;
use rustos_arch_aarch64::kernel_arch::timer_frequency_hz;
use rustos_arch_aarch64::paging::{
    self, activate_user_root, AddressSpace as ArchAddressSpace, PageTablePool,
};
use rustos_arch_aarch64::userentry::UserMode;
use rustos_arch_aarch64::{
    enable_fp_el1, exceptions, gic, handle_panic_via_serial, qemu_exit, syscall_entry, SERIAL_SINK,
};
use rustos_arch_api::{CpuId, EnterUser};
use rustos_bumpalloc::BumpAllocator;
use rustos_fdt::Fdt;
use rustos_kernel_core::{
    reschedule_current, spawn_image, spawn_user_kthread, RescheduleAction, SpawnRequest, Yielder,
};
use rustos_kernel_mem::{AddressSpace, DirectPhysMap, Frame, PhysAddr, UserStack};
use rustos_kernel_sched_eevdf::{Priority, Scheduler, SchedulerConfig};
use rustos_kernel_syscall::SYSCALL_TABLE_HASH;
use rustos_log::{log, Event, EventId, Level};

// `PROGRAM_RXE: &[u8]`, `USER_BIAS: u64`, and `YIELDS_PER_TASK: u64`, generated
// by `build.rs`.
include!(concat!(env!("OUT_DIR"), "/program_rxe.rs"));
// The canonical QEMU `virt` device tree, dumped and embedded at build time:
// the GICv2 base and the timer frequency are read from it (`plans/PI.md` P3/P4).
include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

/// The single-core slice runs logical CPU 0 on the boot core.
const BOOT_CPU: CpuId = 0;

/// The two EL0 tasks the test timeshares.
const TASK_COUNT: u64 = 2;

/// Gigabytes of identity map each EL0 address space provides: `[0, 2 GiB)`
/// covers the `virt` board's device MMIO (GiB 0) and the RAM base at GiB 1
/// where this kernel runs, so switching between the two spaces never moves the
/// ground under the running kernel (every kernel pointer keeps its identity
/// address). [`USER_BIAS`] (64 GiB) sits far above, on freshly walked tables.
const IDENTITY_GIB: usize = 2;

/// User stack base (1 MiB into the high user region) and size. `rustos-rt`'s
/// `_start` only aligns the stack and calls, so a small stack suffices for the
/// trivial yield-then-exit program; 256 KiB is generous headroom.
const USER_STACK_BASE: u64 = USER_BIAS + 0x10_0000;
/// User stack pages (256 KiB).
const USER_STACK_PAGES: u64 = 64;
/// User virtual address the startup-vector block is written at (3 MiB up, well
/// clear of the program image and the stack).
const USER_BLOCK_BASE: u64 = USER_BIAS + 0x30_0000;

/// Per-process stack-canary seed handed to each program (`AGENTS.md` §19.2).
/// Any value; the kernel-RNG-seeded canary is a later stage.
const CANARY: u64 = 0x5520_C000_D15E_A5ED;

/// Number of physical frames the test hands each spawn build: the program's
/// segments, its user stack, and the startup block, for both programs, with
/// headroom. The page-table frames come from the per-space [`PageTablePool`]s,
/// not from here.
const FRAME_COUNT: usize = 256;

/// Cooperative-loop watchdog: maximum `step` iterations before the test
/// declares the workload deadlocked. Sized generously for QEMU TCG.
const MAX_STEPS: u64 = 5_000_000;

/// Stable audit-event ids for the QEMU transcript.
const TEST_START: EventId = EventId(4270);
const TEST_SPAWNED: EventId = EventId(4271);
const TEST_PASS: EventId = EventId(4272);

/// Failure finisher codes, distinct per failure site.
const FAIL_ZERO_FREQ: u16 = 1;
const FAIL_GIC_NOT_DISCOVERED: u16 = 2;
const FAIL_POOL: u16 = 3;
const FAIL_PARSE: u16 = 4;
const FAIL_BUILD: u16 = 5;
const FAIL_SCHED_NEW: u16 = 6;
const FAIL_SPAWN: u16 = 7;
const FAIL_DEADLOCK: u16 = 8;
const FAIL_YIELD_COUNT: u16 = 9;
const FAIL_EXIT_COUNT: u16 = 10;
const FAIL_UNEXPECTED_SYSCALL: u16 = 11;

/// Total `yield` syscalls observed across both EL0 tasks.
static YIELDS: AtomicU64 = AtomicU64::new(0);
/// Total `exit` syscalls observed across both EL0 tasks.
static EXITS: AtomicU64 = AtomicU64::new(0);

/// Size of the test's bump heap, from which the spawn caller allocates the
/// startup-vector block buffers and the two user kthreads' kernel stacks +
/// control blocks. Lives in `.bss` (zeroed by the boot trampoline).
const HEAP_SIZE: usize = 2 * 1024 * 1024;

/// Page-aligned backing store for the bump heap.
#[repr(C, align(4096))]
struct HeapStore([u8; HEAP_SIZE]);

static mut HEAP: HeapStore = HeapStore([0; HEAP_SIZE]);

/// Global allocator backed by [`HEAP`].
///
/// SAFETY: the page-aligned `HEAP` static outlives the binary and the
/// allocator is its only consumer.
#[global_allocator]
static ALLOCATOR: BumpAllocator =
    unsafe { BumpAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_SIZE) };

/// Per-space page-table pools (one per EL0 address space). Each backs a stage-1
/// hierarchy whose root [`activate_user_root`] reinstalls before every switch
/// into its task, so the two tasks stay hardware-isolated (`AGENTS.md` §4).
static PAGE_TABLES_A: PageTablePool = PageTablePool::new();
static PAGE_TABLES_B: PageTablePool = PageTablePool::new();

/// Physical-frame backing store the spawn builders allocate user pages from.
/// `align(4096)` so each `PAGE_SIZE` slice is a valid page frame; identity-
/// mapped (its physical address equals its kernel virtual address), so the
/// builders reach it through [`DirectPhysMap::identity`]. A single monotonic
/// cursor ([`FRAME_CURSOR`]) hands disjoint frames to both program builds, so
/// the two address spaces never share a data frame (`AGENTS.md` §4).
#[repr(C, align(4096))]
struct FramePool([u8; paging::PAGE_SIZE * FRAME_COUNT]);

static mut FRAME_POOL: FramePool = FramePool([0; paging::PAGE_SIZE * FRAME_COUNT]);

/// Monotonic index of the next free [`FRAME_POOL`] frame, shared by both
/// program builds.
static FRAME_CURSOR: AtomicUsize = AtomicUsize::new(0);

/// Hand out the next identity-mapped physical frame from [`FRAME_POOL`], or
/// `None` when the pool is exhausted (the spawn builder then fails closed).
fn next_frame() -> Option<Frame> {
    let idx = FRAME_CURSOR.fetch_add(1, Ordering::SeqCst);
    if idx >= FRAME_COUNT {
        FRAME_CURSOR.store(FRAME_COUNT, Ordering::SeqCst);
        return None;
    }
    let offset = idx * paging::PAGE_SIZE;
    // The pool is identity-mapped, so its kernel virtual address is also its
    // physical address.
    let base = core::ptr::addr_of!(FRAME_POOL) as u64 + offset as u64;
    Some(Frame::containing(PhysAddr::new(base)))
}

fn note(id: EventId, message: &'static str) {
    log(
        &SERIAL_SINK,
        &Event {
            level: Level::Info,
            id,
            message,
            fields: &[],
        },
    );
}

/// Forward to the shared aarch64 panic bridge (parks the CPU; the run then
/// times out and the harness reports the failure).
#[panic_handler]
fn spawn_el0_timeshare_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
    handle_panic_via_serial(info)
}

/// A [`CapabilityQuery`] granting exactly `CAP_PROC_SPAWN` — the privilege the
/// spawn caller requires (`AGENTS.md` §5.4). It does not widen either program's
/// own authority; it only authorises the *act* of spawning (`AGENTS.md` §16.5).
struct SpawnAuthority;
impl CapabilityQuery for SpawnAuthority {
    fn holds(&self, cap: CapabilityId) -> bool {
        cap == CapabilityId::PROC_SPAWN
    }
}

/// The syscall-dispatch callback both EL0 tasks' `svc` traps reach.
///
/// It mirrors the production bin-crate callback (`dispatch_via_slot`): a
/// rescheduling syscall (`yield`/`exit`) from the running user kthread is
/// suspended back to the dispatcher through [`reschedule_current`], so the two
/// tasks timeshare the CPU. `yield` resumes here on the next dispatch (and the
/// callback `eret`s back into EL0); `exit` reaps the task and never returns to
/// the callback. Any other syscall is unexpected from the fixture program and
/// fails the test loudly (`AGENTS.md` §7).
extern "C" fn dispatch(number: u64, _args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64 {
    #[allow(clippy::cast_possible_truncation)]
    let raw = number as u16;
    if raw == SyscallNumber::YIELD.as_u16() {
        YIELDS.fetch_add(1, Ordering::SeqCst);
        // Suspend the caller; control returns here when it is next dispatched.
        // A `false` would mean no user kthread is published on this CPU — never
        // the case here, since both tasks are user kthreads.
        let _ = reschedule_current(BOOT_CPU, RescheduleAction::Yield);
        0
    } else if raw == SyscallNumber::EXIT.as_u16() {
        EXITS.fetch_add(1, Ordering::SeqCst);
        // Reap the caller: this switches back to the dispatcher and never
        // resumes the task, so the `0` below is unreachable.
        let _ = reschedule_current(BOOT_CPU, RescheduleAction::Exit);
        0
    } else {
        note(TEST_START, "fixture program issued an unexpected syscall");
        qemu_exit::exit_failure(FAIL_UNEXPECTED_SYSCALL);
    }
}

/// Build one isolated EL0 address space from the fixture `image` over the
/// per-space page-table `pool`, returning its stage-1 root and the entry
/// register state. Activates the space (so the user mappings land in it) and
/// builds the image through the production capability-checked, audited
/// `spawn_image` caller. Fails the test with a distinct finisher on any error.
fn build_el0_space(
    pool: &'static PageTablePool,
    image: &LoadImage,
) -> (u64, rustos_arch_api::UserEntry) {
    let Some(arch) = ArchAddressSpace::new_identity_gigapages(pool, IDENTITY_GIB) else {
        qemu_exit::exit_failure(FAIL_POOL);
    };
    // Capture the root before the arch space is moved into the `kernel/mem`
    // wrapper, so the `pre_resume` hook can reactivate `TTBR0_EL1` with it.
    let root_phys = arch.root_phys();
    // SAFETY: the identity map covers the kernel's current `pc`, `sp`, the
    // heap, the frame pool, and the device MMIO (all within `[0, 2 GiB)` on the
    // `virt` board), so enabling/switching it does not move the ground under
    // the running code — exactly `AddressSpace::switch`'s contract. Called on
    // the boot CPU.
    unsafe { arch.switch() };

    let mut space = AddressSpace::new(arch);
    let physmap = DirectPhysMap::identity((IDENTITY_GIB as u64) << 30);
    let request = SpawnRequest {
        image,
        image_bytes: PROGRAM_RXE,
        bias: USER_BIAS,
        stack: UserStack {
            base: USER_STACK_BASE,
            page_count: USER_STACK_PAGES,
        },
        start_block_base: USER_BLOCK_BASE,
        args: &[b"el0"],
        env: &[],
        canary: CANARY,
    };

    // SAFETY: building the image is itself safe; the returned `UserEntry` is
    // only entered later, once its space is reactivated (via the `pre_resume`
    // hook) and the EL1 trap path is installed. The frame source draws
    // identity-mapped frames from `FRAME_POOL`.
    let entry = match unsafe {
        spawn_image(
            &SpawnAuthority,
            &SERIAL_SINK,
            &mut space,
            &physmap,
            &request,
            next_frame,
        )
    } {
        Ok(entry) => entry,
        Err(_) => qemu_exit::exit_failure(FAIL_BUILD),
    };
    (root_phys, entry)
}

/// Boot entry point — the symbol the arch crate's `boot.s` trampoline calls
/// (via `rustos_arch_aarch64_main`).
#[no_mangle]
pub extern "C" fn kernel_main(_dtb: u64) -> ! {
    note(TEST_START, "aarch64 EL0 timeshare test: starting");

    // Enable FP/SIMD at EL1 before any code that may use it: the `rxe` decoder
    // and `build_process_image`'s fills compile to vectorised (NEON)
    // `memcpy`/`memcmp`, which trap as undefined instructions unless
    // `CPACR_EL1.FPEN` is set, which the boot trampoline leaves trapped.
    // SAFETY: this is the boot CPU, called once, before any FP/SIMD executes.
    unsafe { enable_fp_el1() };

    // P3/P4: discover the board from the embedded `virt` device tree.
    let Ok(fdt) = Fdt::new(DTB_BLOB) else {
        qemu_exit::exit_failure(FAIL_GIC_NOT_DISCOVERED);
    };
    let counter_hz = timer_frequency_hz(&fdt);
    if counter_hz == 0 {
        qemu_exit::exit_failure(FAIL_ZERO_FREQ);
    }
    if gic::configure_from_fdt(&fdt).is_none() {
        qemu_exit::exit_failure(FAIL_GIC_NOT_DISCOVERED);
    }

    // Parse the build-time `rxe` blob once against the kernel's own CFI tag;
    // both address spaces are built from the same validated image.
    let Ok(image) = LoadImage::parse(PROGRAM_RXE, &SYSCALL_TABLE_HASH) else {
        qemu_exit::exit_failure(FAIL_PARSE);
    };

    // Build the two isolated EL0 address spaces. Each `build_el0_space`
    // activates its own space; after the second returns, that space is active,
    // and the per-task `pre_resume` hooks reactivate the correct root on every
    // dispatch.
    let (root_a, entry_a) = build_el0_space(&PAGE_TABLES_A, &image);
    let (root_b, entry_b) = build_el0_space(&PAGE_TABLES_B, &image);

    // Bring up the EL1 vectors + GICv2 and install the dispatch callback so the
    // programs' `svc` traps are handled. Interrupts stay masked — dispatch is
    // the cooperative `step` loop below, so the EL0→EL0 context switches are
    // the only mechanism under test.
    // SAFETY: called once on the boot CPU with a stack established and the MMU
    // enabled (the address-space builds switched it on).
    unsafe {
        exceptions::init_vectors();
        gic::init();
    }
    syscall_entry::set_dispatch_callback(dispatch);

    // Build the live scheduler over the arch port.
    // Per-CPU bookkeeping backing for this single-CPU vertical
    // (`AGENTS.md` §24.1).
    static ARCH_STORAGE: rustos_arch_aarch64::Aarch64ArchStorage<1> =
        rustos_arch_aarch64::Aarch64ArchStorage::new();
    let arch = Arc::new(rustos_arch_aarch64::Aarch64Arch::new(
        &ARCH_STORAGE,
        BOOT_CPU,
        counter_hz,
    ));
    let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), arch) else {
        qemu_exit::exit_failure(FAIL_SCHED_NEW);
    };

    // Admit both EL0 tasks as resumable user kthreads. Each runs on its own
    // kernel stack; its `pre_resume` hook reactivates its page-table root
    // (isolation, §4), and its work body `enter_user`s into EL0. The
    // `ContextSwitchHal` is the aarch64 §17.2 context-switch primitive.
    let cs = ContextSwitchHal::new();
    for (root_phys, entry) in [(root_a, entry_a), (root_b, entry_b)] {
        let user_mode = UserMode::new();
        let pre_resume = move |_stack_top: u64| {
            // SAFETY: the MMU is enabled and `root_phys` is the L1 root of a
            // space that identity-maps the low kernel window the running kernel
            // executes from — exactly `activate_user_root`'s contract.
            unsafe { activate_user_root(root_phys) };
        };
        let work = move |_yielder: &mut Yielder<ContextSwitchHal>| {
            // SAFETY: the entered space is active (the `pre_resume` hook just
            // reactivated it) and the EL1 trap vector + dispatch callback are
            // installed, so the program's first `svc` is handled.
            // `build_process_image` mapped the entry/stack as user pages.
            unsafe { user_mode.enter_user(entry) }
        };
        if spawn_user_kthread(&sched, cs, BOOT_CPU, Priority::Normal, pre_resume, work).is_err() {
            qemu_exit::exit_failure(FAIL_SPAWN);
        }
    }
    note(
        TEST_SPAWNED,
        "aarch64 EL0 timeshare test: two EL0 tasks spawned",
    );

    // Cooperative dispatch loop: drive `step` until both EL0 tasks have exited.
    // Each `step` enters a task, which `eret`s into EL0, yields straight back
    // through the dispatch callback's `reschedule_current`, so the two tasks
    // ping-pong through real EL0→EL0 context switches. A switch that never
    // resumed its task would stall the drain and the harness would time out
    // (fail-loud, `AGENTS.md` §7).
    let mut steps = 0u64;
    while sched.live_task_count() != 0 && steps < MAX_STEPS {
        let _ = sched.step(BOOT_CPU);
        steps += 1;
    }
    if sched.live_task_count() != 0 {
        qemu_exit::exit_failure(FAIL_DEADLOCK);
    }
    if YIELDS.load(Ordering::SeqCst) != TASK_COUNT * YIELDS_PER_TASK {
        qemu_exit::exit_failure(FAIL_YIELD_COUNT);
    }
    if EXITS.load(Ordering::SeqCst) != TASK_COUNT {
        qemu_exit::exit_failure(FAIL_EXIT_COUNT);
    }

    note(
        TEST_PASS,
        "aarch64 EL0 timeshare test: two isolated EL0 tasks timeshared one CPU",
    );
    qemu_exit::exit_success();
}
