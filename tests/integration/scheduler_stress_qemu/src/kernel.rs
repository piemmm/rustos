//! x86_64 / `target_os = "none"` body of the QEMU scheduler-stress test.

use core::alloc::{GlobalAlloc, Layout};
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};

extern crate alloc;
use alloc::sync::Arc;

use rustos_arch_x86_64::acpi::{self, MadtEntry};
use rustos_arch_x86_64::apic::{Lapic, VolatileLapicMmio};
use rustos_arch_x86_64::apic_timer::{PolledPit, PortIo};
use rustos_arch_x86_64::multiboot2::BootInfo;
use rustos_arch_x86_64::percpu;
use rustos_arch_x86_64::smp::{
    self, init_sipi_sipi, ApBootSlot, Delay, TrampolineFrame, AP_BOOT_SLOT_OFFSET,
    AP_TRAMPOLINE_PHYS,
};
use rustos_arch_x86_64::{qemu_exit, serial};
use rustos_kernel_sched::{Priority, Scheduler, SchedulerArch, SchedulerConfig, TaskAction};

// --- Workload sizing -----------------------------------------------

/// Tasks per CPU. The QEMU runner allocates 256 MiB of guest RAM
/// (`tools/qemu::Spec::for_x86_64_kernel` default) and the bump heap is
/// 64 MiB; 2048 tasks per CPU × 4 CPUs = 8192 tasks fits comfortably
/// with headroom for the BTreeMap / Arc bookkeeping the scheduler does
/// internally. The host-side `scheduler_stress` test still drives the
/// 20 000-task figure mandated by Stage-2; this binary's purpose is to
/// prove the *real-cores* path, not to repeat the host scale.
const TASKS_PER_CPU: u32 = 2_048;

/// Cooperative-loop safety cap: maximum number of `step()` iterations a
/// CPU performs before the BSP declares the test deadlocked. Sized
/// generously (one iteration ≈ a dozen instructions on QEMU TCG; this
/// budget tolerates a 10× slowdown).
const MAX_STEPS_PER_CPU: u64 = 1_000_000;

// --- Cross-CPU coordination ----------------------------------------

/// `Arc<Scheduler<SmpArch>>` raw pointer the BSP publishes once the
/// scheduler is constructed. APs `Acquire`-load it before entering the
/// step loop. The BSP never replaces it, so a single atomic suffices.
static SCHED_PTR: AtomicPtr<Scheduler<SmpArch>> = AtomicPtr::new(core::ptr::null_mut());

/// Set to `true` by the BSP once the workload completes (or fails).
/// APs observe it in their step loop and break out.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Per-CPU execution counter (debug aid; verifies tasks ran on multiple
/// physical cores). Sized to the maximum supported CPU count.
///
/// Must not exceed `rustos_arch_x86_64::percpu::MAX_CPUS` — the arch
/// crate's per-CPU arena is the source of truth for the bound. The
/// const-assert immediately below makes a future divergence a
/// compile-time error.
const MAX_CPUS: usize = 16;

#[allow(dead_code)] // const-assert; not referenced at runtime.
const MAX_CPUS_FITS_PERCPU_ARENA: () = assert!(MAX_CPUS <= rustos_arch_x86_64::percpu::MAX_CPUS);
static PER_CPU_EXEC: [AtomicU64; MAX_CPUS] = {
    // `AtomicU64::new` is `const`. Hand-roll the array; the
    // `[AtomicU64::new(0); MAX_CPUS]` syntax requires `Copy`.
    const Z: AtomicU64 = AtomicU64::new(0);
    [Z; MAX_CPUS]
};

/// Number of APs that have entered the step loop. The BSP waits on this
/// for an acquire-fence before spawning tasks so that the spawn-time
/// `send_ipi` (which is a no-op in this binary) does not race a CPU
/// that has not yet observed the published scheduler.
static APS_LIVE: AtomicU32 = AtomicU32::new(0);

/// Total expected execution count, written by the BSP before spawning.
static EXPECTED_EXECS: AtomicU64 = AtomicU64::new(0);

/// Total tasks actually executed (incremented from every task body).
static EXECUTIONS: AtomicU64 = AtomicU64::new(0);

// --- Allocator -----------------------------------------------------

/// Heap size. 64 MiB comfortably fits 8 192 Arc<TaskInner>s, the
/// scheduler's `BTreeMap<TaskId, ...>` registry, the per-priority
/// `RunDeque` slots, and the closures we spawn.
const HEAP_BYTES: usize = 64 * 1024 * 1024;

