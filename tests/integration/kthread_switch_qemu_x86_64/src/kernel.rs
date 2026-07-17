//! x86_64 / `target_os = "none"` body of the QEMU kthread-switch test.
//!
//! Single boot CPU, no AP bring-up, no LAPIC timer: the two kthreads
//! ping-pong purely through the cooperative `step` loop, so the only
//! mechanism under test is the real `ContextSwitch::switch`.

use core::alloc::{GlobalAlloc, Layout};
use core::fmt::Write as _;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

extern crate alloc;
use alloc::sync::Arc;

use tairix_arch_x86_64::kernel_arch::{X86_64Arch, X86_64ArchStorage};
use tairix_arch_x86_64::percpu;
use tairix_arch_x86_64::{qemu_exit, serial, smp};
use tairix_kernel_core::spawn_kthread;
use tairix_kernel_sched_eevdf::{Priority, Scheduler, SchedulerConfig};

use tairix_arch_x86_64::context_hal::ContextSwitchHal;

/// The single-CPU slice runs logical CPU 0 on the boot processor.
const BOOT_CPU: u32 = 0;

/// Times each kthread yields back to the dispatcher before exiting.
/// Large enough that a single accidental run cannot satisfy the PASS
/// check, small enough to drain well within the harness budget.
const PING_PONGS: u64 = 32;

/// Cooperative-loop watchdog: maximum `step` iterations before the test
/// declares the workload deadlocked. Sized generously for QEMU TCG; the
/// real drain is a few hundred steps.
const MAX_STEPS: u64 = 5_000_000;

/// Per-kthread run counters; index `i` counts kthread `i`'s yields.
static RUNS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

// --- Allocator -----------------------------------------------------

/// Heap size. 8 MiB comfortably fits the eevdf scheduler's registry, the
/// two `KThreadStack` boxes (16 KiB each), and the spawned closures.
const HEAP_BYTES: usize = 8 * 1024 * 1024;

#[repr(C, align(4096))]
struct Heap([u8; HEAP_BYTES]);

// SAFETY of `static mut`: this is the binary's only mutable static,
// accessed exclusively through `FreeListAllocator::alloc`. The allocator
// serialises access through `cursor: AtomicUsize`; the heap bytes are
// never aliased because each allocation hands out a disjoint slice.
static mut HEAP: Heap = Heap([0; HEAP_BYTES]);

/// Forward-only bump allocator. puts shared utilities in
/// `lib/`; this allocator is intentionally local because (a) it never
/// frees (a documented limitation for the test binary only) and (b)
/// nothing in `kernel/` or `lib/` should ever take a dependency on a
/// leak-by-design allocator.
struct BumpAllocator {
    cursor: AtomicUsize,
}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `HEAP` is exactly `HEAP_BYTES` long; the CAS loop never
        // advances the cursor past `HEAP_BYTES`, so a non-null return is
        // always an in-bounds pointer.
        let base = unsafe { core::ptr::addr_of_mut!(HEAP.0).cast::<u8>() };
        let align = layout.align();
        let size = layout.size();
        let mut cur = self.cursor.load(Ordering::Relaxed);
        loop {
            let aligned = (cur + align - 1) & !(align - 1);
            let Some(next) = aligned.checked_add(size) else {
                return core::ptr::null_mut();
            };
            if next > HEAP_BYTES {
                return core::ptr::null_mut();
            }
            match self
                .cursor
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                // SAFETY: `aligned < HEAP_BYTES` by the bound check above.
                Ok(_) => return unsafe { base.add(aligned) },
                Err(observed) => cur = observed,
            }
        }
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Deliberate no-op. The binary runs once per QEMU invocation and
        // the bump heap is reclaimed by QEMU exit.
    }
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator {
    cursor: AtomicUsize::new(0),
};

// --- BSP entry ----------------------------------------------------

