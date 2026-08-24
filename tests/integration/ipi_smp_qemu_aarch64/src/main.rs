//! WIRING Stage W6 QEMU integration test: multi-core SMP bring-up and
//! IPI delivery on the aarch64 `virt` board.
//!
//! ## What this test asserts
//!
//! `plans/WIRING.md` Stage W6 requires that the aarch64 boot core can
//! (1) start every secondary core, (2) deliver its local generic-timer PPI,
//! and (3) deliver a directed inter-processor interrupt to each — the
//! EL1/GICv2 analogue of the riscv64 vertical. This binary exercises all
//! three, end to end, on a four-core `virt` board:
//!
//! 1. The boot core (core 0) installs the shared IPI callback
//!    (`preempt::set_ipi_callback`) and the secondary-core entry
//!    (`smp::set_secondary_entry`), and brings up its own GICv2
//!    distributor (`gic::init`).
//! 2. It starts cores 1–3 via the `SecondaryBringup` PSCI `CPU_ON` path;
//!    each runs the `smp.s` trampoline → the installed entry.
//! 3. Every secondary installs the EL1 vector table (`exceptions::init_vectors`),
//!    brings up its GICv2 CPU interface (`gic::init`), enables the timer PPI
//!    and IPI SGI, arms one local timer quantum, unmasks IRQs, publishes its
//!    `READY` bit, then idles on `wfi`.
//! 4. The boot core requires one timer callback from every secondary, then
//!    sends one IPI to each through `Aarch64Arch::send_ipi`.
//! 5. Each SGI target takes the IRQ and runs `preempt::on_ipi_interrupt` → the
//!    IPI callback. The boot core verifies all three callback CPU ids, then
//!    writes the ARM semihosting PASS finisher.
//!
//! A regression that fails to start a secondary core or to deliver a timer
//! PPI or IPI never reaches the PASS write, so the run times out and the
//! harness reports `Outcome::Timeout` — the documented fail-loud
//! behaviour.
//!
//! ## PSCI conduit (PI Stage P5)
//!
//! `smp::start_secondary` takes the PSCI conduit (`hvc`/`smc`) as a
//! parameter. This vertical proves the conduit is **discovered**, not
//! assumed: before building the arch handle it reads `/psci` `method`
//! from the canonical `virt` device tree embedded at build time
//! (`fdt::psci_method`) and fails closed if no PSCI node is found, then
//! installs *that discovered* conduit on the handle. The secondary core
//! each secondary this test starts is therefore brought up over the conduit read from
//! the tree, mirroring how the production `boot_aarch64` path installs
//! it (`plans/PI.md` P5). The board tree is embedded, not read from
//! `x0`, for the same reason as the GIC bases below: QEMU's ELF
//! `-kernel` boot hands no DTB pointer.
//!
//! ## GICv2 base discovery (PI Stage P3)
//!
//! Before `gic::init`, the boot core **poisons** the runtime GICv2 base
//! and then reads the GICD/GICC bases from the canonical `virt` device
//! tree embedded at build time (`gic::configure_from_fdt`), asserting the
//! base moved off the poison value to the `virt` GICv2 distributor base.
//! Every subsequent GIC access on all cores — `gic::init`, the timer PPIs,
//! directed SGIs, and each secondary CPU interface — targets that
//! discovered base, so the IPI this test delivers is the runtime proof
//! the discovered base works (`plans/PI.md` P3). The board tree is
//! embedded, not read from `x0`, for the same reason as the PSCI conduit
//! above: QEMU's ELF `-kernel` boot hands no DTB pointer.
//!
//! ## How it differs from a production kernel
//!
//! It links only the `tairix-arch-aarch64` port (the SMP path needs no
//! `kernel/*` subsystem) and supplies its own `kernel_main`. The
//! QEMU-exit shortcut lives in this dedicated bin, never behind a Cargo
//! feature on the arch crate (fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::num::NonZeroU16;
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

    use tairix_arch_aarch64::kernel_arch::{read_cntfrq, SecondaryStart};
    use tairix_arch_aarch64::{
        enable_fp_el1, exceptions, fdt, gic, handle_panic_via_serial, preempt, qemu_exit, smp,
        Aarch64Arch, Aarch64ArchStorage, SERIAL_SINK,
    };
    use tairix_arch_api::{CpuId, SchedulerArch, SecondaryBringup};
    use tairix_fdt::Fdt;
    use tairix_itest_finisher::fail_point;
    use tairix_log::{log, Event, EventId, Level};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`): the GICv2 bases are read from it (P3), proving
    // the IPI is delivered over a *discovered* base, not a constant.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// `u32` sentinel for "no IPI callback has fired yet".
    const NO_CPU: u32 = u32::MAX;

    /// Dense id of the boot core. QEMU always enters the `virt` board on
    /// the primary core, and the board assigns it affinity 0.
    const BOOT_CPU: CpuId = 0;

    /// Dense id of the first secondary core this test starts.
    const SECONDARY_CPU: CpuId = 1;

    /// Number of CPUs in the Raspberry Pi 4-shaped QEMU topology.
    const CPU_COUNT: CpuId = 4;

    /// Local generic-timer frequency used to prove each secondary's PPI.
    const TICK_HZ: u64 = 100;

    /// The PSCI conduit the QEMU `virt` board declares (no EL3 → `hvc`).
    /// This is the *expected* result of discovery, asserted against the
    /// conduit `fdt::psci_method` reads from the embedded tree — the
    /// vertical drives bring-up over the discovered value, not this
    /// constant (`plans/PI.md` P5).
    const VIRT_EXPECTED_PSCI_METHOD: fdt::PsciMethod = fdt::PsciMethod::Hvc;

    /// Stable audit-event ids for the QEMU transcript.
    const SMP_TEST_START: EventId = EventId(4230);
    const SMP_SECONDARY_UP: EventId = EventId(4231);
    const SMP_TEST_PASS: EventId = EventId(4232);
    const SMP_TEST_FAIL: EventId = EventId(4233);

    /// Failure finisher code: the secondary core never came up.
    const FAIL_SECONDARY_START: NonZeroU16 = fail_point!(1);
    /// Failure finisher code: the IPI fired on the wrong core.
    const FAIL_WRONG_CPU: NonZeroU16 = fail_point!(2);
    /// Failure finisher code: `CNTFRQ_EL0` reported a zero frequency.
    const FAIL_ZERO_FREQ: NonZeroU16 = fail_point!(3);
    /// Failure finisher code: the GICv2 bases were not discovered from the
    /// embedded `virt` device tree (P3).
    const FAIL_GIC_NOT_DISCOVERED: NonZeroU16 = fail_point!(4);
    /// Failure finisher code: the PSCI conduit was not discovered from the
    /// embedded `virt` device tree, or did not match the board's `hvc`
    /// (P5).
    const FAIL_PSCI_NOT_DISCOVERED: NonZeroU16 = fail_point!(5);

    /// A deliberately-wrong GICv2 distributor/CPU-interface base installed
    /// before discovery runs. It is **not** the `virt` GICv2 base, so a
    /// later successful IPI delivery can only mean discovery overwrote it
    /// with the base read from the device tree.
    const POISON_GIC_BASE: usize = 0xdead_0000;

    /// Bit `cpu` is set by each secondary once its vector table, GICv2
    /// interface, and IPI SGI enable are in place.
    static SECONDARY_READY: AtomicU32 = AtomicU32::new(0);

    /// Bit `cpu` is set after that secondary services its local timer PPI.
    static TIMER_FIRED: AtomicU32 = AtomicU32::new(0);

    /// Counter ticks in one test quantum, published before secondaries start.
    static TIMER_INTERVAL: AtomicU64 = AtomicU64::new(0);

    /// Count of IPI callbacks serviced (incremented on the core that
    /// takes the SGI IRQ).
    static IPI_COUNT: AtomicU32 = AtomicU32::new(0);

    /// The dense id of the core the most recent IPI callback fired on;
    /// `NO_CPU` until one fires.
    static IPI_CPU: AtomicU32 = AtomicU32::new(NO_CPU);

    /// The IPI callback the SGI IRQ path invokes. A real scheduler would
    /// request a reschedule on `cpu`; the test only needs to prove the
    /// IPI was delivered and dispatched on the right core.
    extern "C" fn on_ipi(cpu: CpuId) {
        IPI_CPU.store(cpu, Ordering::SeqCst);
        IPI_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    /// Record one local generic-timer interrupt on `cpu`.
    extern "C" fn on_tick(cpu: CpuId) {
        TIMER_FIRED.fetch_or(1u32 << cpu, Ordering::SeqCst);
    }

    /// Entry the secondary core runs (via the `smp.s` trampoline) once
    /// the boot core starts it. Brings up its interrupt path, signals
    /// ready, and idles waiting for the IPI.
    extern "C" fn secondary_entry(cpu: CpuId) -> ! {
        if smp::current_cpu_index() != cpu {
            qemu_exit::exit_failure(FAIL_WRONG_CPU);
        }
        // SAFETY: this is the secondary core's first action; it has a
        // private stack (smp.s) and no source is armed on it yet. The
        // shared IPI callback was installed by the boot core before it
        // started this core. The vector table and GIC CPU interface are
        // per-CPU, so each must be installed on the core that uses them.
        unsafe {
            enable_fp_el1();
            exceptions::init_vectors();
            gic::init();
            preempt::enable_ipi();
            preempt::init_local_preempt(cpu, TIMER_INTERVAL.load(Ordering::Acquire));
            preempt::arm_oneshot(TIMER_INTERVAL.load(Ordering::Acquire));
            exceptions::enable_irq();
        }
        // Publish readiness only after interrupts are enabled, so the
        // boot core cannot send the IPI before this core can take it.
        SECONDARY_READY.fetch_or(1u32 << cpu, Ordering::SeqCst);

        loop {
            // SAFETY: `wfi` is a wait-for-interrupt hint with no
            // architectural side effects; the delivered IPI wakes it.
            unsafe {
                core::arch::asm!("wfi", options(nomem, nostack, preserves_flags));
            }
        }
    }

    /// Forward to the shared aarch64 panic bridge (parks the core; the
    /// run then times out and the harness reports the failure).
    #[panic_handler]
    fn tairix_ipi_smp_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s`
    /// trampoline calls (via `tairix_arch_aarch64_main`).
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: SMP_TEST_START,
                message: "aarch64 IPI/SMP test: starting secondary core",
                fields: &[],
            },
        );

        // The counter frequency feeds the arch handle's monotonic clock;
        // fail closed if the timer reports zero rather than dividing by it.
        let counter_hz = read_cntfrq();
        if counter_hz == 0 {
            qemu_exit::exit_failure(FAIL_ZERO_FREQ);
        }

        // P5: discover the PSCI conduit from the embedded `virt` device
        // tree rather than naming it. Fail closed if the tree declares no
        // PSCI node, or declares one other than the board's `hvc`, so the
        // secondary is brought up only over a *discovered* conduit.
        let psci_method = match Fdt::new(DTB_BLOB) {
            Ok(fdt) => fdt::psci_method(&fdt),
            Err(_) => None,
        };
        let Some(psci_method) = psci_method else {
            qemu_exit::exit_failure(FAIL_PSCI_NOT_DISCOVERED);
        };
        if psci_method != VIRT_EXPECTED_PSCI_METHOD {
            qemu_exit::exit_failure(FAIL_PSCI_NOT_DISCOVERED);
        }

        // Build the arch handle with the four-core MPIDR map so `send_ipi`
        // targets the right GICv2 CPU interface. Each core's dense identity
        // is published through its per-CPU word before scheduler/IRQ use.
        // Install the *discovered*
        // PSCI conduit so the `SecondaryBringup` HAL trait issues `CPU_ON`
        // over the conduit read from the tree (`plans/PI.md` P5).
        // Per-CPU bookkeeping backing for this two-core vertical.
        static ARCH_STORAGE: Aarch64ArchStorage<4> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::with_cpus(&ARCH_STORAGE, BOOT_CPU, counter_hz, &[0, 1, 2, 3])
            .with_secondary_start(SecondaryStart::Psci(psci_method));
        smp::install_current_cpu_index(BOOT_CPU);
        if smp::current_cpu_index() != BOOT_CPU {
            qemu_exit::exit_failure(FAIL_WRONG_CPU);
        }

        // P3: prove the GICv2 bases are *discovered*, not assumed. Poison
        // the runtime base, then read the GICD/GICC bases from the
        // embedded `virt` device tree. Every later GIC access on both
        // cores (`gic::init`, the directed SGI, the CPU interface the
        // secondary brings up) goes through this discovered base, so a
        // delivered IPI is the runtime proof the discovered base works.
        gic::configure(POISON_GIC_BASE, POISON_GIC_BASE);
        let discovered = match Fdt::new(DTB_BLOB) {
            Ok(fdt) => gic::configure_from_fdt(&fdt),
            Err(_) => None,
        };
        // The base must have moved off the poison value to the `virt`
        // GICv2 distributor base read from the tree.
        if discovered.is_none() || gic::current().0 != gic::DEFAULT_GICD_BASE {
            qemu_exit::exit_failure(FAIL_GIC_NOT_DISCOVERED);
        }

        // Bring up the boot core's GICv2 distributor so the directed SGI
        // it later raises is forwarded to the CPU interfaces.
        // SAFETY: called once on the boot core during bring-up, before
        // any source is armed.
        unsafe {
            gic::init();
        }

        // Register the secondary-core stack pool sized to this four-core
        // vertical before any `CPU_ON`; the `smp.s` trampoline reads its
        // published base/stride to seed each started core's stack
        // (the pool scales with the machine's core
        // count, not a fixed `const`).
        static SECONDARY_STACKS: smp::SecondaryStackPool<4> = smp::SecondaryStackPool::new();
        if SECONDARY_STACKS.register().is_err() {
            qemu_exit::exit_failure(FAIL_SECONDARY_START);
        }

        // Install the shared callbacks and per-CPU timer storage before
        // starting any secondary, so every core observes complete state.
        preempt::set_ipi_callback(on_ipi);
        preempt::set_timer_callback(on_tick);
        static PREEMPT_STORAGE: preempt::PreemptStorage<4> = preempt::PreemptStorage::new();
        if PREEMPT_STORAGE.register().is_err() {
            qemu_exit::exit_failure(FAIL_SECONDARY_START);
        }
        TIMER_INTERVAL.store(
            preempt::interval_for_hz(counter_hz, TICK_HZ),
            Ordering::Release,
        );
        if smp::set_secondary_entry(secondary_entry).is_err() {
            qemu_exit::exit_failure(FAIL_SECONDARY_START);
        }

        // Start every secondary through the `SecondaryBringup` Arch HAL trait
        // (`plans/WIRING.md` Stage W14/W15) rather than the port-private
        // `smp::start_secondary`, so this vertical exercises the same
        // neutral bring-up surface the x86_64 SMP verticals use; the
        // handle issues PSCI `CPU_ON` over the installed conduit.
        // SAFETY: called on the boot core after the secondary-stack pool
        // was registered (above) and the secondary entry was installed;
        // each id maps to a real, parked, distinct core in the handle's
        // topology.
        for cpu in SECONDARY_CPU..CPU_COUNT {
            if unsafe { arch.start_secondary(cpu) }.is_err() {
                qemu_exit::exit_failure(FAIL_SECONDARY_START);
            }
        }

        // Wait until all three secondary cores have enabled interrupts.
        let ready_mask = ((1u32 << CPU_COUNT) - 1) & !1;
        while SECONDARY_READY.load(Ordering::SeqCst) != ready_mask {
            core::hint::spin_loop();
        }
        // Every secondary armed its own physical timer PPI before publishing
        // readiness. Require all three callbacks before testing SGIs, so the
        // timer mechanism CPU-bound user tasks depend on is covered too.
        while TIMER_FIRED.load(Ordering::SeqCst) != ready_mask {
            core::hint::spin_loop();
        }
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: SMP_SECONDARY_UP,
                message: "aarch64 IPI/SMP test: secondary cores up, sending IPIs",
                fields: &[],
            },
        );

        // Send one directed IPI to each secondary and wait for its callback
        // before targeting the next. This proves every target-list bit, not
        // only CPU 1's, reaches the intended GICv2 CPU interface.
        for cpu in SECONDARY_CPU..CPU_COUNT {
            arch.send_ipi(cpu);
            while IPI_COUNT.load(Ordering::SeqCst) < cpu {
                core::hint::spin_loop();
            }
            if IPI_CPU.load(Ordering::SeqCst) != cpu {
                log(
                    &SERIAL_SINK,
                    &Event {
                        level: Level::Error,
                        id: SMP_TEST_FAIL,
                        message: "aarch64 IPI/SMP test: IPI fired on the wrong core",
                        fields: &[],
                    },
                );
                qemu_exit::exit_failure(FAIL_WRONG_CPU);
            }
        }

        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: SMP_TEST_PASS,
                message: "aarch64 IPI/SMP test: IPIs delivered to every secondary core",
                fields: &[],
            },
        );
        qemu_exit::exit_success();
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
