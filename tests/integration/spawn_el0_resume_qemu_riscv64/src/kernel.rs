//! The freestanding riscv64 RV-X1 test kernel: read the timer rate from the
//! firmware tree, build one isolated U-mode program, admit it as a resumable
//! user kthread, and drive the cooperative `Scheduler::step` loop so it yields
//! and exits through `reschedule_current` (`plans/PI.md` RV-X1, the riscv64
//! sibling of the x86_64 X1 / aarch64 `SP2c` verticals).

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use alloc::sync::Arc;

use rustos_abi::rxe::LoadImage;
use rustos_abi::{CapabilityId, CapabilityQuery, SyscallNumber, SYSCALL_MAX_ARGS};
use rustos_arch_api::{CpuId, EnterUser};
use rustos_arch_riscv64::context_hal::ContextSwitchHal;
use rustos_arch_riscv64::fdt::Fdt;
use rustos_arch_riscv64::paging::{self, activate_user_root};
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

/// Gigabytes of identity map the U-mode address space provides: `[0, 4 GiB)`
/// covers the `virt` board's low MMIO and the RAM base where this kernel runs.
/// [`USER_BIAS`] (64 GiB) sits far above, on freshly walked Sv39 tables.
const IDENTITY_GIGABYTES: usize = 4;

/// User stack base (1 MiB into the high user region) and size. `rustos-rt`
/// carves scratch space off the stack, so it must comfortably exceed it.
const USER_STACK_BASE: u64 = USER_BIAS + 0x10_0000;
/// User stack pages (1.125 MiB > the runtime's scratch span plus call frames).
const USER_STACK_PAGES: u64 = 288;
/// User virtual address the startup-vector block is written at (3 MiB up, well
/// clear of the program image and the stack).
const USER_BLOCK_BASE: u64 = USER_BIAS + 0x30_0000;

/// Per-process stack-canary seed handed to the program (`AGENTS.md` §19.2).
const CANARY: u64 = 0x5520_C000_D15E_A5ED;

/// Physical frames the test hands the spawn build (image segments + the user
/// stack + the startup block, with headroom).
const FRAME_COUNT: usize = 320;

/// Cooperative-loop watchdog: maximum `step` iterations before the test
/// declares the workload deadlocked. Sized generously for QEMU TCG.
const MAX_STEPS: u64 = 5_000_000;

/// Stable audit-event ids for the QEMU transcript (clear of the `4000..5000`
/// `kernel/core` boot range).
const TEST_START: EventId = EventId(4314);
const TEST_SPAWNED: EventId = EventId(4315);
const TEST_PASS: EventId = EventId(4316);
const TEST_FAIL: EventId = EventId(4317);

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
const FAIL_COUNT: u16 = 10;

/// `yield` syscalls observed from the U-mode task.
static YIELDS: AtomicU64 = AtomicU64::new(0);
/// `exit` syscalls observed from the U-mode task.
static EXITS: AtomicU64 = AtomicU64::new(0);

/// Set once the round-trip has been driven so a re-entry cannot re-run it.
static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

/// Static boot heap, in the linker's NOLOAD `.heap` section so the boot
/// trampoline neither zeroes nor counts it in the usable memory map. The user
/// kthread's kernel stack + control block and the spawn caller's
/// startup-vector block allocate from it.
#[link_section = ".heap"]
static mut HEAP: Heap = Heap::ZERO;

/// Global allocator backed by [`HEAP`].
///
/// SAFETY: the page-aligned `HEAP` static outlives the binary and the
/// allocator is its only consumer.
#[global_allocator]
static ALLOCATOR: FreeListAllocator =
    unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

/// Page-table pool backing the Sv39 hierarchy (lives in `.bss`).
static PAGE_TABLE_POOL: paging::PageTablePool = paging::PageTablePool::new();

/// Physical-frame backing store the spawn builder draws user pages from.
/// `align(4096)` so each `PAGE_SIZE` slice is a valid page frame; identity-
/// mapped (its physical address equals its kernel virtual address), so the
/// builder reaches it through [`DirectPhysMap::identity`].
#[repr(C, align(4096))]
struct FramePool([u8; paging::PAGE_SIZE * FRAME_COUNT]);

static mut FRAME_POOL: FramePool = FramePool([0; paging::PAGE_SIZE * FRAME_COUNT]);

/// Monotonic index of the next free [`FRAME_POOL`] frame.
static FRAME_CURSOR: AtomicUsize = AtomicUsize::new(0);

