//! The x86_64 X1 test kernel: boot the production pipeline, then on
//! `BootCompleted` build one isolated ring-3 program, admit it as a resumable
//! user kthread, and drive the cooperative `Scheduler::step` loop so it yields
//! and exits through `reschedule_current` (`plans/PI.md` X1).

extern crate alloc;

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use rustos_abi::rxe::LoadImage;
use rustos_abi::{CapabilityId, CapabilityQuery, SyscallNumber, SYSCALL_MAX_ARGS};
use rustos_arch_api::EnterUser;
use rustos_arch_x86_64::context_hal::ContextSwitchHal;
use rustos_arch_x86_64::kernel_arch::{X86_64Arch, X86_64ArchStorage};
use rustos_arch_x86_64::paging::{self, activate_user_root, KERNEL_VMA_BASE};
use rustos_arch_x86_64::userentry::UserMode;
use rustos_arch_x86_64::{qemu_exit, smp, syscall_entry};
use rustos_kernel::kalloc::{Heap, HEAP_BYTES};
use rustos_kernel::{
    boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
};
use rustos_kernel_core::{
    reschedule_current, spawn_image, spawn_user_kthread, RescheduleAction, SpawnRequest, Yielder,
};
use rustos_kernel_mem::{AddressSpace, DirectPhysMap, Frame, PhysAddr, UserStack};
use rustos_kernel_sched_eevdf::{Priority, Scheduler, SchedulerConfig};
use rustos_kernel_syscall::SYSCALL_TABLE_HASH;
use rustos_log::{log, Event, EventId, Level, Sink};

// `PROGRAM_RXE: &[u8]`, `USER_BIAS: u64`, and `YIELDS_PER_TASK: u64`, generated
// by `build.rs`.
include!(concat!(env!("OUT_DIR"), "/program_rxe.rs"));

/// `EventId` emitted when every boot init phase completed.
const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

/// Stable audit-event ids for the QEMU transcript (clear of the `4000..5000`
/// `kernel/core` boot range).
const TEST_START: EventId = EventId(4310);
const TEST_SPAWNED: EventId = EventId(4311);
const TEST_PASS: EventId = EventId(4312);
const TEST_FAIL: EventId = EventId(4313);

/// The single-core slice runs logical CPU 0 on the boot processor.
const BOOT_CPU: u32 = 0;

/// User stack base (1 MiB into the high user region) and size. `rustos-rt`'s
/// `_start` only aligns the stack and calls, so a small stack suffices for the
/// trivial yield-then-exit program; 256 KiB is generous headroom.
const USER_STACK_BASE: u64 = USER_BIAS + 0x10_0000;
/// User stack pages (256 KiB).
const USER_STACK_PAGES: u64 = 64;
/// User virtual address the startup-vector block is written at (3 MiB up, well
/// clear of the program image and the stack).
const USER_BLOCK_BASE: u64 = USER_BIAS + 0x30_0000;

/// Per-process stack-canary seed handed to the program.
const CANARY: u64 = 0x5520_C000_D15E_A5ED;

/// `IA32_EFER` MSR number and its No-Execute-Enable bit (bit 11).
const IA32_EFER: u32 = 0xC000_0080;
const EFER_NXE: u64 = 1 << 11;

/// Physical base of the architectural LAPIC MMIO page (the value
/// `smp::bsp_lapic_id` reads the ID register from and `X86_64Arch::send_ipi`
/// writes the ICR to). 4 KiB-aligned; identity-mapped into the test's address
/// space so those accesses stay valid after the CR3 switch.
const LAPIC_MMIO_BASE: u64 = 0xFEE0_0000;

/// Physical frames the test hands the spawn build.
const FRAME_COUNT: usize = 128;

/// Cooperative-loop watchdog: maximum `step` iterations before the test
/// declares the workload deadlocked. Sized generously for QEMU TCG.
const MAX_STEPS: u64 = 5_000_000;

/// `yield` syscalls observed from the EL0 task.
static YIELDS: AtomicU64 = AtomicU64::new(0);
/// `exit` syscalls observed from the EL0 task.
static EXITS: AtomicU64 = AtomicU64::new(0);

/// Set once the round-trip has been driven so a duplicate `BootCompleted`
/// cannot re-enter the test logic.
static TEST_DRIVEN: AtomicU32 = AtomicU32::new(0);

