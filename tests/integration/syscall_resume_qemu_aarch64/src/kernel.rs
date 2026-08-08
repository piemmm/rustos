//! Freestanding kernel for the deterministic syscall-continuation witness.

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use alloc::sync::Arc;

use tairix_abi::rxe::LoadImage;
use tairix_abi::{CapabilityId, CapabilityQuery, SyscallNumber, SYSCALL_MAX_ARGS};
use tairix_arch_aarch64::context_hal::ContextSwitchHal;
use tairix_arch_aarch64::kernel_arch::timer_frequency_hz;
use tairix_arch_aarch64::paging::{
    self, activate_user_root, AddressSpace as ArchAddressSpace, PageTablePool,
};
use tairix_arch_aarch64::userentry::UserMode;
use tairix_arch_aarch64::{
    enable_fp_el1, exceptions, gic, handle_panic_via_serial, qemu_exit, syscall_entry, SERIAL_SINK,
};
use tairix_arch_api::{CpuId, EnterUser};
use tairix_fdt::Fdt;
use tairix_kalloc::FreeListAllocator;
use tairix_kernel_core::{
    note_preempt_tick, reschedule_current, spawn_image, spawn_user_kthread, take_preempt_pending,
    RescheduleAction, SpawnMode, SpawnRequest, Yielder,
};
use tairix_kernel_mem::{AddressSpace, DirectPhysMap, Frame, PhysAddr, UserStack};
use tairix_kernel_sched_cfq::{Priority, Scheduler, SchedulerConfig};
use tairix_kernel_syscall::SYSCALL_TABLE_HASH;
use tairix_log::{log, Event, EventId, Level};

include!(concat!(env!("OUT_DIR"), "/program_rxe.rs"));
include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

const BOOT_CPU: CpuId = 0;
const IDENTITY_GIB: usize = 2;
const USER_STACK_BASE: u64 = USER_BIAS + 0x10_0000;
const USER_STACK_PAGES: u64 = 64;
const USER_BLOCK_BASE: u64 = USER_BIAS + 0x30_0000;
const CANARY: u64 = 0x51a7_c011_71a0_ca5e;
const EXPECTED_READING: u64 = 0x51a7_c011_71a0_0001;
const FRAME_COUNT: usize = 256;
const HEAP_SIZE: usize = 2 * 1024 * 1024;
const MAX_STEPS: u64 = 1_000_000;

const TEST_START: EventId = EventId(4293);
const TEST_PASS: EventId = EventId(4294);

const FAIL_FDT: u16 = 1;
const FAIL_PARSE: u16 = 2;
const FAIL_POOL: u16 = 3;
const FAIL_BUILD: u16 = 4;
const FAIL_SCHEDULER: u16 = 5;
const FAIL_PARENT: u16 = 6;
const FAIL_CHILD: u16 = 7;
const FAIL_UNEXPECTED_SYSCALL: u16 = 8;
const FAIL_RESCHEDULE: u16 = 9;
const FAIL_ORDER: u16 = 10;
const FAIL_DEADLOCK: u16 = 11;
const FAIL_RESULT: u16 = 12;

static CLOCK_CALLS: AtomicU64 = AtomicU64::new(0);
static PARENT_EXIT_STATUS: AtomicU64 = AtomicU64::new(u64::MAX);
static EXIT_CALLS: AtomicU64 = AtomicU64::new(0);
static CHILD_PARK_ENTERED: AtomicBool = AtomicBool::new(false);
static CHILD_RESUMED: AtomicBool = AtomicBool::new(false);

#[repr(C, align(4096))]
struct HeapStore([u8; HEAP_SIZE]);

static mut HEAP: HeapStore = HeapStore([0; HEAP_SIZE]);

/// SAFETY: the page-aligned static outlives the kernel and this allocator is
/// its sole owner.
#[global_allocator]
static ALLOCATOR: FreeListAllocator =
    unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_SIZE) };

static PARENT_PAGE_TABLES: PageTablePool = PageTablePool::new();
static CHILD_PAGE_TABLES: PageTablePool = PageTablePool::new();

#[repr(C, align(4096))]
struct FramePool([u8; paging::PAGE_SIZE * FRAME_COUNT]);

static mut FRAME_POOL: FramePool = FramePool([0; paging::PAGE_SIZE * FRAME_COUNT]);
static FRAME_CURSOR: AtomicUsize = AtomicUsize::new(0);

