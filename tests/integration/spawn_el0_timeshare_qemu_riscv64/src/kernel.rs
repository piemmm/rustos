//! The freestanding riscv64 RV-X2 test kernel: build **two** isolated U-mode
//! programs and timeshare them under the live scheduler on the `virt` board
//! (`plans/PI.md` RV-X2, the riscv64 sibling of the x86_64 X2 / aarch64 `SP2c`
//! timeshare verticals).

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use alloc::sync::Arc;

use rustos_abi::rxe::LoadImage;
use rustos_abi::{CapabilityId, CapabilityQuery, SyscallNumber, SYSCALL_MAX_ARGS};
use rustos_arch_api::{CpuId, EnterUser, UserEntry};
use rustos_arch_riscv64::context_hal::ContextSwitchHal;
use rustos_arch_riscv64::fdt::Fdt;
use rustos_arch_riscv64::paging::{self, activate_user_root, AddressSpace as ArchAddressSpace};
use rustos_arch_riscv64::userentry::UserMode;
use rustos_arch_riscv64::{
    handle_panic_via_serial, qemu_exit, syscall_entry, trap, RiscvArch, RiscvArchStorage,
    SERIAL_SINK,
};
use rustos_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
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

/// The single-hart slice runs logical CPU 0 on the boot hart.
const BOOT_CPU: CpuId = 0;

/// The two U-mode tasks the test timeshares.
const TASK_COUNT: u64 = 2;

/// Gigabytes of identity map each U-mode address space provides: `[0, 4 GiB)`
/// covers the `virt` board's low MMIO and the RAM base where this kernel runs,
/// so switching between the two spaces never moves the ground under the running
/// kernel (every kernel pointer keeps its identity address). [`USER_BIAS`]
/// (64 GiB) sits far above, on freshly walked Sv39 tables.
const IDENTITY_GIGABYTES: usize = 4;

/// User stack base (1 MiB into the high user region) and size. `rustos-rt`
/// carves scratch space off the stack, so it must comfortably exceed it.
const USER_STACK_BASE: u64 = USER_BIAS + 0x10_0000;
/// User stack pages (1.125 MiB > the runtime's scratch span plus call frames).
const USER_STACK_PAGES: u64 = 288;
/// User virtual address the startup-vector block is written at (3 MiB up, well
/// clear of the program image and the stack).
const USER_BLOCK_BASE: u64 = USER_BIAS + 0x30_0000;

/// Per-process stack-canary seed handed to each program (`AGENTS.md` §19.2).
const CANARY: u64 = 0x5520_C000_D15E_A5ED;

/// Physical frames the test hands the two spawn builds (image segments + the
/// user stacks + the startup blocks for both programs, with headroom). The
/// page-table frames come from the per-space [`paging::PageTablePool`]s, not
/// from here.
const FRAME_COUNT: usize = 640;

/// Cooperative-loop watchdog: maximum `step` iterations before the test
/// declares the workload deadlocked. Sized generously for QEMU TCG.
const MAX_STEPS: u64 = 5_000_000;

/// Stable audit-event ids for the QEMU transcript (clear of the `4000..5000`
/// `kernel/core` boot range).
const TEST_START: EventId = EventId(4324);
const TEST_SPAWNED: EventId = EventId(4325);
const TEST_PASS: EventId = EventId(4326);
const TEST_FAIL: EventId = EventId(4327);

/// `SiFive` Test failure codes, distinct per failure site.
const FAIL_NO_TIMEBASE: u16 = 1;
const FAIL_UNEXPECTED_HART: u16 = 2;
const FAIL_POOL: u16 = 3;
const FAIL_PARSE: u16 = 4;
const FAIL_BUILD: u16 = 5;
const FAIL_SCHED_NEW: u16 = 6;
const FAIL_SPAWN: u16 = 7;
const FAIL_UNEXPECTED_SYSCALL: u16 = 8;
const FAIL_DEADLOCK: u16 = 9;
const FAIL_YIELD_COUNT: u16 = 10;
const FAIL_EXIT_COUNT: u16 = 11;

/// Total `yield` syscalls observed across both U-mode tasks.
static YIELDS: AtomicU64 = AtomicU64::new(0);
/// Total `exit` syscalls observed across both U-mode tasks.
static EXITS: AtomicU64 = AtomicU64::new(0);

/// Set once the round-trip has been driven so a re-entry cannot re-run it.
static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

