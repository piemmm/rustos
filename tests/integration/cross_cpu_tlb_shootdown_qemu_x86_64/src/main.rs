//! WIRING Stage W6 QEMU integration test: cross-CPU TLB shootdown on
//! x86_64.
//!
//! ## What this test asserts
//!
//! x86_64 has no broadcast TLB-invalidation instruction, so `X86_64Arch`'s
//! `tairix_arch_api::CrossCpuTlbShootdown` impl raises an inter-processor
//! interrupt at every other online CPU, each of which runs `invlpg` and
//! acknowledges; the initiator spins until every target has acknowledged.
//! This is the only port whose cross-CPU invalidation is entirely
//! hand-written software, so it gets a real-cores proof of all three ways an
//! acknowledge can come back:
//!
//! 1. The BSP enumerates the application processor from the MADT and
//!    brings it up through `smp::init_sipi_sipi` (INIT-SIPI-SIPI).
//! 2. Both CPUs install the shootdown ISR
//!    (`tlb_shootdown::init_local_tlb_shootdown`); the AP software-enables
//!    its LAPIC, unmasks interrupts, signals `AP_READY`, and waits for work.
//! 3. **From the ISR.** The BSP drives `X86_64Arch::shootdown_page` with the
//!    AP interrupt-enabled. The call returns only once the AP's ISR has run
//!    `invlpg` and decremented the count.
//! 4. **From a masked lock-acquire spin.** The BSP takes an
//!    `IrqSafeSpinLock` — which masks the BSP — and shoots down while the AP
//!    is spinning to acquire *that same lock*, so the AP has its own
//!    interrupts masked and cannot take the IPI at all. This is the
//!    production shape: the kernel-heap teardown is a masked initiator and
//!    the heap lock is the lock a second CPU spins for. The acknowledge can
//!    only come from the AP's spin round, through the spin service the port
//!    installs into `lib/sync`.
//! 5. **From a masked mailbox-acquire spin.** Both CPUs mask, rendezvous,
//!    and then each initiates a shootdown at the other. Whichever wins the
//!    descriptor cannot finish until the loser acknowledges, and the loser is
//!    masked with nothing to spin on but the descriptor — so the loser's
//!    mailbox spin is forced to be the acknowledging path, whichever way the
//!    race falls.
//!
//! A regression in any of the three leaves an initiator spinning for an
//! acknowledge that never comes, so the run times out — the documented
//! fail-loud behaviour. Steps 4 and 5 are the `plans/OPEN-DEFECTS.md` D52
//! deadlock: before the protocol let a target acknowledge from a spin, each
//! of them wedged both CPUs permanently.
//!
//! ## How it differs from a production kernel
//!
//! It links only the `tairix-arch-x86_64` port and `lib/sync`, is alloc-free,
//! and supplies its own `kernel_main` — so it installs the spin service
//! itself where the production kernel installs it in `x86_64/boot.rs`. The
//! QEMU-exit shortcut lives in this dedicated bin, never behind a Cargo
//! feature on the arch crate (fail closed).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]
// The bare-metal body narrows page-table and APIC register fields whose
// widths the hardware fixes; each site's value is masked or shifted into
// range first.
#![cfg_attr(itest_x86_64, allow(clippy::cast_possible_truncation))]

#[cfg(itest_x86_64)]
mod kernel {
    use core::fmt::Write as _;
    use core::sync::atomic::{AtomicU32, Ordering};

    use tairix_arch_api::{CrossCpuTlbShootdown, SecondaryBringup};
    use tairix_arch_x86_64::acpi::{self, MadtEntry};
    use tairix_arch_x86_64::apic::{Lapic, VolatileLapicMmio};
    use tairix_arch_x86_64::bootinfo::BootData;
    use tairix_arch_x86_64::irqmask::PortIrqControl;
    use tairix_arch_x86_64::kernel_arch::{X86_64Arch, X86_64ArchStorage};
    use tairix_arch_x86_64::smp;
    use tairix_arch_x86_64::{percpu, preempt, qemu_exit, serial, tlb_shootdown};
    use tairix_sync::irq::InterruptControl as _;
    use tairix_sync::IrqSafeSpinLock;

    /// Logical CPUs this vertical brings up: the BSP plus a single AP
    /// (the per-CPU backings are sized to the CPUs the
    /// caller actually drives, not a baked-in `MAX_CPUS`).
    const CPUS: usize = 2;

