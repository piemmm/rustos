//! WIRING Stage W6 QEMU integration test: cross-CPU TLB shootdown on
//! x86_64.
//!
//! ## What this test asserts
//!
//! x86_64 has no broadcast TLB-invalidation instruction, so
//! `X86_64Arch`'s `rustos_arch_api::CrossCpuTlbShootdown` impl raises an
//! inter-processor interrupt at every other online CPU, each of which runs
//! `invlpg` in the shootdown ISR and acknowledges; the initiator spins
//! until every target has acknowledged. This is the only port whose
//! cross-CPU invalidation is entirely hand-written software, so it gets a
//! real-cores proof:
//!
//! 1. The BSP enumerates the application processor from the MADT and
//!    brings it up through `smp::init_sipi_sipi` (INIT-SIPI-SIPI).
//! 2. Both CPUs install the shootdown ISR
//!    (`tlb_shootdown::init_local_tlb_shootdown`); the AP software-enables
//!    its LAPIC, unmasks interrupts, signals `READY`, and idles `hlt`.
//! 3. The BSP drives `X86_64Arch::shootdown_page`, which IPIs the AP and
//!    spins on the acknowledge counter. The call **returns only once the
//!    AP's ISR has run `invlpg` and decremented the counter**, so reaching
//!    the PASS finisher proves the cross-CPU IPI + invalidation + ack
//!    round-trip executed on a second real core.
//!
//! A regression that fails to bring up the AP, or whose AP never services
//! the shootdown IPI, leaves `shootdown_page` spinning forever, so the run
//! times out — the documented fail-loud behaviour.
//!
//! ## How it differs from a production kernel
//!
//! It links only the `rustos-arch-x86_64` port, is alloc-free, and
//! supplies its own `kernel_main`. The QEMU-exit shortcut lives in this
//! dedicated bin, never behind a Cargo feature on the arch crate
//! (fail closed).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]
#![cfg_attr(itest_x86_64, allow(clippy::cast_possible_truncation))]

#[cfg(itest_x86_64)]
mod kernel {
    use core::fmt::Write as _;
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    use rustos_arch_api::{CrossCpuTlbShootdown, SecondaryBringup};
    use rustos_arch_x86_64::acpi::{self, MadtEntry};
    use rustos_arch_x86_64::apic::{Lapic, VolatileLapicMmio};
    use rustos_arch_x86_64::kernel_arch::{X86_64Arch, X86_64ArchStorage};
    use rustos_arch_x86_64::multiboot2::BootInfo;
    use rustos_arch_x86_64::smp;
    use rustos_arch_x86_64::{percpu, preempt, qemu_exit, serial, tlb_shootdown};

    /// Logical CPUs this vertical brings up: the BSP plus a single AP
    /// (the per-CPU backings are sized to the CPUs the
    /// caller actually drives, not a baked-in `MAX_CPUS`).
    const CPUS: usize = 2;

    /// A representative page to invalidate. The exact address is
    /// immaterial — a TLB shootdown can only ever *over*-invalidate.
    const SHOOTDOWN_VADDR: u64 = 0x10_0000_0000;

    /// Set to `1` by the AP once its shootdown ISR is installed and
    /// interrupts are enabled, so the BSP only shoots down a live target.
    static AP_READY: AtomicU32 = AtomicU32::new(0);

    /// Set by the BSP once the test is finished, so the AP leaves its idle
    /// loop and halts.
    static SHUTDOWN: AtomicBool = AtomicBool::new(false);

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

    fn discover_aps(multiboot_info: u64, bsp_id: u8) -> Option<ApList> {
        // SAFETY: `multiboot_info` is the verbatim pointer from `boot.s`
        // SAFETY-INVARIANT 7, in the identity-mapped 0..4 GiB window. We
        // read the 4-byte total_size first, then bound the rest.
        let header = unsafe { core::slice::from_raw_parts(multiboot_info as *const u8, 8) };
        let total_size = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
        // SAFETY: same justification; the length is now known.
        let mb = unsafe { core::slice::from_raw_parts(multiboot_info as *const u8, total_size) };
        let info = BootInfo::parse(mb).ok()?;

        let rsdp_bytes = info.rsdp()?;
        let rsdp = acpi::Rsdp::validate(rsdp_bytes).ok()?;
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

        while !SHUTDOWN.load(Ordering::Acquire) {
            // SAFETY: `hlt` with IF=1 parks until the next interrupt (the
            // shootdown IPI), then re-checks the shutdown flag.
            unsafe {
                core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
            }
        }
        halt_forever();
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
    fn rustos_xtlb_x86_64_panic(info: &core::panic::PanicInfo<'_>) -> ! {
        let mut com1 = serial::Serial::init(serial::COM1_BASE);
        let _ = writeln!(com1, "[cross_cpu_tlb_shootdown_qemu_x86_64] panic: {info}");
        qemu_exit::exit_failure();
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
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
            // The BSP is the initiator (it `invlpg`s itself), so it does
            // not strictly need the shootdown ISR, but install it for
            // symmetry and to keep a stray self-IPI well-defined.
            if tlb_shootdown::init_local_tlb_shootdown(0).is_err() {
                qemu_exit::exit_failure();
            }
        }

        let mut lapic = make_lapic();
        lapic.software_enable(LAPIC_SWENABLE);
        let bsp_id = smp::bsp_lapic_id();
        let _ = writeln!(
            com1,
            "[cross_cpu_tlb_shootdown_qemu_x86_64] BSP LAPIC id = {bsp_id}"
        );

        let Some(ap_ids) = discover_aps(multiboot_info, bsp_id) else {
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
        // orchestration lives in `rustos_arch_x86_64::smp`, Stage W14).
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
        while AP_READY.load(Ordering::Acquire) == 0 {
            core::hint::spin_loop();
        }
        let _ = writeln!(
            com1,
            "[cross_cpu_tlb_shootdown_qemu_x86_64] AP up, issuing cross-CPU shootdown"
        );

        // Drive the real HAL entry point. `shootdown_page` IPIs the AP and
        // spins on the acknowledge counter; it returns *only* once the
        // AP's ISR has run `invlpg` and acknowledged. Reaching the next
        // line therefore proves the cross-CPU round-trip ran on the AP.
        arch.shootdown_page(SHOOTDOWN_VADDR);
        // A second shootdown proves the mailbox is correctly released and
        // reusable.
        arch.shootdown_page(SHOOTDOWN_VADDR + 0x1000);

        SHUTDOWN.store(true, Ordering::Release);
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

#[cfg(not(itest_x86_64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