#[repr(C, align(4096))]
struct Heap([u8; HEAP_BYTES]);

// SAFETY of `static mut`: this is the kernel binary's *only* mutable
// static, accessed exclusively through `BumpAllocator::alloc`. The
// allocator serialises access through `cursor: AtomicUsize`; the heap
// bytes themselves are never aliased because each allocation hands out
// a disjoint slice from the bump cursor.
static mut HEAP: Heap = Heap([0; HEAP_BYTES]);

/// Forward-only bump allocator. AGENTS.md §6 puts shared utilities in
/// `lib/`; this allocator is intentionally local because (a) it never
/// frees (a documented limitation for the test binary only) and (b)
/// nothing in `kernel/` or `lib/` should ever take a dependency on a
/// leak-by-design allocator.
struct BumpAllocator {
    cursor: AtomicUsize,
}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `HEAP` is exactly `HEAP_BYTES` long; the loop below
        // performs CAS-driven bump allocation and never advances past
        // `HEAP_BYTES`, so the returned pointer is in-bounds whenever
        // the function returns non-null.
        let base = unsafe { core::ptr::addr_of_mut!(HEAP.0) as *mut u8 };
        let align = layout.align();
        let size = layout.size();
        let mut cur = self.cursor.load(Ordering::Relaxed);
        loop {
            let aligned = (cur + align - 1) & !(align - 1);
            let next = match aligned.checked_add(size) {
                Some(n) => n,
                None => return core::ptr::null_mut(),
            };
            if next > HEAP_BYTES {
                return core::ptr::null_mut();
            }
            match self
                .cursor
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    // SAFETY: `aligned < HEAP_BYTES` by the bound check
                    // above, so `base.add(aligned)` is in-bounds.
                    return unsafe { base.add(aligned) };
                }
                Err(observed) => cur = observed,
            }
        }
    }
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Deliberate no-op. The binary runs once per QEMU invocation
        // and the bump heap is reclaimed by QEMU exit.
    }
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator {
    cursor: AtomicUsize::new(0),
};

// --- SchedulerArch impl -------------------------------------------

/// Architecture implementation for the QEMU kernel. `current_cpu`
/// reads the LAPIC ID register directly; `send_ipi` is a no-op-with-
/// counter because Stage 3a (b) does not arm preemption (cores are in
/// a tight `step()` loop and observe spawned tasks on their next
/// iteration; the host-side test on `TestArch` exhibits the same
/// behaviour).
struct SmpArch;

impl SchedulerArch for SmpArch {
    fn current_cpu(&self) -> u32 {
        u32::from(smp::bsp_lapic_id())
    }
    fn ticks_now(&self) -> u64 {
        // RDTSC has been available since the Pentium and is universally
        // emulated by QEMU. We treat it as a monotonic-enough clock.
        // SAFETY: RDTSC is unprivileged and has no side effects.
        unsafe {
            let lo: u32;
            let hi: u32;
            core::arch::asm!(
                "rdtsc",
                out("eax") lo,
                out("edx") hi,
                options(nomem, nostack, preserves_flags),
            );
            (u64::from(hi) << 32) | u64::from(lo)
        }
    }
    fn send_ipi(&self, _target: u32) {
        // No preemption in (b); the receiver is already polling
        // `step()`. Stage 3a (c) replaces this with a real IPI.
    }
}

// --- PIT-based delay ----------------------------------------------

/// `Delay` implementation backed by busy-waiting on PIT channel 2 OUT.
struct PitDelay {
    pit: PolledPit,
}
impl Delay for PitDelay {
    fn delay_us(&mut self, us: u32) {
        // PIT runs at 1.193182 MHz. ticks = us * 1_193_182 / 1_000_000
        // ≈ us * 1.193. For `us` ≤ 54_925 the value fits in 16 bits; the
        // bring-up uses 10 000 and 200, both safely below.
        let reload = ((u64::from(us) * 1_193_182) / 1_000_000) as u16;
        if reload == 0 {
            return;
        }
        // Arm channel 2 one-shot, gate it, then poll the OUT bit.
        let gate = self.pit.inb(0x61);
        self.pit.outb(0x61, (gate & 0xFC) | 0x01);
        self.pit.outb(0x43, 0xB0);
        self.pit.outb(0x42, (reload & 0xFF) as u8);
        self.pit.outb(0x42, (reload >> 8) as u8);
        while self.pit.inb(0x61) & 0x20 == 0 {}
    }
}

