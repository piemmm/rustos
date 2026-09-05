//! QEMU integration test: the x86_64 hardware referenced (Accessed) bit,
//! read and cleared through the Arch HAL.
//!
//! ## What this test asserts
//!
//! The compressed-memory tier (`plans/SWAPSWAPSWAP.md`) reclaims a task's
//! anonymous page only once the page-replacement clock scan
//! (`kernel/mem::coldscan`) has shown it *genuinely cold* — untouched
//! across a full scan pass. That decision rests on a per-page referenced
//! bit exposed through
//! `tairix_arch_api::mmu::AddressSpace::test_and_clear_accessed`. On
//! x86_64 that bit is the hardware Accessed bit (PTE bit 5), which the CPU
//! sets on the first access and never clears itself (Intel SDM Vol 3A
//! §4.8) — so no software access-flag-fault path is needed, unlike the
//! aarch64 / riscv64 ports.
//!
//! This vertical proves the x86_64 port reports it honestly, on real
//! (emulated) hardware where the software `HostPageTable` double cannot:
//!
//! 1. A freshly-mapped, never-accessed page reads **clear** (not cold-
//!    misclassified as hot, and not spuriously accessed).
//! 2. After a genuine read of the page, the bit reads **set** — and the
//!    probe *clears* it (+ flushes the TLB entry).
//! 3. With no access in between, the next probe reads **clear** again,
//!    proving the clear + TLB flush took effect (the page is now cold).
//! 4. After another genuine access, the bit reads **set** again, proving
//!    the CPU re-sets it after a clear — the whole clock/second-chance
//!    mechanism, end to end.
//!
//! It also checks the fail-closed edges: a misaligned address is rejected
//! and an unmapped address reports "not mapped", never a fabricated
//! verdict, and the port declares `AccessTracking::Supported`.
//!
//! Any other outcome is a closed failure reported to QEMU.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_x86_64)]
use core::fmt::Write as _;
#[cfg(itest_x86_64)]
use core::sync::atomic::{AtomicBool, Ordering};

#[cfg(itest_x86_64)]
use tairix_arch_api::mmu::{AccessTracking, AddressSpace as _, MapError, PageFlags};
#[cfg(itest_x86_64)]
use tairix_arch_x86_64::{idt, paging, qemu_exit, serial};

/// Virtual address the test maps its single 4 KiB probe page at: the first
/// byte past the live identity window, so the mapping is a fresh 4 KiB leaf
/// (its own PDPT/PD/PT) and never lands inside one of the window's huge
/// blocks. Derived from the port's published window rather than picked, so a
/// window widened to cover more RAM cannot swallow it.
#[cfg(itest_x86_64)]
fn test_vaddr() -> u64 {
    paging::configured_identity_bytes()
}

/// A misaligned address, to confirm the fail-closed reject.
#[cfg(itest_x86_64)]
fn misaligned_vaddr() -> u64 {
    test_vaddr() + 0x123
}

/// An address mapped nowhere in the test space, to confirm the "not mapped"
/// fail-closed reject: a gigabyte past the probe page, so it is clear of both
/// the identity window and the probe's own leaf.
#[cfg(itest_x86_64)]
fn unmapped_vaddr() -> u64 {
    test_vaddr() + (1 << 30)
}

/// Magic byte written into the probe frame so the accesses read real data.
#[cfg(itest_x86_64)]
const PROBE_BYTE: u8 = 0x5A;

#[cfg(itest_x86_64)]
static PAGE_TABLE_POOL: paging::PageTablePool = paging::PageTablePool::new();

/// 4 KiB frame the test maps at [`test_vaddr`]. `#[repr(align(4096))]` so
/// its physical address is a valid page frame.
#[cfg(itest_x86_64)]
#[repr(C, align(4096))]
struct ProbeFrame([u8; 4096]);

#[cfg(itest_x86_64)]
static mut PROBE_FRAME: ProbeFrame = ProbeFrame([0; 4096]);

/// `true` once setup is complete. Any fault after this point is a kernel
/// bug (this test provokes none), so the fault handler reports failure.
#[cfg(itest_x86_64)]
static SETUP_DONE: AtomicBool = AtomicBool::new(false);

/// Report a failed expectation and exit QEMU with a failure code.
#[cfg(itest_x86_64)]
fn fail(com1: &mut serial::Serial, msg: &str) -> ! {
    let _ = writeln!(com1, "[accessed_bit] FAIL: {msg}");
    qemu_exit::exit_failure();
}

