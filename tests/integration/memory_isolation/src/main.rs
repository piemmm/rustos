//! Stage-2 QEMU integration test: hardware-enforced memory isolation.
//!
//! ## What this test asserts
//!
//! `AGENTS.md` §4 states that "Memory isolation is enforced by hardware
//! (page tables / MMU / WASM sandboxing). A process can only reach
//! another process's memory through an explicit, capability-checked
//! shared-memory IPC object." Stage 2 of `PLAN.md` (deliverable: "QEMU-
//! based integration tests for memory isolation: a test process
//! attempting to read another's memory must fault") makes that promise
//! concrete at the page-table layer.
//!
//! ## How it asserts it
//!
//! Two distinct `AddressSpace`s are constructed (`rustos_arch_x86_64`'s
//! Stage-3a-partial paging primitives — see that crate's docs):
//!
//! * **Victim** — identity-maps the first 32 MiB *and* adds a 4 KiB
//!   mapping at the secret virtual address `SECRET_VADDR` pointing to a
//!   physical frame initialised with the byte `SECRET_BYTE`.
//! * **Attacker** — identity-maps the first 32 MiB only. `SECRET_VADDR`
//!   is not mapped at any level of the attacker's PML4.
//!
//! The boot CPU then:
//!
//! 1. Switches to the *victim* CR3, reads `SECRET_VADDR`, asserts the
//!    byte is intact (proves the mapping was actually set up).
//! 2. Switches to the *attacker* CR3 and reads `SECRET_VADDR`.
//! 3. The CPU raises `#PF` (vector 14) with `error_code = 0` (page
//!    not-present, supervisor mode, read). The IDT routes the fault
//!    into `page_fault_handler` (below), which: (a) validates that the error
//!    code is exactly the not-present supervisor-mode read it expects
//!    (no other class of fault is acceptable); (b) validates that the
//!    *victim's* secret frame is still readable via its identity-mapped
//!    physical address — i.e. the attack did not corrupt the victim's
//!    data ("kernel must … keep the victim alive"); (c) reports success
//!    to QEMU via `isa-debug-exit`.
//!
//! Any other outcome (no fault, wrong fault, corrupted victim byte) is a
//! closed failure (`AGENTS.md` §5.4.5 — fail closed).

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_x86_64)]
use core::fmt::Write as _;
#[cfg(itest_x86_64)]
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

#[cfg(itest_x86_64)]
use rustos_arch_x86_64::{idt, paging, qemu_exit, serial};

/// Virtual address only the *victim* address space maps. Chosen well
/// outside the 32 MiB identity-mapped boot region so any access from
/// the *attacker* address space is guaranteed to be unmapped at every
/// level of its PML4 hierarchy.
#[cfg(itest_x86_64)]
const SECRET_VADDR: u64 = 0x4000_0000;

/// Magic byte written into the secret frame.
#[cfg(itest_x86_64)]
const SECRET_BYTE: u8 = 0xC0;

#[cfg(itest_x86_64)]
static PAGE_TABLE_POOL: paging::PageTablePool = paging::PageTablePool::new();

/// 4 KiB frame the victim space maps at `SECRET_VADDR`. Aligned via
/// `#[repr(align(4096))]` so its physical address is a valid page frame.
#[cfg(itest_x86_64)]
#[repr(C, align(4096))]
struct SecretFrame([u8; 4096]);

#[cfg(itest_x86_64)]
static mut SECRET_FRAME: SecretFrame = SecretFrame([0; 4096]);

/// `true` once the attacker context has been entered. Used by the page-
/// fault handler to distinguish an *expected* fault (attacker reading a
/// supposedly-isolated address) from a kernel bug (any other fault).
#[cfg(itest_x86_64)]
static ATTACKER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Physical address of the secret frame, recorded once after we set it
/// up. The fault handler reads it through this address to prove the
/// victim's data survived the attack.
#[cfg(itest_x86_64)]
static SECRET_PHYS: AtomicU64 = AtomicU64::new(0);

