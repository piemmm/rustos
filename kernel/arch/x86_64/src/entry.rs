//! `extern "C" fn tairix_arch_x86_64_main` — the Rust side of the boot
//! trampoline.
//!
//! The 32-bit assembly in `boot.s` finishes by `call`ing this symbol with
//! the boot magic in `%rdi` and the boot-info pointer in `%rsi`
//! (System V AMD64 ABI, see `boot.s` SAFETY-INVARIANT 7). The magic is
//! either the multiboot2 bootloader magic (GRUB entered `_start`) or the
//! PVH start-info magic (QEMU's `-kernel` ELF loader entered
//! `pvh_start`).
//!
//! This function validates the magic, records which protocol delivered
//! the boot info (`crate::bootinfo`), and then transfers control to a
//! binary-supplied `extern "C" fn kernel_main() -> !`. Every test binary
//! defines that symbol exactly once.

use crate::{bootinfo, pic, pio, pvh, qemu_exit, MULTIBOOT2_BOOTLOADER_MAGIC};

extern "C" {
    /// Provided by the test binary. Must not return.
    ///
    /// The single `boot_info` parameter carries the verbatim 64-bit
    /// pointer the loader passed in `%ebx`. A binary that does not need
    /// to inspect the boot info (e.g.
    /// `tests/integration/memory_isolation`) can simply ignore it; a
    /// binary that does need it (e.g.
    /// `tests/integration/scheduler_stress_qemu`) parses it via
    /// `crate::bootinfo::BootData::load` once it has identity-mapped
    /// access to that address — `boot.s` SAFETY-INVARIANT 4 guarantees
    /// the first 4 GiB of physical memory are reachable.
    fn kernel_main(boot_info: u64) -> !;
}

/// The trampoline jumps here. Called *exactly once* on the boot CPU.
///
/// # Behaviour
///
/// 1. Validates the boot magic against the two protocols the trampoline
///    accepts; anything else is a closed-fail (validate every input).
/// 2. Records the protocol in [`crate::bootinfo`] so
///    [`crate::bootinfo::BootData::load`] can dispatch without
///    re-guessing; a second record attempt is a boot-path defect and
///    also fails closed.
/// 3. Quiesces the legacy 8259 PICs ([`crate::pic`]): a BIOS-style
///    hand-off (`SeaBIOS` in front of PVH direct boot, or a real
///    legacy-boot machine) leaves them unmasked at vector base 8, where
///    the first PIT tick taken with `IF=1` would be decoded as `#DF`.
/// 4. Transfers to the binary-supplied `kernel_main`.
///
/// IDT installation is deferred to `kernel_main` because each test
/// installs its *own* page-fault handler. This avoids the alternative
/// (a kernel-side IDT that has to be re-pointed) which would touch a
/// `static mut` from two call sites — forbidden by.
///
/// # Safety
///
/// Implicitly safe to call from the asm trampoline because the
/// invariants in `boot.s` are upheld. Calling from anywhere else is a
/// kernel bug.
#[no_mangle]
pub extern "C" fn tairix_arch_x86_64_main(magic: u64, boot_info: u64) -> ! {
    // The magic arrives in `%rdi` zero-extended from the 32-bit value
    // the entry stub placed in `%edi`. Only the low 32 bits carry the
    // magic; the truncation is the documented 32-bit entry ABI of both
    // protocols.
    let magic32 = u32::try_from(magic & 0xFFFF_FFFF).unwrap_or(0);
    let protocol = match magic32 {
        MULTIBOOT2_BOOTLOADER_MAGIC => bootinfo::BootProtocol::Multiboot2,
        pvh::PVH_BOOT_MAGIC => bootinfo::BootProtocol::Pvh,
        // Entered by something other than the two supported loaders;
        // fail closed.
        _ => qemu_exit::exit_failure(),
    };
    if bootinfo::record(protocol).is_err() {
        // The boot path runs exactly once; a second record is a defect.
        qemu_exit::exit_failure();
    }
    pic::remap_and_mask_all(&pio::x86_port_io8());
    // SAFETY: `kernel_main` is provided by the linked test binary and is
    // documented as `-> !` (see `extern` block above). Calling it once
    // with the verbatim boot-info pointer is the entire contract.
    unsafe { kernel_main(boot_info) }
}