fn next_frame() -> Option<Frame> {
    let index = FRAME_CURSOR.fetch_add(1, Ordering::SeqCst);
    if index >= FRAME_COUNT {
        FRAME_CURSOR.store(FRAME_COUNT, Ordering::SeqCst);
        return None;
    }
    let base = core::ptr::addr_of!(FRAME_POOL) as u64 + (index * paging::PAGE_SIZE) as u64;
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

#[panic_handler]
fn syscall_resume_panic(info: &PanicInfo<'_>) -> ! {
    handle_panic_via_serial(info)
}

struct SpawnAuthority;

impl CapabilityQuery for SpawnAuthority {
    fn holds(&self, cap: CapabilityId) -> bool {
        cap == CapabilityId::PROC_SPAWN
    }
}

extern "C" fn dispatch(number: u64, args_ptr: *const [u64; SYSCALL_MAX_ARGS]) -> u64 {
    #[allow(clippy::cast_possible_truncation)]
    let raw = number as u16;
    if raw == SyscallNumber::CLOCK_GET.as_u16() {
        match CLOCK_CALLS.fetch_add(1, Ordering::SeqCst) {
            0 => {
                note_preempt_tick(BOOT_CPU);
                if !take_preempt_pending(BOOT_CPU)
                    || !reschedule_current(BOOT_CPU, RescheduleAction::Yield)
                {
                    qemu_exit::exit_failure(FAIL_RESCHEDULE);
                }
                if !CHILD_PARK_ENTERED.load(Ordering::SeqCst)
                    || CHILD_RESUMED.load(Ordering::SeqCst)
                {
                    qemu_exit::exit_failure(FAIL_ORDER);
                }
            }
            1 => {
                CHILD_PARK_ENTERED.store(true, Ordering::SeqCst);
                if !reschedule_current(BOOT_CPU, RescheduleAction::Park) {
                    qemu_exit::exit_failure(FAIL_RESCHEDULE);
                }
                CHILD_RESUMED.store(true, Ordering::SeqCst);
            }
            _ => qemu_exit::exit_failure(FAIL_UNEXPECTED_SYSCALL),
        }
        EXPECTED_READING
    } else if raw == SyscallNumber::EXIT.as_u16() {
        // SAFETY: the architecture syscall trampoline passes a live pointer to
        // its fixed-size copied argument array for the duration of this call.
        let status = unsafe { (*args_ptr)[0] };
        let exit_index = EXIT_CALLS.fetch_add(1, Ordering::SeqCst);
        if exit_index == 0 {
            if !CHILD_PARK_ENTERED.load(Ordering::SeqCst) || CHILD_RESUMED.load(Ordering::SeqCst) {
                qemu_exit::exit_failure(FAIL_ORDER);
            }
            PARENT_EXIT_STATUS.store(status, Ordering::SeqCst);
        } else if exit_index > 1 {
            qemu_exit::exit_failure(FAIL_UNEXPECTED_SYSCALL);
        }
        if !reschedule_current(BOOT_CPU, RescheduleAction::Exit) {
            qemu_exit::exit_failure(FAIL_RESCHEDULE);
        }
        0
    } else {
        qemu_exit::exit_failure(FAIL_UNEXPECTED_SYSCALL)
    }
}

fn build_el0_space(
    image: &LoadImage,
    page_tables: &'static PageTablePool,
) -> (u64, tairix_arch_api::UserEntry) {
    let Some(arch) = ArchAddressSpace::new_identity_gigapages(page_tables, IDENTITY_GIB) else {
        qemu_exit::exit_failure(FAIL_POOL);
    };
    let root_phys = arch.root_phys();
    // SAFETY: the identity map covers the executing kernel, RAM, and virt-board
    // MMIO before this one-time boot-CPU switch.
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
        args: &[b"syscall-resume"],
        env: &[],
        canary: CANARY,
    };
    // SAFETY: the returned entry is used only after its retained page-table
    // root is activated by the user kthread's pre-resume hook.
    let entry = match unsafe {
        spawn_image(
            &SpawnAuthority,
            SpawnMode::General,
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

#[no_mangle]
pub extern "C" fn kernel_main(_dtb: u64) -> ! {
    note(TEST_START, "aarch64 syscall continuation test: starting");
    // SAFETY: this is the boot CPU before code that may use FP/SIMD.
    unsafe { enable_fp_el1() };

    let Ok(fdt) = Fdt::new(DTB_BLOB) else {
        qemu_exit::exit_failure(FAIL_FDT);
    };
    let counter_hz = timer_frequency_hz(&fdt);
    if counter_hz == 0 || gic::configure_from_fdt(&fdt).is_none() {
        qemu_exit::exit_failure(FAIL_FDT);
    }
    let Ok(image) = LoadImage::parse(PROGRAM_RXE, &SYSCALL_TABLE_HASH) else {
        qemu_exit::exit_failure(FAIL_PARSE);
    };
    let (parent_root, parent_entry) = build_el0_space(&image, &PARENT_PAGE_TABLES);
    let (child_root, child_entry) = build_el0_space(&image, &CHILD_PAGE_TABLES);

    // SAFETY: one boot CPU, with MMU and stack established.
    unsafe {
        exceptions::init_vectors();
        gic::init();
    }
    syscall_entry::set_dispatch_callback(dispatch);

    static ARCH_STORAGE: tairix_arch_aarch64::Aarch64ArchStorage<1> =
        tairix_arch_aarch64::Aarch64ArchStorage::new();
    let arch = Arc::new(tairix_arch_aarch64::Aarch64Arch::new(
        &ARCH_STORAGE,
        BOOT_CPU,
        counter_hz,
    ));
    if tairix_kernel_core::initialize_cpu_state(1).is_err() {
        qemu_exit::exit_failure(FAIL_SCHEDULER);
    }
    let Ok(scheduler) = Scheduler::new(SchedulerConfig::defaults_for(1), arch) else {
        qemu_exit::exit_failure(FAIL_SCHEDULER);
    };

    let context_switch = ContextSwitchHal::new();
    let parent_user_mode = UserMode::new();
    let parent_pre_resume = move |_stack_top: u64| {
        // SAFETY: `parent_root` is the retained L1 root built above and the MMU
        // is enabled; the mapping includes the running kernel window.
        unsafe { activate_user_root(parent_root) };
    };
    let parent = spawn_user_kthread(
        &scheduler,
        context_switch,
        BOOT_CPU,
        Priority::Normal,
        parent_pre_resume,
        move |_yielder: &mut Yielder<ContextSwitchHal>| {
            // SAFETY: the parent space is active and vectors plus the syscall
            // callback are installed, so both its clock and exit traps return
            // through this kernel.
            unsafe { parent_user_mode.enter_user(parent_entry) }
        },
    );
    if parent.is_err() {
        qemu_exit::exit_failure(FAIL_PARENT);
    }

    let child_user_mode = UserMode::new();
    let child_pre_resume = move |_stack_top: u64| {
        // SAFETY: `child_root` is the retained L1 root built above and the MMU
        // is enabled; the mapping includes the running kernel window.
        unsafe { activate_user_root(child_root) };
    };
    let child = match spawn_user_kthread(
        &scheduler,
        context_switch,
        BOOT_CPU,
        Priority::Normal,
        child_pre_resume,
        move |_yielder: &mut Yielder<ContextSwitchHal>| {
            // SAFETY: the child space is active and its ordinary syscall is
            // deliberately parked by `dispatch` before returning.
            unsafe { child_user_mode.enter_user(child_entry) }
        },
    ) {
        Ok(id) => id,
        Err(_) => qemu_exit::exit_failure(FAIL_CHILD),
    };

    let mut child_woken = false;
    let mut steps = 0u64;
    while scheduler.live_task_count() != 0 && steps < MAX_STEPS {
        let _ = scheduler.step(BOOT_CPU);
        if !child_woken && PARENT_EXIT_STATUS.load(Ordering::SeqCst) != u64::MAX {
            if scheduler.unpark(child).is_err() {
                qemu_exit::exit_failure(FAIL_CHILD);
            }
            child_woken = true;
        }
        steps += 1;
    }

    if scheduler.live_task_count() != 0 {
        qemu_exit::exit_failure(FAIL_DEADLOCK);
    }
    if CLOCK_CALLS.load(Ordering::SeqCst) != 2
        || EXIT_CALLS.load(Ordering::SeqCst) != 2
        || PARENT_EXIT_STATUS.load(Ordering::SeqCst) != 0
        || !CHILD_PARK_ENTERED.load(Ordering::SeqCst)
        || !CHILD_RESUMED.load(Ordering::SeqCst)
    {
        qemu_exit::exit_failure(FAIL_RESULT);
    }

    note(
        TEST_PASS,
        "aarch64 syscall continuation test: parent resumed after child park",
    );
    qemu_exit::exit_success();
}