/// Entry point for the freestanding kernel. Called by
/// `rustos_arch_x86_64`'s boot trampoline after the multiboot magic
/// has been validated.
#[no_mangle]
#[cfg(itest_x86_64)]
pub extern "C" fn kernel_main(_multiboot_info: u64) -> ! {
    let mut com1 = serial::Serial::init(serial::COM1_BASE);
    let _ = writeln!(com1, "[memory_isolation] booted on x86_64");

    // SAFETY: `IDT` is installed exactly once on the boot CPU before any
    // exception can fire; `page_fault_handler` is `-> !` so re-entry is
    // impossible.
    unsafe { idt::init(page_fault_handler) };
    let _ = writeln!(com1, "[memory_isolation] idt installed");

    // ---- Build the victim address space and stash the secret byte. ----
    let secret_paddr = core::ptr::addr_of!(SECRET_FRAME) as u64;
    SECRET_PHYS.store(secret_paddr, Ordering::SeqCst);
    // SAFETY: SECRET_FRAME is a static mut owned exclusively by this
    // boot-time setup code; no other CPU exists yet (Stage-2 is single-
    // CPU) and the read in `page_fault_handler` happens after the store
    // completes. Use the raw pointer to avoid a `&mut` to the static.
    unsafe {
        let p = core::ptr::addr_of_mut!(SECRET_FRAME).cast::<u8>();
        p.write_volatile(SECRET_BYTE);
    }

    let Some(mut victim) = paging::AddressSpace::new_identity_first_32mib(&PAGE_TABLE_POOL) else {
        let _ = writeln!(com1, "[memory_isolation] FAIL: pool exhausted (victim)");
        qemu_exit::exit_failure();
    };
    if victim
        .map_4k(&PAGE_TABLE_POOL, SECRET_VADDR, secret_paddr, true)
        .is_none()
    {
        let _ = writeln!(com1, "[memory_isolation] FAIL: pool exhausted (mapping)");
        qemu_exit::exit_failure();
    }
    let victim_pml4 = victim.pml4_phys();
    let _ = writeln!(
        com1,
        "[memory_isolation] victim PML4 = 0x{victim_pml4:x}, secret_paddr = 0x{secret_paddr:x}"
    );

    let Some(attacker) = paging::AddressSpace::new_identity_first_32mib(&PAGE_TABLE_POOL) else {
        let _ = writeln!(com1, "[memory_isolation] FAIL: pool exhausted (attacker)");
        qemu_exit::exit_failure();
    };
    let attacker_pml4 = attacker.pml4_phys();
    let _ = writeln!(
        com1,
        "[memory_isolation] attacker PML4 = 0x{attacker_pml4:x}"
    );

    // ---- Phase 1: confirm the victim mapping is genuine. ----
    // SAFETY: we identity-map first 32 MiB in both address spaces, which
    // covers RIP and RSP. Switching to the victim is sound.
    unsafe { victim.switch() };
    // SAFETY: SECRET_VADDR is mapped read/write in the victim space.
    let v_byte = unsafe { core::ptr::read_volatile(SECRET_VADDR as *const u8) };
    if v_byte != SECRET_BYTE {
        let _ = writeln!(
            com1,
            "[memory_isolation] FAIL: victim observed wrong byte 0x{v_byte:x}"
        );
        qemu_exit::exit_failure();
    }
    let _ = writeln!(com1, "[memory_isolation] victim sees secret = 0x{v_byte:x}");

    // ---- Phase 2: switch to the attacker and read the same VA. ----
    ATTACKER_ACTIVE.store(true, Ordering::SeqCst);
    // SAFETY: identity map still covers RIP/RSP under the attacker.
    unsafe { attacker.switch() };
    let _ = writeln!(
        com1,
        "[memory_isolation] attacker about to read 0x{SECRET_VADDR:x} (expect #PF)"
    );

    // The next read MUST fault. If it returns, the kernel is broken.
    // SAFETY: we *want* the fault. The handler routes us to QEMU exit so
    // this volatile read never observably completes.
    let attacker_byte = unsafe { core::ptr::read_volatile(SECRET_VADDR as *const u8) };

    // Reaching this line is a failure: the CPU should have faulted.
    let _ = writeln!(
        com1,
        "[memory_isolation] FAIL: attacker read 0x{attacker_byte:x} without faulting"
    );
    qemu_exit::exit_failure();
}