// --- BSP entry ----------------------------------------------------

/// Entry point: BSP only.
#[no_mangle]
pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
    let mut com1 = serial::Serial::init(serial::COM1_BASE);
    let _ = writeln!(com1, "[scheduler_stress_qemu] BSP boot");

    // Stage 3a (c1/c2): replace the boot-time GDT (`boot.s`) and the
    // not-yet-installed IDT with the BSP's per-CPU GDT + IDT. After
    // this call the kernel is on its real long-mode descriptor tables
    // for cpu_index 0; `#DF` and `#NMI` land on dedicated IST stacks.
    //
    // SAFETY: this is the BSP, called exactly once before any
    // interrupts are enabled. The boot trampoline (`boot.s` SAFETY-
    // INVARIANT 6) guarantees the IDTR is invalid on entry; replacing
    // it now is the documented sequencing.
    unsafe {
        if percpu::init(0).is_err() {
            let _ = writeln!(com1, "[scheduler_stress_qemu] FAIL: percpu::init(BSP)");
            qemu_exit::exit_failure();
        }
    }

    // Software-enable the BSP's LAPIC so we can drive IPIs.
    let mut lapic = make_lapic();
    lapic.software_enable(0xFF);
    let bsp_id = smp::bsp_lapic_id();
    let _ = writeln!(com1, "[scheduler_stress_qemu] BSP LAPIC id = {bsp_id}");

    // Discover APs.
    let ap_ids = match discover_aps(multiboot_info, bsp_id, &mut com1) {
        Some(ids) => ids,
        None => {
            let _ = writeln!(com1, "[scheduler_stress_qemu] FAIL: MADT discovery");
            qemu_exit::exit_failure();
        }
    };
    let cpu_count = (ap_ids.count + 1) as u32;
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] discovered {cpu_count} CPUs (BSP + {} APs)",
        ap_ids.count
    );
    if cpu_count < 4 {
        let _ = writeln!(
            com1,
            "[scheduler_stress_qemu] FAIL: Stage-2 mandates >= 4 cores, got {cpu_count}"
        );
        qemu_exit::exit_failure();
    }

    // Build the scheduler and publish it.
    let cfg = SchedulerConfig {
        cpus: cpu_count,
        queue_capacity_per_band: 8192,
        yields_before_demotion: 4,
        boost_interval_ticks: 256,
    };
    let arch = Arc::new(SmpArch);
    let sched = Arc::new(Scheduler::new(cfg, arch).expect("scheduler"));
    // Leak one Arc into the global pointer so APs can clone it.
    let raw = Arc::into_raw(sched.clone()) as *mut Scheduler<SmpArch>;
    SCHED_PTR.store(raw, Ordering::Release);

    // Bring the APs up one at a time.
    let mut pit_delay = PitDelay { pit: PolledPit };
    for i in 0..ap_ids.count {
        let target = ap_ids.ids[i];
        let cpu_id = (i + 1) as u32; // BSP = 0, APs = 1..N
        bring_up_ap(target, cpu_id, &mut lapic, &mut pit_delay, &mut com1);
    }

    // Wait for every AP to reach its step loop.
    while APS_LIVE.load(Ordering::Acquire) < ap_ids.count as u32 {
        core::hint::spin_loop();
    }
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] all {} APs live, spawning tasks",
        ap_ids.count
    );

    // Spawn the workload.
    let total_tasks = u64::from(TASKS_PER_CPU) * u64::from(cpu_count);
    EXPECTED_EXECS.store(total_tasks, Ordering::Release);
    for i in 0..total_tasks {
        let home = (i % u64::from(cpu_count)) as u32;
        sched
            .spawn(home, Priority::Normal, move |_ctx| {
                // Each CPU's execution counter is incremented by the
                // *executing* CPU, not the home CPU — that's what proves
                // multi-core dispatch happened.
                let me = smp::bsp_lapic_id() as usize;
                if me < MAX_CPUS {
                    PER_CPU_EXEC[me].fetch_add(1, Ordering::Relaxed);
                }
                EXECUTIONS.fetch_add(1, Ordering::Relaxed);
                TaskAction::Exit
            })
            .expect("spawn");
    }
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] spawned {total_tasks} tasks; entering step loop"
    );

    // BSP step loop alongside the APs.
    run_step_loop(0, &sched);

    // BSP fell out of the loop: live_task_count == 0 OR step budget
    // exhausted. Signal APs and verify.
    SHUTDOWN.store(true, Ordering::Release);

    let live = sched.live_task_count();
    let execs = EXECUTIONS.load(Ordering::Acquire);
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] step loop end: live={live} execs={execs} expected={total_tasks}"
    );

    if live != 0 {
        let _ = writeln!(
            com1,
            "[scheduler_stress_qemu] FAIL: deadlock — {live} tasks remained"
        );
        qemu_exit::exit_failure();
    }
    if execs != total_tasks {
        let _ = writeln!(
            com1,
            "[scheduler_stress_qemu] FAIL: exec count {execs} != expected {total_tasks}"
        );
        qemu_exit::exit_failure();
    }

    // Sanity: ≥ 2 distinct CPUs must have executed work. (Stronger than
    // "≥ 4" because cooperative dispatch can starve a CPU briefly while
    // its queue is empty; the host test asserts bounded first-run
    // latency which is a (c) concern. The point of *this* test is to
    // prove the AP bring-up path put multiple real cores to work.)
    let mut distinct_cpus = 0u32;
    for slot in PER_CPU_EXEC.iter() {
        if slot.load(Ordering::Relaxed) > 0 {
            distinct_cpus += 1;
        }
    }
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] distinct executing CPUs = {distinct_cpus}"
    );
    if distinct_cpus < 2 {
        let _ = writeln!(
            com1,
            "[scheduler_stress_qemu] FAIL: only {distinct_cpus} CPU(s) ran tasks; AP bring-up did not actually dispatch"
        );
        qemu_exit::exit_failure();
    }

    let _ = writeln!(com1, "[scheduler_stress_qemu] PASS");
    qemu_exit::exit_success();
}