/// Static heap for the bump allocator (per the production bin); the boot
/// pipeline, the spawn caller's startup-vector block, and the user kthread's
/// kernel stack + control block allocate from it.
static mut HEAP: Heap = Heap::ZERO;

/// Global allocator backed by [`HEAP`].
///
/// SAFETY: the page-aligned `HEAP` static outlives the binary and the
/// allocator is its only consumer.
#[global_allocator]
static ALLOCATOR: FreeListAllocator =
    unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

/// Page-table pool backing the new address space (lives in `.bss`).
static PAGE_TABLE_POOL: paging::PageTablePool = paging::PageTablePool::new();

/// Physical-frame backing store the spawn builder draws user pages from.
/// `align(4096)` so each `PAGE_SIZE` slice is a valid page frame. A higher-half
/// kernel static, so its physical address is its virtual address minus
/// [`KERNEL_VMA_BASE`]; the builder reaches it through a [`DirectPhysMap`] with
/// that same offset.
#[repr(C, align(4096))]
struct FramePool([u8; paging::PAGE_SIZE * FRAME_COUNT]);

static mut FRAME_POOL: FramePool = FramePool([0; paging::PAGE_SIZE * FRAME_COUNT]);

/// Monotonic index of the next free [`FRAME_POOL`] frame.
static FRAME_CURSOR: AtomicUsize = AtomicUsize::new(0);

/// Hand out the next physical frame from [`FRAME_POOL`], or `None` when
/// exhausted. The pool is a higher-half kernel static, so its physical address
/// is its virtual address minus [`KERNEL_VMA_BASE`].
fn next_frame() -> Option<Frame> {
    let idx = FRAME_CURSOR.fetch_add(1, Ordering::SeqCst);
    if idx >= FRAME_COUNT {
        FRAME_CURSOR.store(FRAME_COUNT, Ordering::SeqCst);
        return None;
    }
    let offset = idx * paging::PAGE_SIZE;
    let virt = core::ptr::addr_of!(FRAME_POOL) as u64 + offset as u64;
    Some(Frame::containing(PhysAddr::new(virt - KERNEL_VMA_BASE)))
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

/// Forward to the shared bridge in `rustos_kernel`.
#[panic_handler]
fn spawn_el0_resume_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
    handle_panic_via_kernel_core(info)
}

/// A [`CapabilityQuery`] granting exactly `CAP_PROC_SPAWN` — the privilege the
/// spawn caller requires. It does not widen the program's
/// own authority.
struct SpawnAuthority;
impl CapabilityQuery for SpawnAuthority {
    fn holds(&self, cap: CapabilityId) -> bool {
        cap == CapabilityId::PROC_SPAWN
    }
}

/// The syscall-dispatch callback the ring-3 task's `syscall` traps reach.
///
/// It mirrors the production bin-crate callback (`dispatch_via_slot`): a
/// rescheduling syscall (`yield`/`exit`) from the running user kthread is
/// suspended back to the dispatcher through [`reschedule_current`]. `yield`
/// resumes here on the next dispatch (and the callback `sysret`s back into ring
/// 3); `exit` reaps the task and never returns to the callback. Any other
/// syscall is unexpected from the fixture program and fails the test loudly.
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
        qemu_exit::exit_failure();
    }
}

/// Outer audit sink: replays every event to serial (so the QEMU transcript
/// captures the boot timeline) and, on the single [`BOOT_COMPLETED_EVENT_ID`],
/// drives [`run_resume`].
struct BootCompletedSink;

impl Sink for BootCompletedSink {
    fn write_event(&self, event: &Event<'_>) {
        SerialSink::new().write_event(event);

        if event.id == BOOT_COMPLETED_EVENT_ID
            && TEST_DRIVEN
                .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            run_resume();
        }
    }
}

static AUDIT_SINK: BootCompletedSink = BootCompletedSink;