/// IDT-registered `#PF` handler. The trap frame layout for `#PF` is:
///
/// ```text
/// rsp+0   error_code   (already popped into %rdi by the thunk)
/// rsp+8   rip          (already popped into %rsi by the thunk)
/// ```
///
/// The handler treats anything other than the expected supervisor-mode
/// not-present read at `SECRET_VADDR` (and from inside the attacker
/// context) as a kernel bug.
#[cfg(itest_x86_64)]
fn page_fault_handler(error_code: u64, rip: u64) -> ! {
    let mut com1 = serial::Serial::init(serial::COM1_BASE);
    let _ = writeln!(
        com1,
        "[memory_isolation] #PF: error=0x{error_code:x} rip=0x{rip:x}"
    );

    if !ATTACKER_ACTIVE.load(Ordering::SeqCst) {
        let _ = writeln!(
            com1,
            "[memory_isolation] FAIL: #PF before attacker switch — kernel bug"
        );
        qemu_exit::exit_failure();
    }

    // x86 #PF error code bits we care about:
    //  bit 0  P     — 0 = not present, 1 = protection violation
    //  bit 1  W/R   — 0 = read, 1 = write
    //  bit 2  U/S   — 0 = supervisor, 1 = user
    //  bit 3  RSVD  — reserved-bit violation
    //  bit 4  I/D   — instruction-fetch fault
    //
    // We require not-present (P=0), read (W/R=0), supervisor (U/S=0),
    // no reserved-bit violation, no instruction fetch. That is exactly
    // `error_code == 0`.
    if error_code != 0 {
        let _ = writeln!(
            com1,
            "[memory_isolation] FAIL: unexpected #PF error code 0x{error_code:x}"
        );
        qemu_exit::exit_failure();
    }

    // The fault must have come from our deliberate read of SECRET_VADDR.
    // We confirm the CR2 register (faulting linear address) below. We
    // *cannot* trust `rip` to point exactly at the load instruction
    // because the compiler chooses how to materialise the `read_volatile`,
    // so we use CR2 (architecturally guaranteed to hold the faulting LA).
    let cr2: u64;
    // SAFETY: `mov rax, cr2` is well-defined in ring 0.
    unsafe {
        core::arch::asm!("mov {x}, cr2", x = out(reg) cr2, options(nostack, preserves_flags));
    }
    if cr2 != SECRET_VADDR {
        let _ = writeln!(
            com1,
            "[memory_isolation] FAIL: CR2 was 0x{cr2:x}, expected 0x{SECRET_VADDR:x}"
        );
        qemu_exit::exit_failure();
    }

    // Final invariant: the victim's data must still be intact at its
    // *physical* address (i.e. the attack did not corrupt anything).
    let secret_paddr = SECRET_PHYS.load(Ordering::SeqCst);
    // SAFETY: identity-mapped first 32 MiB covers the static `SECRET_FRAME`,
    // so the physical address dereferences directly under either CR3.
    let victim_byte = unsafe { core::ptr::read_volatile(secret_paddr as *const u8) };
    if victim_byte != SECRET_BYTE {
        let _ = writeln!(
            com1,
            "[memory_isolation] FAIL: victim corrupted (saw 0x{victim_byte:x})"
        );
        qemu_exit::exit_failure();
    }

    let _ = writeln!(
        com1,
        "[memory_isolation] PASS: attacker faulted, victim intact (0x{victim_byte:x})"
    );
    qemu_exit::exit_success();
}

/// Panic handler for the freestanding binary. Reports failure to QEMU
/// rather than entering an infinite loop, so a buggy test never
/// silently hangs (`AGENTS.md` §7 — no flaky tests, strict timeouts).
#[panic_handler]
#[cfg(itest_x86_64)]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let mut com1 = serial::Serial::init(serial::COM1_BASE);
    let _ = writeln!(com1, "[memory_isolation] panic: {info}");
    qemu_exit::exit_failure();
}

// Host-target stubs. The crate is *only* meaningful on the bare-metal
// target; on the host we provide a no-op `main` so `cargo build` /
// `cargo test` against the host triple work for IDE indexing and so
// `cargo xtask ci` doesn't have to special-case this crate.
#[cfg(not(itest_x86_64))]
fn main() {}
#[cfg(not(itest_x86_64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