// --- AP entry -----------------------------------------------------

extern "C" fn ap_entry(cpu_id: u32) -> ! {
    // Stage 3a (c1/c2): install this AP's per-CPU GDT + IDT before
    // touching any further shared state. The AP trampoline left us on
    // the trampoline-internal GDT (`ap_trampoline.s` SAFETY-INVARIANT 5)
    // with the IDTR still invalid (SAFETY-INVARIANT 8); `percpu::init`
    // moves us to the real per-CPU tables.
    //
    // SAFETY: called exactly once per AP, on that AP, before
    // interrupts are enabled.
    unsafe {
        if percpu::init(cpu_id as usize).is_err() {
            // We cannot signal the BSP cleanly from here (the BSP is
            // spinning on `ready` in the trampoline frame, which has
            // already been set). Halt the AP; the BSP's watchdog
            // budget will eventually catch the missing dispatch.
            loop {
                core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
            }
        }
    }

    // Wait for the BSP to publish the scheduler pointer.
    while SCHED_PTR.load(Ordering::Acquire).is_null() {
        core::hint::spin_loop();
    }
    let raw = SCHED_PTR.load(Ordering::Acquire);
    // SAFETY: the BSP stored a `Arc::into_raw`-cloned pointer; this AP
    // increments the strong count and treats it as a borrowed `Arc` for
    // the lifetime of the step loop. We do *not* call `Arc::from_raw`
    // here (that would consume the strong count); instead we reach into
    // the scheduler by reference.
    let sched: &Scheduler<SmpArch> = unsafe { &*raw };

    APS_LIVE.fetch_add(1, Ordering::AcqRel);

    run_step_loop(cpu_id, sched);

    // The BSP set SHUTDOWN; halt with interrupts masked.
    loop {
        // SAFETY: hlt with IF=0 is a privileged halt and is well-defined.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

// --- Step loop driver ----------------------------------------------

fn run_step_loop(cpu_id: u32, sched: &Scheduler<SmpArch>) {
    let mut steps: u64 = 0;
    while !SHUTDOWN.load(Ordering::Acquire) && steps < MAX_STEPS_PER_CPU {
        // Step the scheduler. Errors here would indicate an internal
        // invariant break; treat as failure.
        let _ = sched.step(cpu_id);
        steps += 1;
        // BSP-side termination: stop once every task has exited.
        if cpu_id == 0
            && sched.live_task_count() == 0
            && EXECUTIONS.load(Ordering::Acquire) >= EXPECTED_EXECS.load(Ordering::Acquire)
        {
            return;
        }
    }
}

// --- LAPIC helper --------------------------------------------------

fn make_lapic() -> Lapic<VolatileLapicMmio> {
    // SAFETY: LAPIC MMIO base is 0xFEE00000 on every Intel-architecture
    // system QEMU emulates. The frame is identity-mapped by `boot.s`
    // (SAFETY-INVARIANT 4 — 0..4 GiB identity map).
    let mmio = unsafe { VolatileLapicMmio::new(0xFEE0_0000 as *mut u32) };
    Lapic::new(mmio)
}

// --- MADT discovery -----------------------------------------------

struct ApList {
    ids: [u8; smp::AP_TRAMPOLINE_LEN], // overprovisioned upper bound
    count: usize,
}

fn discover_aps(multiboot_info: u64, bsp_id: u8, com1: &mut serial::Serial) -> Option<ApList> {
    // SAFETY: `multiboot_info` is the verbatim pointer from `boot.s`
    // SAFETY-INVARIANT 7. The block is in the identity-mapped 0..4 GiB
    // window so dereferences are sound; we only inspect the first
    // 4 bytes (total_size) before bounding the rest of the slice.
    let header = unsafe { core::slice::from_raw_parts(multiboot_info as *const u8, 8) };
    let total_size = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] mb2 total_size = {total_size}"
    );
    // SAFETY: same justification as above; we now know the actual length.
    let mb = unsafe { core::slice::from_raw_parts(multiboot_info as *const u8, total_size) };
    let info = BootInfo::parse(mb).ok()?;

    let rsdp_bytes = info.rsdp()?;
    let rsdp = acpi::Rsdp::validate(rsdp_bytes).ok()?;
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] RSDP rev {} rsdt={:#x} xsdt={:#x}",
        rsdp.revision, rsdp.rsdt_address, rsdp.xsdt_address
    );

    // Locate the MADT via XSDT (preferred) or RSDT.
    let madt_bytes = if rsdp.xsdt_address != 0 {
        find_madt_via_xsdt(rsdp.xsdt_address)?
    } else {
        find_madt_via_rsdt(u64::from(rsdp.rsdt_address))?
    };
    let madt = acpi::Madt::parse(madt_bytes).ok()?;

    let mut list = ApList {
        ids: [0; smp::AP_TRAMPOLINE_LEN],
        count: 0,
    };
    for entry in madt.entries() {
        if let MadtEntry::LocalApic { apic_id, flags, .. } = entry {
            // ACPI 6.5 Table 5.40: bit 0 = "Processor Enabled".
            if flags & 1 == 0 {
                continue;
            }
            if apic_id == bsp_id {
                continue;
            }
            if list.count < list.ids.len() {
                list.ids[list.count] = apic_id;
                list.count += 1;
            }
        }
    }
    Some(list)
}