/// Hand out the next identity-mapped physical frame, or `None` when exhausted.
fn next_frame() -> Option<Frame> {
    let idx = FRAME_CURSOR.fetch_add(1, Ordering::SeqCst);
    if idx >= FRAME_COUNT {
        FRAME_CURSOR.store(FRAME_COUNT, Ordering::SeqCst);
        return None;
    }
    let offset = idx * paging::PAGE_SIZE;
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
fn spawn_el0_resume_qemu_riscv64_panic(info: &PanicInfo<'_>) -> ! {
    handle_panic_via_serial(info)
}

/// A [`CapabilityQuery`] granting exactly `CAP_PROC_SPAWN` — the privilege the
/// spawn caller requires (`AGENTS.md` §5.4). It does not widen the program's
/// own authority (`AGENTS.md` §16.5).
struct SpawnAuthority;
impl CapabilityQuery for SpawnAuthority {
    fn holds(&self, cap: CapabilityId) -> bool {
        cap == CapabilityId::PROC_SPAWN
    }
}

/// The syscall-dispatch callback the U-mode task's `ecall` traps reach.
///
/// It mirrors the production bin-crate callback (`dispatch_via_slot`): a
/// rescheduling syscall (`yield`/`exit`) from the running user kthread is
/// suspended back to the dispatcher through [`reschedule_current`]. `yield`
/// resumes here on the next dispatch (and the callback `sret`s back into
/// U-mode); `exit` reaps the task and never returns to the callback. Any other
/// syscall is unexpected from the fixture program and fails the test loudly
/// (`AGENTS.md` §7).
extern "C" fn dispatch(number: u64, _args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64 {
    #[allow(clippy::cast_possible_truncation)]
    let raw = number as u16;
    if raw == SyscallNumber::YIELD.as_u16() {
        YIELDS.fetch_add(1, Ordering::SeqCst);
        // Suspend the caller; control returns here when it is next dispatched.
        // A `false` would mean no user kthread is published on this CPU — never
        // the case here, since the task is a user kthread.
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

/// Boot entry point — the symbol the arch crate's `boot.s` trampoline calls.
#[no_mangle]
pub extern "C" fn kernel_main(hartid: u64, dtb: u64) -> ! {
    if TEST_DRIVEN
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        qemu_exit::exit_failure(FAIL_POOL);
    }

    note(TEST_START, "riscv64 RV-X1 test: building the U-mode image");

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

    // Build the Sv39 address space and activate it (so the user mappings land
    // in the active translation regime), then install the trap vector + the
    // syscall-dispatch callback before any user task runs.
    let Some(arch_space) =
        paging::AddressSpace::new_identity_gigapages(&PAGE_TABLE_POOL, IDENTITY_GIGABYTES)
    else {
        qemu_exit::exit_failure(FAIL_POOL);
    };
    // Capture the `satp` root before the arch space is moved into the
    // `kernel/mem` wrapper, so the per-task `pre_resume` hook can reactivate it
    // on every switch.
    let root_phys = arch_space.root_phys();
    // SAFETY: the identity map covers the kernel's current `pc`, `sp`, heap,
    // frame pool, and device MMIO (all within `[0, 4 GiB)` on `virt`), so the
    // `satp` switch does not move the ground under the running code. Boot hart.
    unsafe { arch_space.switch() };
    // SAFETY: called once on the boot hart with a stack established; only the
    // task's `ecall`s reach the vector (interrupts stay masked, so the
    // scheduler's self-IPI stays pending and the dispatch is the cooperative
    // `step` loop below).
    unsafe { trap::init_traps() };
    syscall_entry::set_dispatch_callback(dispatch);

    // Parse the build-time `rxe` blob against the kernel's own CFI tag.
    let Ok(image) = LoadImage::parse(PROGRAM_RXE, &SYSCALL_TABLE_HASH) else {
        qemu_exit::exit_failure(FAIL_PARSE);
    };

    let mut space = AddressSpace::new(arch_space);
    let physmap = DirectPhysMap::identity((IDENTITY_GIGABYTES as u64) << 30);
    let request = SpawnRequest {
        image: &image,
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
    // only entered later, once the task is dispatched and its `pre_resume` hook
    // has reactivated `satp` (the space is already active here too). The trap
    // vector + dispatch callback are installed above. Frames are drawn from
    // `FRAME_POOL`.
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

    // Admit the U-mode program as a resumable user kthread. Its `pre_resume`
    // hook runs on the dispatcher's context immediately before every switch-in:
    // it reactivates the task's own `satp` root (isolation, §4) — the RV-X1
    // primitive. The kernel-stack top the hook is handed (`_top`) is unused on
    // riscv64: a single user task arms `sscratch` with its own kernel stack on
    // the first `enter_user` (`userentry`), and the RV1 trap path preserves it
    // across a mid-handler park (per-task `sscratch` repointing for *concurrent*
    // tasks is RV-X2). `ContextSwitchHal` is the riscv64 §17.2 context-switch
    // primitive.
    let cs = ContextSwitchHal::new();
    let user_mode = UserMode::new();
    let pre_resume = move |_top: u64| {
        // SAFETY: paging is enabled and `root_phys` is the Sv39 root of the
        // task's space, which maps the low identity window the running
        // dispatcher executes from — exactly `activate_user_root`'s contract.
        unsafe { activate_user_root(root_phys) };
    };
    let work = move |_yielder: &mut Yielder<ContextSwitchHal>| {
        // SAFETY: by the time this body runs the task has been dispatched, so
        // its `pre_resume` hook reactivated `satp`, and the trap vector +
        // dispatch callback are installed; the program's first `ecall` is
        // handled. `build_process_image` mapped the entry/stack as user pages.
        unsafe { user_mode.enter_user(entry) }
    };
    if spawn_user_kthread(&sched, cs, BOOT_CPU, Priority::Normal, pre_resume, work).is_err() {
        qemu_exit::exit_failure(FAIL_SPAWN);
    }
    note(TEST_SPAWNED, "riscv64 RV-X1 test: U-mode task spawned");

    // Cooperative dispatch loop: drive `step` until the U-mode task has exited.
    // Each `step` resumes the task, which `sret`s into U-mode, yields back
    // through the dispatch callback's `reschedule_current`, so it ping-pongs
    // with the dispatcher through real U-mode↔kernel context switches landing
    // on its own kernel stack (the RV1 park-safe path). A switch that never
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
    if YIELDS.load(Ordering::SeqCst) != YIELDS_PER_TASK || EXITS.load(Ordering::SeqCst) != 1 {
        qemu_exit::exit_failure(FAIL_COUNT);
    }

    note(
        TEST_PASS,
        "riscv64 RV-X1 test: U-mode task resumed on its own kernel stack, yielded, and exited",
    );
    qemu_exit::exit_success();
}
