//! User-mode entry surface of the Arch HAL.
//!
//! Dropping a freshly built process image into user mode (U-mode on
//! riscv64, EL0 on aarch64, ring 3 on x86_64) is a privilege transition
//! only the architecture port can perform: it requires the
//! port-specific instruction (`sret` / `eret` / `iretq`) and the
//! port-specific control-register state (`sstatus` / `SPSR_EL1` /
//! the `iretq` frame). The charter makes the architecture surface a closed set
//! of traits on the HAL; this module is the "enter user mode" member of
//! that set, so the `sret`/`eret`/`iretq` sequence lives in exactly one
//! place per port instead of being copied into every call site.
//!
//! # What lives here
//!
//! * [`UserEntry`] — the architecture-neutral register state a process
//!   image is entered with: the entry-point virtual address, the initial
//!   user stack pointer, and the value of the first-argument register
//!   (the kernel hands a freshly spawned process the address of its
//!   `rustos_abi::process` startup-vector block there). This mirrors the
//!   `ProcessImage` the kernel-side image builder
//!   (`kernel/mem/src/spawn.rs`) produces.
//! * [`EnterUser`] — the per-port handle the kernel reaches through. Its
//!   single method consumes a [`UserEntry`] and diverges into user mode.
//!
//! # Why no host conformance vertical
//!
//! Unlike the side-channel and memory-tagging surfaces, "enter user
//! mode" has no declarative profile and cannot be exercised from a
//! host-run unit test: the transition is only meaningful on the
//! bare-metal target, where it cannot be observed from `cargo test`
//! (the call never returns). The portable, host-testable part is the
//! [`UserEntry`] value, which is pinned by the in-module tests below;
//! the transition itself is proven end-to-end by each port's QEMU
//! round-trip (a program is entered at a `UserEntry` and its syscalls
//! are observed kernel-side — `plans/CCOMPAT.md` CC3). Inventing a
//! host stub that "enters user mode" would be a fake primitive.

/// The architecture-neutral register state a process image is entered
/// with.
///
/// Produced by the kernel-side process-image builder
/// (`kernel/mem`'s `ProcessImage`) and consumed by [`EnterUser::enter_user`].
/// Every field is a user virtual address or a register value in the
/// freshly built address space:
///
/// * [`Self::entry`] — the (relocated) entry-point virtual address the
///   port jumps to in user mode.
/// * [`Self::stack_pointer`] — the initial user stack pointer (the
///   exclusive top of the mapped user stack).
/// * [`Self::arg0`] — the value placed in the first-argument register
///   (`a0` on riscv64, `x0` on aarch64, `rdi` on x86_64). The kernel
///   passes the user address of the `rustos_abi::process` startup-vector
///   block here so the program's startup object can find its arguments.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct UserEntry {
    /// Relocated entry-point virtual address.
    pub entry: u64,
    /// Initial user stack pointer (exclusive top of the user stack).
    pub stack_pointer: u64,
    /// Value of the first-argument register on entry (the startup-vector
    /// block address).
    pub arg0: u64,
}

impl UserEntry {
    /// Construct a [`UserEntry`] from its three register values.
    #[must_use]
    pub const fn new(entry: u64, stack_pointer: u64, arg0: u64) -> Self {
        Self {
            entry,
            stack_pointer,
            arg0,
        }
    }
}

/// The "enter user mode" handle an architecture port exposes.
///
/// The kernel calls [`Self::enter_user`] once a process image has been
/// built (segments mapped and filled, user stack mapped, startup-vector
/// block written) to transfer control to the new program. The port
/// performs the privilege transition with its native instruction
/// (`sret` / `eret` / `iretq`) and never returns.
///
/// Implementations must be [`Send`] + [`Sync`]: the kernel reaches the
/// handle from every CPU. A port's handle is typically zero-sized — the
/// transition needs no per-instance state.
pub trait EnterUser: Send + Sync {
    /// Drop the calling CPU into user mode at `regs`, never returning.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that, in the address space active on
    /// the calling CPU:
    ///
    /// * [`UserEntry::entry`] is a user-accessible, executable virtual
    ///   address;
    /// * [`UserEntry::stack_pointer`] is a user-accessible, writable
    ///   stack top;
    /// * the kernel's user→kernel trap path (syscall vector / exception
    ///   vector) is installed, so a syscall from the new program is
    ///   handled rather than faulting into an unconfigured state.
    ///
    /// The transition diverges: control passes to user mode and never
    /// returns to the caller.
    unsafe fn enter_user(&self, regs: UserEntry) -> !;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_entry_preserves_its_register_values() {
        let regs = UserEntry::new(0x1000, 0x8000, 0xdead_beef);
        assert_eq!(regs.entry, 0x1000);
        assert_eq!(regs.stack_pointer, 0x8000);
        assert_eq!(regs.arg0, 0xdead_beef);
        // The constructor and the struct literal agree.
        assert_eq!(
            regs,
            UserEntry {
                entry: 0x1000,
                stack_pointer: 0x8000,
                arg0: 0xdead_beef,
            }
        );
    }

    /// A trivial port double: the trait must be object-safe so the
    /// kernel can reach a port's handle through `&dyn EnterUser`. The
    /// transition itself is never executed on the host (it is only
    /// meaningful on the bare-metal target); this pins object safety and
    /// the `Send + Sync` bound.
    struct NeverEnter;

    impl EnterUser for NeverEnter {
        unsafe fn enter_user(&self, _regs: UserEntry) -> ! {
            // Never reached on the host; the real ports emit the
            // privilege-transition instruction here.
            unreachable!("enter_user is only meaningful on the bare-metal target")
        }
    }

    fn assert_send_sync<T: Send + Sync>(_: &T) {}

    #[test]
    fn enter_user_is_object_safe_and_thread_safe() {
        let port = NeverEnter;
        let _: &dyn EnterUser = &port;
        assert_send_sync(&port);
    }
}