/// Read a 4-byte little-endian u32 from a physical address. SAFETY: the
/// address must lie within the boot-time identity-mapped 0..4 GiB.
unsafe fn read_u32(phys: u64) -> u32 {
    // SAFETY: caller's contract.
    unsafe { core::ptr::read_unaligned(phys as *const u32) }
}
unsafe fn read_u64(phys: u64) -> u64 {
    // SAFETY: caller's contract.
    unsafe { core::ptr::read_unaligned(phys as *const u64) }
}

fn find_madt_via_xsdt(xsdt_phys: u64) -> Option<&'static [u8]> {
    // SAFETY: xsdt_phys < 4 GiB (firmware-reserved tables placed by OVMF).
    let header_len = 36usize;
    let len = unsafe { read_u32(xsdt_phys + 4) } as usize;
    if len < header_len {
        return None;
    }
    let n_entries = (len - header_len) / 8;
    for i in 0..n_entries {
        let entry = unsafe { read_u64(xsdt_phys + header_len as u64 + (i as u64) * 8) };
        if let Some(bytes) = try_madt(entry) {
            return Some(bytes);
        }
    }
    None
}

fn find_madt_via_rsdt(rsdt_phys: u64) -> Option<&'static [u8]> {
    let header_len = 36usize;
    let len = unsafe { read_u32(rsdt_phys + 4) } as usize;
    if len < header_len {
        return None;
    }
    let n_entries = (len - header_len) / 4;
    for i in 0..n_entries {
        let entry = unsafe { read_u32(rsdt_phys + header_len as u64 + (i as u64) * 4) } as u64;
        if let Some(bytes) = try_madt(entry) {
            return Some(bytes);
        }
    }
    None
}

