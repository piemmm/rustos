//! WIRING Stage W6 QEMU integration test: multi-core SMP bring-up and
//! IPI delivery on the aarch64 `virt` board.
//!
//! ## What this test asserts
//!
//! `plans/WIRING.md` Stage W6 requires that the aarch64 boot core can
//! (1) start a secondary core and (2) deliver a directed inter-processor
//! interrupt to it — the EL1/GICv2 analogue of the riscv64 vertical.
//! This binary exercises both, end to end, on a two-core `virt` board:
//!
//! 1. The boot core (core 0) installs the shared IPI callback
//!    (`preempt::set_ipi_callback`) and the secondary-core entry
//!    (`smp::set_secondary_entry`), and brings up its own GICv2
//!    distributor (`gic::init`).
//! 2. It starts core 1 via `smp::start_secondary` (the PSCI `CPU_ON`
//!    call), which runs the `smp.s` trampoline → the installed entry on
//!    core 1.
//! 3. Core 1 installs the EL1 vector table (`exceptions::init_vectors`),
//!    brings up its GICv2 CPU interface (`gic::init`), enables the IPI
//!    SGI (`preempt::enable_ipi`), unmasks IRQs (`exceptions::enable_irq`),
//!    publishes a `READY` flag, then idles on `wfi`.
//! 4. The boot core waits for `READY`, then sends an IPI to logical CPU
//!    1 through `Aarch64Arch::send_ipi` (a GICv2 directed SGI), which
//!    raises INTID 0 on core 1.
//! 5. Core 1 takes the IRQ, the IRQ path runs `preempt::on_ipi_interrupt`
//!    → the IPI callback, recording the core the callback fired on. The
//!    boot core waits for the callback to fire on core 1, then writes the
//!    ARM semihosting PASS finisher.
//!
//! A regression that fails to start the secondary core or to deliver the
//! IPI never reaches the PASS write, so the run times out and the
//! harness reports `Outcome::Timeout` — the documented fail-loud
//! behaviour (`AGENTS.md` §7).
//!
//! ## PSCI conduit (PI Stage P5)
//!
//! `smp::start_secondary` takes the PSCI conduit (`hvc`/`smc`) as a
//! parameter. This vertical proves the conduit is **discovered**, not
//! assumed: before building the arch handle it reads `/psci` `method`
//! from the canonical `virt` device tree embedded at build time
//! (`fdt::psci_method`) and fails closed if no PSCI node is found, then
//! installs *that discovered* conduit on the handle. The secondary core
//! this test starts is therefore brought up over the conduit read from
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
//! Every subsequent GIC access on both cores — `gic::init`, the directed
//! SGI, and the CPU interface the secondary brings up — targets that
//! discovered base, so the IPI this test delivers is the runtime proof
//! the discovered base works (`plans/PI.md` P3). The board tree is
//! embedded, not read from `x0`, for the same reason as the PSCI conduit
//! above: QEMU's ELF `-kernel` boot hands no DTB pointer.
//!
//! ## How it differs from a production kernel
//!
//! It links only the `rustos-arch-aarch64` port (the SMP path needs no
//! `kernel/*` subsystem) and supplies its own `kernel_main`. The
//! QEMU-exit shortcut lives in this dedicated bin, never behind a Cargo
//! feature on the arch crate (`AGENTS.md` §5.4.5 — fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU32, Ordering};

    use rustos_arch_aarch64::kernel_arch::read_cntfrq;
    use rustos_arch_aarch64::{
        exceptions, fdt, gic, handle_panic_via_serial, preempt, qemu_exit, smp, Aarch64Arch,
        Aarch64ArchStorage, SERIAL_SINK,
    };
    use rustos_arch_api::{CpuId, SchedulerArch, SecondaryBringup};
    use rustos_fdt::Fdt;
    use rustos_log::{log, Event, EventId, Level};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`): the GICv2 bases are read from it (P3), proving
    // the IPI is delivered over a *discovered* base, not a constant.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// `u32` sentinel for "no IPI callback has fired yet".
    const NO_CPU: u32 = u32::MAX;

    /// Dense id of the boot core. QEMU always enters the `virt` board on
    /// the primary core, and the board assigns it affinity 0.
    const BOOT_CPU: CpuId = 0;

    /// Dense id of the secondary core this test starts.
    const SECONDARY_CPU: CpuId = 1;

    /// `MPIDR_EL1` affinity QEMU assigns each core on the `virt` board:
    /// the linear core index. Core 1's affinity is therefore 1.
    const SECONDARY_MPIDR: u64 = SECONDARY_CPU as u64;

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
    const FAIL_SECONDARY_START: u16 = 1;
    /// Failure finisher code: the IPI fired on the wrong core.
    const FAIL_WRONG_CPU: u16 = 2;
    /// Failure finisher code: `CNTFRQ_EL0` reported a zero frequency.
    const FAIL_ZERO_FREQ: u16 = 3;
    /// Failure finisher code: the GICv2 bases were not discovered from the
    /// embedded `virt` device tree (P3).
    const FAIL_GIC_NOT_DISCOVERED: u16 = 4;
    /// Failure finisher code: the PSCI conduit was not discovered from the
    /// embedded `virt` device tree, or did not match the board's `hvc`
    /// (P5).
    const FAIL_PSCI_NOT_DISCOVERED: u16 = 5;

    /// A deliberately-wrong GICv2 distributor/CPU-interface base installed
    /// before discovery runs. It is **not** the `virt` GICv2 base, so a
    /// later successful IPI delivery can only mean discovery overwrote it
    /// with the base read from the device tree.
    const POISON_GIC_BASE: usize = 0xdead_0000;

    /// Set to `1` by the secondary core once its vector table, GICv2
    /// interface, and IPI SGI enable are in place.
    static SECONDARY_READY: AtomicU32 = AtomicU32::new(0);

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

    /// Entry the secondary core runs (via the `smp.s` trampoline) once
    /// the boot core starts it. Brings up its interrupt path, signals
    /// ready, and idles waiting for the IPI.
    extern "C" fn secondary_entry(_cpu: CpuId) -> ! {
        // SAFETY: this is the secondary core's first action; it has a
        // private stack (smp.s) and no source is armed on it yet. The
        // shared IPI callback was installed by the boot core before it
        // started this core. The vector table and GIC CPU interface are
        // per-CPU, so each must be installed on the core that uses them.
        unsafe {
            exceptions::init_vectors();
            gic::init();
            preempt::enable_ipi();
            exceptions::enable_irq();
        }
        // Publish readiness only after interrupts are enabled, so the
        // boot core cannot send the IPI before this core can take it.
        SECONDARY_READY.store(1, Ordering::SeqCst);

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
    fn rustos_ipi_smp_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s`
    /// trampoline calls (via `rustos_arch_aarch64_main`).
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

        // Build the arch handle with the two-core MPIDR map so
        // `current_cpu` reverse-maps each core's affinity and `send_ipi`
        // targets the right GICv2 CPU interface. Install the *discovered*
        // PSCI conduit so the `SecondaryBringup` HAL trait issues `CPU_ON`
        // over the conduit read from the tree (`plans/PI.md` P5).
        // Per-CPU bookkeeping backing for this two-core vertical
        // (`AGENTS.md` §24.1).
        static ARCH_STORAGE: Aarch64ArchStorage<2> = Aarch64ArchStorage::new();
        let arch = Aarch64Arch::with_cpus(
            &ARCH_STORAGE,
            BOOT_CPU,
            counter_hz,
            &[BOOT_CPU as u64, SECONDARY_MPIDR],
        )
        .with_psci_method(psci_method);

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

        // Install the shared callbacks before starting the secondary
        // core, so it observes them already in place.
        preempt::set_ipi_callback(on_ipi);
        if smp::set_secondary_entry(secondary_entry).is_err() {
            qemu_exit::exit_failure(FAIL_SECONDARY_START);
        }

        // Start core 1 through the `SecondaryBringup` Arch HAL trait
        // (`plans/WIRING.md` Stage W14/W15) rather than the port-private
        // `smp::start_secondary`, so this vertical exercises the same
        // neutral bring-up surface the x86_64 SMP verticals use; the
        // handle issues PSCI `CPU_ON` over the installed conduit.
        // SAFETY: called on the boot core after `boot.s` zeroed `.bss`
        // (clearing the secondary stack pool) and after the secondary
        // entry was installed; `SECONDARY_CPU` maps to a real, parked,
        // distinct core in the handle's topology.
        if unsafe { arch.start_secondary(SECONDARY_CPU) }.is_err() {
            qemu_exit::exit_failure(FAIL_SECONDARY_START);
        }

        // Wait until the secondary core has enabled interrupts.
        while SECONDARY_READY.load(Ordering::SeqCst) == 0 {
            core::hint::spin_loop();
        }
        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: SMP_SECONDARY_UP,
                message: "aarch64 IPI/SMP test: secondary core up, sending IPI",
                fields: &[],
            },
        );

        // Send a directed IPI to the secondary core through the arch
        // handle's GICv2 SGI path (the deliverable that replaces the
        // former single-CPU self-target best-effort send).
        arch.send_ipi(SECONDARY_CPU);

        // Wait for the secondary core to take the SGI IRQ and run the
        // callback.
        while IPI_COUNT.load(Ordering::SeqCst) == 0 {
            core::hint::spin_loop();
        }

        // The callback must have fired on the secondary core, not the
        // boot core.
        if IPI_CPU.load(Ordering::SeqCst) != SECONDARY_CPU {
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

        log(
            &SERIAL_SINK,
            &Event {
                level: Level::Info,
                id: SMP_TEST_PASS,
                message: "aarch64 IPI/SMP test: IPI delivered to secondary core",
                fields: &[],
            },
        );
        qemu_exit::exit_success();
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