    /// A representative page to invalidate. The exact address is
    /// immaterial — a TLB shootdown can only ever *over*-invalidate.
    const SHOOTDOWN_VADDR: u64 = 0x10_0000_0000;

    /// What the BSP has asked the AP to do next.
    const AP_WAIT: u32 = 0;
    /// Spin, interrupts masked, for the lock the BSP is shooting down under.
    const AP_SPIN_FOR_GATE: u32 = 1;
    /// Rendezvous masked, then initiate a shootdown back at the BSP.
    const AP_SHOOT_AT_BSP: u32 = 2;
    /// Leave the work loop and park.
    const AP_STOP: u32 = 3;

    /// Set to `1` by the AP once its shootdown ISR is installed and
    /// interrupts are enabled, so the BSP only shoots down a live target.
    static AP_READY: AtomicU32 = AtomicU32::new(0);

    /// The step the AP is to run, published by the BSP.
    static AP_COMMAND: AtomicU32 = AtomicU32::new(AP_WAIT);

    /// Raised by the AP once its own interrupts are masked, so the BSP knows
    /// the target it is about to shoot down genuinely cannot take the IPI.
    static AP_MASKED: AtomicU32 = AtomicU32::new(0);

    /// The mirror of [`AP_MASKED`], so step 5 has both CPUs masked before
    /// either touches the shootdown descriptor.
    static BSP_MASKED: AtomicU32 = AtomicU32::new(0);

    /// Raised by the AP when the published step is complete.
    static AP_DONE: AtomicU32 = AtomicU32::new(0);

    /// The BSP's LAPIC id, so the AP can shoot back at it in step 5.
    static BSP_LAPIC_ID: AtomicU32 = AtomicU32::new(0);

    /// The lock whose *masked acquire spin* has to acknowledge a shootdown:
    /// the BSP holds it across the shootdown while the AP spins for it,
    /// which is the kernel heap's lock discipline in miniature.
    static GATE: IrqSafeSpinLock<(), PortIrqControl> = IrqSafeSpinLock::new(());

    /// Software-enable mask written to the LAPIC spurious-interrupt
    /// register (all-ones spurious vector, APIC software-enable bit set).
    const LAPIC_SWENABLE: u8 = 0xFF;

    fn make_lapic() -> Lapic<VolatileLapicMmio> {
        // SAFETY: the LAPIC MMIO base is 0xFEE00000 on every Intel-
        // architecture system QEMU emulates, identity-mapped by `boot.s`
        // (SAFETY-INVARIANT 4 — 0..4 GiB identity map).
        let mmio = unsafe { VolatileLapicMmio::new(0xFEE0_0000 as *mut u32) };
        Lapic::new(mmio)
    }

    /// Enabled application processors discovered from the MADT (BSP
    /// excluded). `AP_TRAMPOLINE_LEN` is an over-provisioned upper bound.
    struct ApList {
        ids: [u8; smp::AP_TRAMPOLINE_LEN],
        count: usize,
    }