fn try_madt(phys: u64) -> Option<&'static [u8]> {
    // SAFETY: identity-mapped 0..4 GiB.
    let sig = unsafe { core::slice::from_raw_parts(phys as *const u8, 4) };
    if sig != b"APIC" {
        return None;
    }
    let len = unsafe { read_u32(phys + 4) } as usize;
    // SAFETY: same as above; length validated by `Madt::parse`.
    let bytes = unsafe { core::slice::from_raw_parts(phys as *const u8, len) };
    Some(bytes)
}

// --- Per-AP stack pool --------------------------------------------

/// Per-AP stack. `0x4000` (16 KiB) matches the BSP bootstrap stack size
/// in `boot.s`. Aligned to 16 bytes per the System V AMD64 ABI.
#[repr(C, align(16))]
struct ApStack([u8; 16 * 1024]);

const MAX_APS: usize = MAX_CPUS - 1;
static mut AP_STACKS: [ApStack; MAX_APS] = {
    const Z: ApStack = ApStack([0; 16 * 1024]);
    [Z; MAX_APS]
};

fn ap_stack_top(idx: usize) -> u64 {
    // SAFETY: idx < MAX_APS; we return the *top* (one past the last
    // byte) which is what the System V ABI wants RSP to be initialised
    // to. The 16-byte alignment is preserved because the struct is
    // `align(16)` and the array of `align(16)` structs is also
    // `align(16)`; adding `size_of::<ApStack>()` keeps that alignment.
    unsafe {
        let base = core::ptr::addr_of!(AP_STACKS[idx]) as u64;
        base + core::mem::size_of::<ApStack>() as u64
    }
}

// --- Trampoline-frame access --------------------------------------

/// Wrap the 4 KiB low frame at `AP_TRAMPOLINE_PHYS` in a typed view.
fn trampoline_frame_mut() -> &'static mut [u8] {
    // SAFETY: identity-mapped, no other CPU is reading or writing this
    // page (the BSP serialises AP launches via the `ready` flag), and
    // the page is reserved by the test binary (no other allocator
    // hands it out — the BSP uses a static heap that lives well above
    // 0x8000).
    unsafe { core::slice::from_raw_parts_mut(AP_TRAMPOLINE_PHYS as *mut u8, 4096) }
}

// --- Per-AP launcher ----------------------------------------------

fn bring_up_ap(
    target: u8,
    cpu_id: u32,
    lapic: &mut Lapic<VolatileLapicMmio>,
    delay: &mut PitDelay,
    com1: &mut serial::Serial,
) {
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] bringing up APIC id {target} as cpu_id {cpu_id}"
    );

    let mut frame = TrampolineFrame::new(trampoline_frame_mut()).expect("frame");
    frame.install(smp::trampoline_payload()).expect("install");
    let stack_top = ap_stack_top((cpu_id - 1) as usize);
    let entry_addr = ap_entry as *const () as u64;
    // Read CR3 — the BSP's PML4. APs inherit it.
    let cr3: u64;
    // SAFETY: reading CR3 in ring 0 is well-defined.
    unsafe {
        core::arch::asm!("mov {x}, cr3", x = out(reg) cr3, options(nostack, preserves_flags));
    }
    let slot = ApBootSlot::new(cr3, stack_top, entry_addr, cpu_id).expect("slot");
    frame.write_slot(&slot);

    // Memory fence: every slot byte must be visible before SIPI.
    core::sync::atomic::fence(Ordering::Release);

    init_sipi_sipi(lapic, delay, target, smp::sipi_vector());

    // Wait for the AP's `xchg`-released ready flag, with a generous
    // budget. PIT delay isn't necessary here — we just spin on the
    // memory location.
    let mut spins: u64 = 0;
    while frame.load_ready() == 0 {
        spins += 1;
        if spins > 10_000_000 {
            let _ = writeln!(
                com1,
                "[scheduler_stress_qemu] FAIL: AP {target} (cpu_id {cpu_id}) never set ready"
            );
            qemu_exit::exit_failure();
        }
        core::hint::spin_loop();
    }
    let _ = writeln!(
        com1,
        "[scheduler_stress_qemu] AP {target} live after {spins} spins"
    );
    // The slot is reused for the next AP; the AP has already copied
    // everything it needs into its own registers/stack.
    let _ = AP_BOOT_SLOT_OFFSET; // silence unused-import lint if any
}

// --- Panic handler ------------------------------------------------

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let mut com1 = serial::Serial::init(serial::COM1_BASE);
    let _ = writeln!(com1, "[scheduler_stress_qemu] panic: {info}");
    qemu_exit::exit_failure();
}