/// Build the ring-3 image, admit it as a resumable user kthread, and drive the
/// cooperative `step` loop until it yields its full count and exits. Never
/// returns (it exits the VM on PASS or FAIL).
fn run_resume() -> ! {
    note(TEST_START, "x86_64 X1 test: building the ring-3 image");

    // Enable `IA32_EFER.NXE` so the W^X No-Execute leaf bit the adapter sets on
    // data/rodata pages is honoured rather than reserved.
    // SAFETY: reading and writing `IA32_EFER` is the documented enable
    // sequence; it runs once on the BSP and only sets bit 11, preserving
    // `SCE`/`LME`/`LMA` the boot pipeline established.
    unsafe {
        let lo: u32;
        let hi: u32;
        core::arch::asm!(
            "rdmsr",
            in("ecx") IA32_EFER,
            out("eax") lo,
            out("edx") hi,
            options(nostack, preserves_flags),
        );
        let efer = (((hi as u64) << 32) | lo as u64) | EFER_NXE;
        core::arch::asm!(
            "wrmsr",
            in("ecx") IA32_EFER,
            in("eax") efer as u32,
            in("edx") (efer >> 32) as u32,
            options(nostack, preserves_flags),
        );
    }

    // Fresh address space (low 32 MiB identity + higher-half kernel window) and
    // activate it before any user mapping is added.
    let Some(mut arch_space) = paging::AddressSpace::new_identity_first_32mib(&PAGE_TABLE_POOL)
    else {
        note(TEST_FAIL, "X1 test: page-table pool exhausted");
        qemu_exit::exit_failure();
    };
    // Identity-map the LAPIC MMIO page (~3.98 GiB) into the new space. Unlike
    // the `mem_map`/`enter_user` siblings — which never touch the LAPIC after
    // the CR3 switch — this test builds a live scheduler over `X86_64Arch`,
    // whose `bsp_lapic_id` reads the LAPIC ID register and whose spawn-time
    // self-IPI writes the LAPIC ICR; the minimal new space maps only the low
    // 32 MiB + higher-half kernel window, so without this those accesses would
    // fault after the switch (fail closed if the pool is exhausted).
    if arch_space
        .map_4k(&PAGE_TABLE_POOL, LAPIC_MMIO_BASE, LAPIC_MMIO_BASE, true)
        .is_none()
    {
        note(TEST_FAIL, "X1 test: could not map LAPIC MMIO page");
        qemu_exit::exit_failure();
    }
    // Capture the CR3 root before the arch space is moved into the `kernel/mem`
    // wrapper, so the per-task `pre_resume` hook can reload it on every switch.
    let root_phys = arch_space.pml4_phys();
    // SAFETY: the new space maps the low 32 MiB, the higher-half kernel
    // window, and the LAPIC MMIO page, so the executing RIP, the current
    // stack, the per-CPU `swapgs` TLS, the page-table pool, the frame pool,
    // the heap, `dispatch`, and every LAPIC access stay mapped across the CR3
    // switch.
    unsafe { arch_space.switch() };
    syscall_entry::set_dispatch_callback(dispatch);

    // Parse the build-time `rxe` blob against the kernel's own CFI tag.
    let Ok(image) = LoadImage::parse(PROGRAM_RXE, &SYSCALL_TABLE_HASH) else {
        note(TEST_FAIL, "X1 test: fixture rxe image failed to parse");
        qemu_exit::exit_failure();
    };

    let mut space = AddressSpace::new(arch_space);
    let physmap = DirectPhysMap::new(KERNEL_VMA_BASE, 1 << 30);
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
    // only entered later, once the task is dispatched and its `pre_resume`
    // hook has reloaded CR3 (the space is already active here too). The GDT
    // user selectors / TSS / `syscall` entry were installed during boot, and
    // the dispatch callback is installed above. Frames are drawn from
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
        Err(_) => {
            note(TEST_FAIL, "X1 test: spawn_image failed");
            qemu_exit::exit_failure();
        }
    };

    // Build the live scheduler over the production arch handle. Interrupts stay
    // masked, so dispatch is the cooperative `step` loop below (the spawn-time
    // self-IPI to the LAPIC ICR is latched and never delivered).
    let bsp_id = smp::bsp_lapic_id();
    // This vertical drives a single CPU (the BSP, dense id 0), so its
    // per-CPU bookkeeping is sized to one slot (capacity matches the machine the caller drives, not a baked-in
    // `MAX_CPUS`).
    let cpu_to_lapic: [Option<u8>; 1] = [Some(bsp_id)];
    // The arch handle borrows its per-CPU bookkeeping from a caller-sized
    // `&'static` backing; `run` runs once, so a
    // function-local `static` is sound and needs no allocator.
    static ARCH_STORAGE: X86_64ArchStorage<1> = X86_64ArchStorage::new();
    let Ok(arch) = X86_64Arch::new(&ARCH_STORAGE, 0, bsp_id, &cpu_to_lapic) else {
        note(TEST_FAIL, "X1 test: X86_64Arch::new failed");
        qemu_exit::exit_failure();
    };
    let arch = alloc::sync::Arc::new(arch);
    let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), arch) else {
        note(TEST_FAIL, "X1 test: Scheduler::new failed");
        qemu_exit::exit_failure();
    };

    // Admit the EL0 program as a resumable user kthread. Its `pre_resume` hook
    // runs on the dispatcher's context immediately before every switch-in: it
    // reloads CR3 to the task's own root (isolation) and — the X1 primitive
    // — repoints the per-CPU `syscall` entry stack at *this* task's own kernel
    // stack (`set_kernel_rsp0`), the value the seam hands it. Its work body
    // `enter_user`s into ring 3. `ContextSwitchHal` is the x86_64
    // context-switch primitive.
    let cs = ContextSwitchHal::new();
    let user_mode = UserMode::new();
    let pre_resume = move |kernel_stack_top: u64| {
        // Repoint the per-CPU syscall entry stack at this task's own kernel
        // stack before the switch-in (`plans/PI.md` §X). A rejected value
        // (it is validated canonical/aligned/kernel-half) leaves the slot
        // unchanged and the next syscall would fault loudly — fail closed,
        // never a silent wrong stack.
        if syscall_entry::set_kernel_rsp0(BOOT_CPU as usize, kernel_stack_top).is_err() {
            note(TEST_FAIL, "X1 test: set_kernel_rsp0 rejected the stack top");
            qemu_exit::exit_failure();
        }
        // SAFETY: paging is enabled and `root_phys` is the PML4 of the task's
        // space, which maps the low identity + higher-half kernel window the
        // running dispatcher executes from — exactly `activate_user_root`'s
        // contract.
        unsafe { activate_user_root(root_phys) };
    };
    let work = move |_yielder: &mut Yielder<ContextSwitchHal>| {
        // SAFETY: by the time this body runs the task has been dispatched, so
        // its `pre_resume` hook reloaded CR3 + repointed the entry stack, and
        // the GDT user selectors / TSS / `syscall` entry + dispatch callback
        // are installed; the program's first `syscall` is handled.
        // `build_process_image` mapped the entry/stack as user pages.
        unsafe { user_mode.enter_user(entry) }
    };
    if spawn_user_kthread(&sched, cs, BOOT_CPU, Priority::Normal, pre_resume, work).is_err() {
        note(TEST_FAIL, "X1 test: spawn_user_kthread failed");
        qemu_exit::exit_failure();
    }
    note(TEST_SPAWNED, "x86_64 X1 test: ring-3 task spawned");

    // Cooperative dispatch loop: drive `step` until the EL0 task has exited.
    // Each `step` resumes the task, which `sysret`s into ring 3, yields back
    // through the dispatch callback's `reschedule_current`, so it ping-pongs
    // with the dispatcher through real ring-3↔kernel context switches landing
    // on its own kernel stack. A switch that never resumed its task would stall
    // the drain and the harness would time out (fail-loud).
    let mut steps = 0u64;
    while sched.live_task_count() != 0 && steps < MAX_STEPS {
        let _ = sched.step(BOOT_CPU);
        steps += 1;
    }
    if sched.live_task_count() != 0 {
        note(
            TEST_FAIL,
            "X1 test: deadlock — task remained after MAX_STEPS",
        );
        qemu_exit::exit_failure();
    }
    if YIELDS.load(Ordering::SeqCst) != YIELDS_PER_TASK {
        note(TEST_FAIL, "X1 test: wrong yield count");
        qemu_exit::exit_failure();
    }
    if EXITS.load(Ordering::SeqCst) != 1 {
        note(TEST_FAIL, "X1 test: task did not exit exactly once");
        qemu_exit::exit_failure();
    }

    note(
        TEST_PASS,
        "x86_64 X1 test: ring-3 task resumed on its own kernel stack, yielded, and exited",
    );
    qemu_exit::exit_success();
}

/// The symbol the arch crate's boot trampoline calls.
#[no_mangle]
pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
    boot(multiboot_info, &SERIAL_SINK, &AUDIT_SINK)
}