/// Static boot heap, in the linker's NOLOAD `.heap` section so the boot
/// trampoline neither zeroes nor counts it in the usable memory map. The two
/// user kthreads' kernel stacks + control blocks and the spawn caller's
/// startup-vector blocks allocate from it.
#[link_section = ".heap"]
static mut HEAP: Heap = Heap::ZERO;

/// Global allocator backed by [`HEAP`].
///
/// SAFETY: the page-aligned `HEAP` static outlives the binary and the
/// allocator is its only consumer.
#[global_allocator]
static ALLOCATOR: FreeListAllocator =
    unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

/// Per-space page-table pools (one per U-mode address space). Each backs an
/// Sv39 hierarchy whose root [`activate_user_root`] reinstalls (via `satp`)
/// before every switch into its task, so the two tasks stay hardware-isolated
/// (`AGENTS.md` §4).
static PAGE_TABLES_A: paging::PageTablePool = paging::PageTablePool::new();
static PAGE_TABLES_B: paging::PageTablePool = paging::PageTablePool::new();

/// Physical-frame backing store the spawn builders draw user pages from.
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

/// Forward to the shared riscv64 panic bridge (parks the hart; the run then
/// times out and the harness reports the failure).
#[panic_handler]
fn spawn_el0_timeshare_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
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

/// The syscall-dispatch callback both U-mode tasks' `ecall` traps reach.
///
/// It mirrors the production bin-crate callback (`dispatch_via_slot`): a
/// rescheduling syscall (`yield`/`exit`) from the running user kthread is
/// suspended back to the dispatcher through [`reschedule_current`], so the two
/// tasks timeshare the hart. `yield` resumes here on the next dispatch (and the
/// callback `sret`s back into U-mode); `exit` reaps the task and never returns
/// to the callback. Any other syscall is unexpected from the fixture program
/// and fails the test loudly (`AGENTS.md` §7).
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
        note(TEST_FAIL, "fixture program issued an unexpected syscall");
        qemu_exit::exit_failure(FAIL_UNEXPECTED_SYSCALL);
    }
}