    fn discover_aps(boot_info: u64, bsp_id: u8) -> Option<ApList> {
        // SAFETY: `boot_info` is the verbatim pointer from `boot.s`
        // SAFETY-INVARIANT 7. The blob and every table it points at sit
        // in the identity-mapped 0..4 GiB window, the documented
        // contract of `BootData::load` / `validated_rsdp`, whose
        // parsers bound every slice before reading it.
        let data = unsafe { BootData::load(boot_info) }.ok()?;
        // SAFETY: same identity-window contract as the load above.
        let rsdp = unsafe { data.validated_rsdp() }?;
        // SAFETY: the RSDP came from firmware and its table pointers sit
        // in the boot trampoline's 0..4 GiB identity-mapped window.
        let madt_bytes = unsafe { acpi::locate_madt(&rsdp) }?;
        let madt = acpi::Madt::parse(madt_bytes).ok()?;

        let mut list = ApList {
            ids: [0; smp::AP_TRAMPOLINE_LEN],
            count: 0,
        };
        for entry in madt.entries() {
            if let MadtEntry::LocalApic { apic_id, flags, .. } = entry {
                // ACPI 6.5 Table 5.40: bit 0 = "Processor Enabled".
                if flags & 1 == 0 || apic_id == bsp_id {
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

    /// Wait for `flag` to be raised. Bare rather than event-driven because
    /// this bin has two cores, no scheduler and no timer, so a cross-CPU
    /// store is the only wake source there is.
    fn await_flag(flag: &AtomicU32) {
        while flag.load(Ordering::Acquire) == 0 {
            core::hint::spin_loop();
        }
    }

    /// Entry the AP runs after the trampoline hands control to long mode.
    extern "C" fn ap_entry(cpu_id: u32) -> ! {
        // SAFETY: called once on this AP before interrupts are enabled;
        // installs its per-CPU GDT + IDT.
        unsafe {
            if percpu::init(cpu_id as usize).is_err() {
                halt_forever();
            }
            // Install the shootdown ISR in this AP's IDT so it can field
            // the BSP's shootdown IPI.
            if tlb_shootdown::init_local_tlb_shootdown(cpu_id as usize).is_err() {
                halt_forever();
            }
        }

        let mut lapic = make_lapic();
        lapic.software_enable(LAPIC_SWENABLE);

        // SAFETY: per-CPU IDT installed, shootdown vector points at the
        // ISR stub, LAPIC software-enabled — ready to field the IPI.
        unsafe {
            core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
        }
        AP_READY.store(1, Ordering::Release);

        loop {
            match AP_COMMAND.load(Ordering::Acquire) {
                AP_SPIN_FOR_GATE => ap_spin_for_gate(),
                AP_SHOOT_AT_BSP => ap_shoot_at_bsp(),
                AP_STOP => halt_forever(),
                // Interrupts are enabled here, so the plain shootdowns of
                // step 3 are acknowledged from the ISR while the AP waits.
                _ => core::hint::spin_loop(),
            }
        }
    }

    /// Step 4's AP half: mask, then block on the lock the BSP holds.
    ///
    /// Masking *before* publishing [`AP_MASKED`] is what makes the step
    /// deterministic: once the BSP sees the flag, this CPU provably cannot
    /// take the shootdown IPI until it has acquired and released the gate,
    /// which the BSP will not permit until its shootdown has returned.
    fn ap_spin_for_gate() {
        let state = PortIrqControl::disable();
        AP_MASKED.store(1, Ordering::Release);
        drop(GATE.lock());
        // SAFETY: `state` came from the `disable` directly above, on this
        // CPU, and has not been restored yet.
        unsafe { PortIrqControl::restore(state) };
        AP_COMMAND.store(AP_WAIT, Ordering::Release);
        AP_DONE.store(1, Ordering::Release);
    }

    /// Step 5's AP half: hold the gate, rendezvous masked, then initiate at
    /// the BSP.
    ///
    /// The rendezvous spin is safe while bare because no shootdown can be in
    /// flight during it — neither CPU has touched the descriptor yet. Both
    /// are masked by the time either does, which is what forces the loser of
    /// the descriptor race to acknowledge from its mailbox spin.
    ///
    /// The gate is held across the shootdown so that the BSP — which never
    /// enables interrupts in this bin — has a served spin to acknowledge
    /// from when the AP is the one that wins the descriptor.
    ///
    /// The module entry point rather than the HAL one because the arch
    /// handle is the BSP's local; `shootdown` is the protocol itself, and
    /// step 3 already drives it through `X86_64Arch`.
    fn ap_shoot_at_bsp() {
        let state = PortIrqControl::disable();
        let gate = GATE.lock();
        AP_MASKED.store(1, Ordering::Release);
        await_flag(&BSP_MASKED);
        let bsp = BSP_LAPIC_ID.load(Ordering::Relaxed) as u8;
        tlb_shootdown::shootdown(SHOOTDOWN_VADDR + 0x4000, 1, core::iter::once(bsp));
        drop(gate);
        // SAFETY: `state` came from the `disable` above, on this CPU, and
        // has not been restored yet.
        unsafe { PortIrqControl::restore(state) };
        AP_COMMAND.store(AP_WAIT, Ordering::Release);
        AP_DONE.store(1, Ordering::Release);
    }

    /// Publish `command`, wait for the AP to finish it, and reset the
    /// handshake flags for the next step.
    fn drive_ap(command: u32) {
        AP_DONE.store(0, Ordering::Release);
        AP_MASKED.store(0, Ordering::Release);
        AP_COMMAND.store(command, Ordering::Release);
    }

    /// Mask interrupts and park this CPU forever.
    fn halt_forever() -> ! {
        loop {
            // SAFETY: `cli; hlt` with IF=0 is a well-defined privileged
            // halt.
            unsafe {
                core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
            }
        }
    }

    /// Forward to a serial-logging panic that exits QEMU with failure.
    #[panic_handler]
    fn tairix_xtlb_x86_64_panic(info: &core::panic::PanicInfo<'_>) -> ! {
        let mut com1 = serial::Serial::init(serial::COM1_BASE);
        let _ = writeln!(com1, "[cross_cpu_tlb_shootdown_qemu_x86_64] panic: {info}");
        qemu_exit::exit_failure();
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls.
    #[no_mangle]
    pub extern "C" fn kernel_main(boot_info: u64) -> ! {
        let mut com1 = serial::Serial::init(serial::COM1_BASE);
        let _ = writeln!(com1, "[cross_cpu_tlb_shootdown_qemu_x86_64] BSP boot");

        // Publish the caller-owned per-CPU GDT/IDT/IST arena (covering the
        // BSP and the one AP) before any `percpu::init`, on the BSP and
        // before the AP is started so the AP's `percpu::init(1)` sees it. Set-once; this `kernel_main` runs once.
        static PER_CPU_STORAGE: percpu::PerCpuStorage<CPUS> = percpu::PerCpuStorage::new();
        if PER_CPU_STORAGE.register().is_err() {
            qemu_exit::exit_failure();
        }
        // SAFETY: BSP, once, before interrupts are enabled; installs the
        // BSP per-CPU GDT + IDT.
        unsafe {
            if percpu::init(0).is_err() {
                qemu_exit::exit_failure();
            }
            // The BSP fields the AP's shootdown in step 5, and a stray
            // self-IPI must be well-defined in any case.
            if tlb_shootdown::init_local_tlb_shootdown(0).is_err() {
                qemu_exit::exit_failure();
            }
        }

        // Let a CPU spinning with its interrupts masked acknowledge a
        // shootdown, which is the only way steps 4 and 5 can complete. The
        // production kernel installs the same service from
        // `x86_64/boot.rs`; here it is installed before the AP starts, so no
        // shootdown can be in flight while a spinner has no way to answer.
        tairix_sync::spinwait::install_service(tlb_shootdown::serve_pending);

        let mut lapic = make_lapic();
        lapic.software_enable(LAPIC_SWENABLE);
        let bsp_id = smp::bsp_lapic_id();
        BSP_LAPIC_ID.store(u32::from(bsp_id), Ordering::Relaxed);
        let _ = writeln!(
            com1,
            "[cross_cpu_tlb_shootdown_qemu_x86_64] BSP LAPIC id = {bsp_id}"
        );

        let Some(ap_ids) = discover_aps(boot_info, bsp_id) else {
            let _ = writeln!(
                com1,
                "[cross_cpu_tlb_shootdown_qemu_x86_64] FAIL: MADT discovery"
            );
            qemu_exit::exit_failure();
        };
        if ap_ids.count == 0 {
            let _ = writeln!(
                com1,
                "[cross_cpu_tlb_shootdown_qemu_x86_64] FAIL: no application processor found"
            );
            qemu_exit::exit_failure();
        }
        let ap_id = ap_ids.ids[0];

        // Publish the LAPIC-id -> dense-CpuId map so `current_cpu` (read
        // by `shootdown_page`) resolves on both CPUs, then build the arch
        // handle with the same two-CPU map.
        preempt::set_cpu_id_for_lapic(bsp_id, 0);
        preempt::set_cpu_id_for_lapic(ap_id, 1);
        let mut cpu_to_lapic: [Option<u8>; CPUS] = [None; CPUS];
        cpu_to_lapic[0] = Some(bsp_id);
        cpu_to_lapic[1] = Some(ap_id);
        // The arch handle borrows its per-CPU bookkeeping from a
        // caller-sized `&'static` backing; `kernel_main`
        // runs once, so a function-local `static` is sound and needs no
        // allocator. `shootdown_page` walks exactly this two-CPU map.
        static ARCH_STORAGE: X86_64ArchStorage<CPUS> = X86_64ArchStorage::new();
        let arch = match X86_64Arch::new(&ARCH_STORAGE, 0, bsp_id, &cpu_to_lapic) {
            Ok(a) => a,
            Err(_) => qemu_exit::exit_failure(),
        };

        // Install the AP entry once, then bring the single AP up through
        // the Arch HAL `SecondaryBringup` trait (the INIT-SIPI-SIPI
        // orchestration lives in `tairix_arch_x86_64::smp`, Stage W14).
        if smp::set_secondary_entry(ap_entry).is_err() {
            let _ = writeln!(
                com1,
                "[cross_cpu_tlb_shootdown_qemu_x86_64] FAIL: secondary entry already installed"
            );
            qemu_exit::exit_failure();
        }
        // Publish the caller-owned AP bootstrap-stack pool (one stack per
        // application processor) before `start_secondary`. Set-once; fails closed before registration.
        static AP_STACKS: smp::ApStackPool<{ CPUS - 1 }> = smp::ApStackPool::new();
        if AP_STACKS.register().is_err() {
            let _ = writeln!(
                com1,
                "[cross_cpu_tlb_shootdown_qemu_x86_64] FAIL: AP stack pool already registered"
            );
            qemu_exit::exit_failure();
        }
        // SAFETY: BSP; `boot.s` zeroed `.bss` (clear AP stack pool), the
        // BSP LAPIC is software-enabled, the entry is installed, and dense
        // CPU 1 maps to the real, parked AP discovered from the MADT.
        if let Err(e) = unsafe { arch.start_secondary(1) } {
            let _ = writeln!(
                com1,
                "[cross_cpu_tlb_shootdown_qemu_x86_64] FAIL: start_secondary: {}",
                e.as_str()
            );
            qemu_exit::exit_failure();
        }

        // Wait until the AP has installed its shootdown ISR and enabled
        // interrupts, so the IPI has a live target.
        await_flag(&AP_READY);
        let _ = writeln!(
            com1,
            "[cross_cpu_tlb_shootdown_qemu_x86_64] AP up, issuing cross-CPU shootdown"
        );

        // Step 3 — acknowledge from the ISR. `shootdown_page` IPIs the AP
        // and spins on the acknowledge counter; it returns *only* once the
        // AP's ISR has run `invlpg` and acknowledged. Reaching the next line
        // therefore proves the cross-CPU round-trip ran on the AP.
        arch.shootdown_page(SHOOTDOWN_VADDR);
        // A second shootdown proves the mailbox is correctly released and
        // reusable.
        arch.shootdown_page(SHOOTDOWN_VADDR + 0x1000);

        // Step 4 — acknowledge from a masked lock-acquire spin. The gate is
        // held across the shootdown, so the AP is masked *and* blocked on
        // this CPU: no IPI can reach it and only its spin round can answer.
        {
            // Held *before* the AP is told to spin for it, so the AP cannot
            // win it first and reduce the step to the ISR path.
            let _gate = GATE.lock();
            drive_ap(AP_SPIN_FOR_GATE);
            // Safe while bare: no shootdown is in flight yet, so this wait
            // owes nothing to the AP.
            await_flag(&AP_MASKED);
            arch.shootdown_page(SHOOTDOWN_VADDR + 0x2000);
        }
        await_flag(&AP_DONE);
        let _ = writeln!(
            com1,
            "[cross_cpu_tlb_shootdown_qemu_x86_64] masked spinner acknowledged the shootdown"
        );

        // Step 5 — acknowledge from a masked mailbox-acquire spin. Both CPUs
        // mask before either takes the descriptor, so whichever wins it
        // cannot finish until the loser serves the request from its own
        // mailbox spin.
        drive_ap(AP_SHOOT_AT_BSP);
        let state = PortIrqControl::disable();
        BSP_MASKED.store(1, Ordering::Release);
        // Safe while bare for the same reason as the AP's half: nothing is in
        // flight until both CPUs are past this point.
        await_flag(&AP_MASKED);
        arch.shootdown_page(SHOOTDOWN_VADDR + 0x3000);
        // The AP holds the gate across its own shootdown, so this masked
        // spin is where this CPU acknowledges it when the AP won the
        // descriptor. This bin never enables interrupts on the BSP, so a
        // served spin is the only route home — which is the point.
        drop(GATE.lock());
        // SAFETY: `state` came from the `disable` above, on this CPU, and
        // has not been restored yet.
        unsafe { PortIrqControl::restore(state) };
        await_flag(&AP_DONE);
        let _ = writeln!(
            com1,
            "[cross_cpu_tlb_shootdown_qemu_x86_64] two masked initiators both completed"
        );

        AP_COMMAND.store(AP_STOP, Ordering::Release);
        let _ = writeln!(
            com1,
            "[cross_cpu_tlb_shootdown_qemu_x86_64] PASS: AP acknowledged the shootdown"
        );
        qemu_exit::exit_success();
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