/// Entry point: the multiboot-loaded kernel's boot CPU. `multiboot_info`
/// is unused — this slice brings up no APs and parses no firmware tables.
#[no_mangle]
pub extern "C" fn kernel_main(_multiboot_info: u64) -> ! {
    let mut com1 = serial::Serial::init(serial::COM1_BASE);
    let _ = writeln!(com1, "[kthread_switch_qemu_x86_64] boot");

    // Move off the boot-time GDT/IDT onto the BSP's real per-CPU tables,
    // so an unexpected fault lands on a dedicated stack rather than
    // triple-faulting silently.
    //
    // SAFETY: this is the boot CPU, called exactly once before any
    // interrupts are enabled (the boot trampoline left IF=0 and the IDTR
    // invalid); installing the per-CPU tables now is the documented
    // sequencing.
    //
    // Publish the caller-owned per-CPU GDT/IDT/IST arena before the first
    // `percpu::init`, sized to this single-CPU vertical (
    // — no baked-in `MAX_CPUS`). `register` is set-once; this `kernel_main`
    // runs once, so a function-local `static` is sound and needs no
    // allocator.
    static PER_CPU_STORAGE: percpu::PerCpuStorage<1> = percpu::PerCpuStorage::new();
    if PER_CPU_STORAGE.register().is_err() {
        let _ = writeln!(
            com1,
            "[kthread_switch_qemu_x86_64] FAIL: PerCpuStorage::register"
        );
        qemu_exit::exit_failure();
    }
    unsafe {
        if percpu::init(0).is_err() {
            let _ = writeln!(com1, "[kthread_switch_qemu_x86_64] FAIL: percpu::init(0)");
            qemu_exit::exit_failure();
        }
    }

    // Build the live scheduler over the production arch handle. Interrupts
    // stay masked, so the spawn-time self-IPI `X86_64Arch::send_ipi`
    // writes to the LAPIC ICR is simply latched and never delivered —
    // dispatch is the cooperative `step` loop below.
    let bsp_id = smp::bsp_lapic_id();
    // Single-CPU vertical (BSP, dense id 0): per-CPU bookkeeping is sized
    // to one slot (no baked-in `MAX_CPUS`).
    let cpu_to_lapic: [Option<u8>; 1] = [Some(bsp_id)];
    // The arch handle borrows its per-CPU bookkeeping from a caller-sized
    // `&'static` backing; `kernel_main` runs once, so
    // a function-local `static` is sound and needs no allocator.
    static ARCH_STORAGE: X86_64ArchStorage<1> = X86_64ArchStorage::new();
    let arch = match X86_64Arch::new(&ARCH_STORAGE, 0, bsp_id, &cpu_to_lapic) {
        Ok(handle) => Arc::new(handle),
        Err(e) => {
            let _ = writeln!(
                com1,
                "[kthread_switch_qemu_x86_64] FAIL: X86_64Arch::new: {}",
                e.as_str()
            );
            qemu_exit::exit_failure();
        }
    };
    let Ok(sched) = Scheduler::new(SchedulerConfig::defaults_for(1), arch) else {
        let _ = writeln!(com1, "[kthread_switch_qemu_x86_64] FAIL: Scheduler::new");
        qemu_exit::exit_failure();
    };

    // Spawn two kthreads. Each runs on its own kernel stack and yields
    // back to the dispatcher PING_PONGS times via the real
    // `ContextSwitch::switch`, then returns (Exit). `ContextSwitchHal` is
    // the x86_64 context-switch primitive.
    for index in 0..2usize {
        let spawned = spawn_kthread(
            &sched,
            ContextSwitchHal::new(),
            BOOT_CPU,
            Priority::Normal,
            move |yielder| {
                for _ in 0..PING_PONGS {
                    RUNS[index].fetch_add(1, Ordering::SeqCst);
                    yielder.yield_now();
                }
            },
        );
        if spawned.is_err() {
            let _ = writeln!(com1, "[kthread_switch_qemu_x86_64] FAIL: spawn_kthread");
            qemu_exit::exit_failure();
        }
    }
    let _ = writeln!(com1, "[kthread_switch_qemu_x86_64] two kthreads spawned");

    // Cooperative dispatch loop: drive `step` until both kthreads have
    // exited. Each `step` enters a task, which yields straight back, so
    // the two tasks ping-pong through the real context switch. A switch
    // that never resumed its task would stall the drain and the harness
    // would time out (fail-loud).
    let mut steps = 0u64;
    while sched.live_task_count() != 0 && steps < MAX_STEPS {
        let _ = sched.step(BOOT_CPU);
        steps += 1;
    }
    if sched.live_task_count() != 0 {
        let _ = writeln!(
            com1,
            "[kthread_switch_qemu_x86_64] FAIL: deadlock — tasks remained after {steps} steps"
        );
        qemu_exit::exit_failure();
    }
    let (a, b) = (
        RUNS[0].load(Ordering::SeqCst),
        RUNS[1].load(Ordering::SeqCst),
    );
    if a != PING_PONGS || b != PING_PONGS {
        let _ = writeln!(
            com1,
            "[kthread_switch_qemu_x86_64] FAIL: run counts {a},{b} != {PING_PONGS}"
        );
        qemu_exit::exit_failure();
    }

    let _ = writeln!(
        com1,
        "[kthread_switch_qemu_x86_64] PASS: two kthreads ping-ponged via the real context switch"
    );
    qemu_exit::exit_success();
}

// --- Panic handler ------------------------------------------------

#[panic_handler]
fn tairix_kthread_switch_x86_64_panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let mut com1 = serial::Serial::init(serial::COM1_BASE);
    let _ = writeln!(com1, "[kthread_switch_qemu_x86_64] panic: {info}");
    qemu_exit::exit_failure();
}