/// Build one isolated U-mode address space from the fixture `image` over the
/// per-space page-table `pool`, returning its Sv39 root and the entry register
/// state. Activates the space (so the user mappings land in it) and builds the
/// image through the production capability-checked, audited `spawn_image`
/// caller. Fails the test with a distinct finisher on any error.
fn build_user_space(pool: &'static paging::PageTablePool, image: &LoadImage) -> (u64, UserEntry) {
    let Some(arch) = ArchAddressSpace::new_identity_gigapages(pool, IDENTITY_GIGABYTES) else {
        qemu_exit::exit_failure(FAIL_POOL);
    };
    // Capture the `satp` root before the arch space is moved into the
    // `kernel/mem` wrapper, so the per-task `pre_resume` hook can reactivate it
    // on every switch.
    let root_phys = arch.root_phys();
    // SAFETY: the identity map covers the kernel's current `pc`, `sp`, heap,
    // frame pool, and device MMIO (all within `[0, 4 GiB)` on `virt`), so the
    // `satp` switch does not move the ground under the running code. Boot hart.
    unsafe { arch.switch() };

    let mut space = AddressSpace::new(arch);
    let physmap = DirectPhysMap::identity((IDENTITY_GIGABYTES as u64) << 30);
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
    // hook) and the trap path is installed. The frame source draws identity-
    // mapped frames from `FRAME_POOL`.
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

/// Boot entry point — the symbol the arch crate's `boot.s` trampoline calls.
#[no_mangle]
pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
    if TEST_DRIVEN
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        qemu_exit::exit_failure(FAIL_POOL);
    }

    note(TEST_START, "riscv64 RV-X2 test: building two U-mode images");

    // Read the timer frequency from the firmware tree. Fail closed (finisher)
    // if it is omitted rather than guessing a divisor (`AGENTS.md` §5.4).
    // SAFETY: `dtb` is the verbatim `a1` pointer OpenSBI handed the boot hart;
    // `boot.s` forwards it unchanged.
    let Some(timebase) = (unsafe { Fdt::from_ptr(dtb as *const u8) })
        .ok()
        .and_then(|f| f.timebase_frequency())
    else {
        qemu_exit::exit_failure(FAIL_NO_TIMEBASE);
    };

    // The single-hart slice only brings up logical CPU 0 on hart 0.
    if hartid != u64::from(BOOT_CPU) {
        qemu_exit::exit_failure(FAIL_UNEXPECTED_HART);
    }

    // Parse the build-time `rxe` blob once against the kernel's own CFI tag;
    // both address spaces are built from the same validated image.
    let Ok(image) = LoadImage::parse(PROGRAM_RXE, &SYSCALL_TABLE_HASH) else {
        qemu_exit::exit_failure(FAIL_PARSE);
    };

    // Build the two isolated U-mode address spaces. Each `build_user_space`
    // activates its own space; after the second returns, that space is active,
    // and the per-task `pre_resume` hooks reactivate the correct root on every
    // dispatch.
    let (root_a, entry_a) = build_user_space(&PAGE_TABLES_A, &image);
    let (root_b, entry_b) = build_user_space(&PAGE_TABLES_B, &image);

    // Install the trap vector + the syscall-dispatch callback before any user
    // task runs. The vector lives in the kernel's identity window present in
    // both spaces, so it is reachable whichever space is active.
    // SAFETY: called once on the boot hart with a stack established; only the
    // tasks' `ecall`s reach the vector (interrupts stay masked, so the
    // scheduler's self-IPI stays pending and the dispatch is the cooperative
    // `step` loop below).
    unsafe { trap::init_traps() };
    syscall_entry::set_dispatch_callback(dispatch);

    // Build the live scheduler over the production arch handle. Interrupts stay
    // masked, so dispatch is the cooperative `step` loop below (the spawn-time
    // self-IPI via SBI stays pending and is never delivered).
    // Single-hart slice: one per-CPU slot, owned by an allocator-free
    // `static` backing (`AGENTS.md` §24.1).
    static STORAGE: RiscvArchStorage<1> = RiscvArchStorage::new();
    let arch = Arc::new(RiscvArch::new(&STORAGE, BOOT_CPU, timebase));
    let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), arch) else {
        qemu_exit::exit_failure(FAIL_SCHED_NEW);
    };

    // Admit both U-mode tasks as resumable user kthreads. Each runs on its own
    // kernel stack; its `pre_resume` hook reactivates its page-table root
    // (isolation, §4), and its work body `enter_user`s into U-mode. The
    // `ContextSwitchHal` is the riscv64 §17.2 context-switch primitive.
    //
    // The kernel-stack top the `pre_resume` hook is handed (`_top`) is unused on
    // riscv64: `sscratch` is per-task hardware state — `userentry::enter_user`
    // arms it with the task's own kernel-stack top on first entry, and the RV1
    // trap vector re-arms it from each task's own kernel-stack frame on every
    // U-return (`trap.s`: `sscratch = sp + TRAP_FRAME_SIZE`), so a trap from the
    // resumed task always lands on *its* kernel stack with no dispatcher-side
    // repointing (unlike x86_64's per-CPU `set_kernel_rsp0`).
    let cs = ContextSwitchHal::new();
    for (root_phys, entry) in [(root_a, entry_a), (root_b, entry_b)] {
        let user_mode = UserMode::new();
        let pre_resume = move |_top: u64| {
            // SAFETY: paging is enabled and `root_phys` is the Sv39 root of a
            // space that maps the low identity window the running dispatcher
            // executes from — exactly `activate_user_root`'s contract.
            unsafe { activate_user_root(root_phys) };
        };
        let work = move |_yielder: &mut Yielder<ContextSwitchHal>| {
            // SAFETY: by the time this body runs the task has been dispatched,
            // so its `pre_resume` hook reactivated `satp`, and the trap vector +
            // dispatch callback are installed; the program's first `ecall` is
            // handled. `build_process_image` mapped the entry/stack as user
            // pages.
            unsafe { user_mode.enter_user(entry) }
        };
        if spawn_user_kthread(&sched, cs, BOOT_CPU, Priority::Normal, pre_resume, work).is_err() {
            qemu_exit::exit_failure(FAIL_SPAWN);
        }
    }
    note(TEST_SPAWNED, "riscv64 RV-X2 test: two U-mode tasks spawned");

    // Cooperative dispatch loop: drive `step` until both U-mode tasks have
    // exited. Each `step` resumes a task, which `sret`s into U-mode, yields back
    // through the dispatch callback's `reschedule_current`, so the two tasks
    // ping-pong with the dispatcher through real U-mode↔kernel context switches
    // landing on their own kernel stacks (the RV1 park-safe path). A switch that
    // never resumed its task would stall the drain and the harness would time
    // out (fail-loud, `AGENTS.md` §7).
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
        "riscv64 RV-X2 test: two isolated U-mode tasks timeshared one hart",
    );
    qemu_exit::exit_success();
}