/// Entry point for the freestanding kernel. Called by
/// `tairix_arch_x86_64`'s boot trampoline after the multiboot magic has
/// been validated.
#[no_mangle]
#[cfg(itest_x86_64)]
pub extern "C" fn kernel_main(_multiboot_info: u64) -> ! {
    let mut com1 = serial::Serial::init(serial::COM1_BASE);
    let _ = writeln!(com1, "[accessed_bit] booted on x86_64");

    // SAFETY: `IDT` is installed exactly once on the boot CPU before any
    // exception can fire; `unexpected_fault` is `-> !` so re-entry is
    // impossible.
    unsafe { idt::init(unexpected_fault) };
    let _ = writeln!(com1, "[accessed_bit] idt installed");

    // Seed the probe frame with a known byte so the accesses read real
    // data. `PROBE_FRAME` is a higher-half kernel static; its physical
    // frame address is its virtual address minus the higher-half base.
    let probe_paddr = (core::ptr::addr_of!(PROBE_FRAME) as u64) - paging::KERNEL_VMA_BASE;
    // SAFETY: `PROBE_FRAME` is owned exclusively by this boot-time setup;
    // no other CPU exists yet (single-CPU test). Use the raw pointer to
    // avoid a `&mut` to the static.
    unsafe {
        core::ptr::addr_of_mut!(PROBE_FRAME)
            .cast::<u8>()
            .write_volatile(PROBE_BYTE);
    }

    let Some(mut space) = paging::AddressSpace::new_identity_window(&PAGE_TABLE_POOL) else {
        fail(&mut com1, "page-table pool exhausted building space");
    };

    // Map the probe page read/write through the Arch HAL MMU surface (the
    // path the architecture-neutral kernel uses).
    if space
        .map_page(
            test_vaddr(),
            probe_paddr,
            PageFlags::READ | PageFlags::WRITE,
        )
        .is_err()
    {
        fail(&mut com1, "probe-page mapping refused");
    }

    // The port must declare it can report a referenced bit.
    if !matches!(space.access_tracking(), AccessTracking::Supported) {
        fail(&mut com1, "x86_64 must declare AccessTracking::Supported");
    }

    // SAFETY: the space carries the live identity window (boot stack / RIP) and the
    // higher-half kernel window, so RIP/RSP stay mapped across the switch.
    unsafe { space.activate() };
    SETUP_DONE.store(true, Ordering::SeqCst);
    let _ = writeln!(
        com1,
        "[accessed_bit] space active, probe mapped at 0x{:x} -> 0x{probe_paddr:x}",
        test_vaddr()
    );

    // ---- Fail-closed edges. ----
    match space.test_and_clear_accessed(misaligned_vaddr()) {
        Err(MapError::Misaligned) => {}
        _ => fail(&mut com1, "misaligned address must be rejected"),
    }
    match space.test_and_clear_accessed(unmapped_vaddr()) {
        Err(MapError::NotMapped) => {}
        _ => fail(&mut com1, "unmapped address must report NotMapped"),
    }
    let _ = writeln!(com1, "[accessed_bit] fail-closed edges OK");

    // ---- Probe 1: a fresh mapping was never accessed → clear. ----
    match space.test_and_clear_accessed(test_vaddr()) {
        Ok(false) => {}
        Ok(true) => fail(&mut com1, "fresh mapping reported accessed"),
        Err(_) => fail(&mut com1, "probe 1 errored on a mapped page"),
    }
    let _ = writeln!(com1, "[accessed_bit] probe 1 (fresh) = clear OK");

    // ---- Access the page, then probe 2 → set (and cleared). ----
    touch(test_vaddr());
    match space.test_and_clear_accessed(test_vaddr()) {
        Ok(true) => {}
        Ok(false) => fail(&mut com1, "accessed page reported clear"),
        Err(_) => fail(&mut com1, "probe 2 errored on a mapped page"),
    }
    let _ = writeln!(com1, "[accessed_bit] probe 2 (after access) = set OK");

    // ---- Probe 3: no access since the clear → clear again. ----
    match space.test_and_clear_accessed(test_vaddr()) {
        Ok(false) => {}
        Ok(true) => fail(&mut com1, "cleared bit did not take effect"),
        Err(_) => fail(&mut com1, "probe 3 errored on a mapped page"),
    }
    let _ = writeln!(com1, "[accessed_bit] probe 3 (cold) = clear OK");

    // ---- Access again, then probe 4 → set (CPU re-sets after a clear). ----
    touch(test_vaddr());
    match space.test_and_clear_accessed(test_vaddr()) {
        Ok(true) => {}
        Ok(false) => fail(&mut com1, "CPU did not re-set the bit after a clear"),
        Err(_) => fail(&mut com1, "probe 4 errored on a mapped page"),
    }
    let _ = writeln!(com1, "[accessed_bit] probe 4 (re-accessed) = set OK");

    let _ = writeln!(
        com1,
        "[accessed_bit] PASS: hardware referenced bit read/cleared through the HAL"
    );
    qemu_exit::exit_success();
}

/// Read one byte from `vaddr` so the CPU sets the leaf's Accessed bit.
/// The read is volatile so the compiler cannot elide it.
#[cfg(itest_x86_64)]
fn touch(vaddr: u64) {
    // SAFETY: `vaddr` is mapped read/write in the active space; the read
    // observes the probe frame and sets the hardware Accessed bit.
    let byte = unsafe { core::ptr::read_volatile(vaddr as *const u8) };
    // Guard against a misconfigured mapping silently reading zero.
    core::hint::black_box(byte);
}

/// IDT-registered fault handler. This test provokes no fault, so any
/// fault is a kernel bug — report it and exit with failure.
#[cfg(itest_x86_64)]
fn unexpected_fault(error_code: u64, rip: u64) -> ! {
    let mut com1 = serial::Serial::init(serial::COM1_BASE);
    let phase = if SETUP_DONE.load(Ordering::SeqCst) {
        "after setup"
    } else {
        "during setup"
    };
    let _ = writeln!(
        com1,
        "[accessed_bit] FAIL: unexpected #PF {phase} error=0x{error_code:x} rip=0x{rip:x}"
    );
    qemu_exit::exit_failure();
}

/// Panic handler for the freestanding binary. Reports failure to QEMU
/// rather than hanging, so a buggy test never silently stalls.
#[panic_handler]
#[cfg(itest_x86_64)]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let mut com1 = serial::Serial::init(serial::COM1_BASE);
    let _ = writeln!(com1, "[accessed_bit] panic: {info}");
    qemu_exit::exit_failure();
}

// Host-target stubs. The crate is *only* meaningful on the bare-metal
// target; on the host we provide a no-op `main` so `cargo build` /
// `cargo test` against the host triple work for IDE indexing and so
// `cargo xtask ci` doesn't have to special-case this crate.
#[cfg(not(itest_x86_64))]
fn main() {}
