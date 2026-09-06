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
//!   `tairix_abi::process` startup-vector block there). This mirrors the
//!   `ProcessImage` the kernel-side image builder
//!   (`kernel/mem/src/spawn.rs`) produces.
//! * [`EnterUser`] — the per-port handle the kernel reaches through. Its
//!   single method consumes a [`UserEntry`] and diverges into user mode.
//!
//! # The inbound counterpart
//!
//! [`note_user_entry`] is the same boundary in the other direction: the
//! port reports the register state a user thread is *entering the kernel*
//! with, which only the port can read (the values live in whatever frame
//! its trap entry saved). It exists so a diagnostic can attribute a user
//! call stack to the thread that made the call; the observer is installed
//! by the kernel, and a port whose kernel installed none pays a single
//! relaxed load.
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
///   passes the user address of the `tairix_abi::process` startup-vector
///   block here so the program's startup object can find its arguments.
/// * [`Self::tls_base`] — the initial *thread pointer*, the per-thread
///   register the psABI reserves for thread-local storage
///   (`TPIDR_EL0` on aarch64, `tp` on riscv64, `IA32_FS_BASE` on
///   x86_64). `0` leaves the thread without one.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct UserEntry {
    /// Relocated entry-point virtual address.
    pub entry: u64,
    /// Initial user stack pointer (exclusive top of the user stack).
    pub stack_pointer: u64,
    /// Value of the first-argument register on entry (the startup-vector
    /// block address).
    pub arg0: u64,
    /// Initial thread-pointer value, or `0` for none.
    ///
    /// Each port programs its own psABI thread-pointer register from this
    /// (`plans/THREADS.md` decision 7). Two of the three registers are
    /// architecturally *user-writable*, so on those the value is only a
    /// seed: the thread may replace it, and the port's trap frame — which
    /// lives on that thread's own kernel stack — carries it across every
    /// trap and context switch, which is what makes it per-thread. On
    /// x86_64 the register is privileged, so the kernel holds the value and
    /// reloads it before each switch into the thread.
    pub tls_base: u64,
}

impl UserEntry {
    /// Construct a [`UserEntry`] from its four register values.
    #[must_use]
    pub const fn new(entry: u64, stack_pointer: u64, arg0: u64, tls_base: u64) -> Self {
        Self {
            entry,
            stack_pointer,
            arg0,
            tls_base,
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
    /// [`UserEntry::tls_base`] needs no such guarantee: the thread pointer
    /// is an opaque register value the entered thread interprets itself, so
    /// an unmapped or nonsensical value can only fault that thread's own
    /// accesses.
    ///
    /// The transition diverges: control passes to user mode and never
    /// returns to the caller.
    unsafe fn enter_user(&self, regs: UserEntry) -> !;
}

/// The signature of the user-entry observer: the entering CPU's
/// [`CpuId`](crate::CpuId), the interrupted user program counter and
/// frame-pointer register, and whether that frame-pointer register is one
/// the port actually saved.
///
/// Three scalars rather than a
/// [`UserRegisterFrame`](crate::backtrace::UserRegisterFrame): this fires
/// on every user->kernel entry, so it carries only what a frame-pointer
/// walk needs — the chain's root and the pc that names its top. The walk's
/// bounds come from the thread's own stack span, so the stack pointer is
/// not among them, and a consumer that needs the whole register file has
/// the fault path's frame instead.
pub type UserEntryObserverFn = extern "C" fn(crate::CpuId, u64, u64, bool);

/// The installed observer as a raw function pointer (`0` = none).
static USER_ENTRY_OBSERVER: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

/// Install the user-entry observer every port reports through.
///
/// Storing a `fn` (not a closure) keeps it safe to call from trap
/// context: there is no captured environment to drop mid-flight.
pub fn set_user_entry_observer(cb: UserEntryObserverFn) {
    USER_ENTRY_OBSERVER.store(cb as usize, core::sync::atomic::Ordering::Relaxed);
}

/// The installed observer, if any.
///
/// A port checks this *before* reading its saved frame, so an image whose
/// kernel installed no observer pays one relaxed load per entry and never
/// touches the frame.
#[must_use]
pub fn user_entry_observer() -> Option<UserEntryObserverFn> {
    let raw = USER_ENTRY_OBSERVER.load(core::sync::atomic::Ordering::Relaxed);
    if raw == 0 {
        None
    } else {
        // SAFETY: every store into `USER_ENTRY_OBSERVER` round-trips a valid
        // `UserEntryObserverFn` through `set_user_entry_observer`.
        Some(unsafe { core::mem::transmute::<usize, UserEntryObserverFn>(raw) })
    }
}

/// Report the register state `cpu` is entering the kernel from user mode
/// with, to the installed observer.
///
/// `fp_valid` is the port's honest statement about its own trap frame: a
/// port that does not save the frame-pointer register passes `false` so no
/// consumer walks a chain from a register it never held.
pub fn note_user_entry(cpu: crate::CpuId, pc: u64, fp: u64, fp_valid: bool) {
    if let Some(observe) = user_entry_observer() {
        observe(cpu, pc, fp, fp_valid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_entry_preserves_its_register_values() {
        let regs = UserEntry::new(0x1000, 0x8000, 0xdead_beef, 0x7f00_0000);
        assert_eq!(regs.entry, 0x1000);
        assert_eq!(regs.stack_pointer, 0x8000);
        assert_eq!(regs.arg0, 0xdead_beef);
        assert_eq!(regs.tls_base, 0x7f00_0000);
        // The constructor and the struct literal agree.
        assert_eq!(
            regs,
            UserEntry {
                entry: 0x1000,
                stack_pointer: 0x8000,
                arg0: 0xdead_beef,
                tls_base: 0x7f00_0000,
            }
        );
    }

    /// Two threads of one process differ only in the stack and thread
    /// pointer: the entry address and argument register are the caller's
    /// choice, but the thread pointer is what makes thread-local storage
    /// per-thread rather than per-process.
    #[test]
    fn threads_of_one_process_carry_distinct_thread_pointers() {
        let first = UserEntry::new(0x1000, 0x8000, 0, 0x7f00_0000);
        let second = UserEntry::new(0x1000, 0x9000, 0, 0x7f01_0000);
        assert_eq!(first.entry, second.entry);
        assert_ne!(first.tls_base, second.tls_base);
        assert_ne!(first.stack_pointer, second.stack_pointer);
    }

    /// A thread with no thread-local storage is spelled `0`, so a port can
    /// tell "no thread pointer" from a real address without a second flag.
    #[test]
    fn a_thread_without_thread_local_storage_carries_zero() {
        assert_eq!(UserEntry::new(0x1000, 0x8000, 0, 0).tls_base, 0);
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

    /// The observed entry, so the installed-observer round trip is
    /// checkable without a trap.
    static OBSERVED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

    extern "C" fn record(_cpu: crate::CpuId, pc: u64, _fp: u64, _fp_valid: bool) {
        OBSERVED.store(pc, core::sync::atomic::Ordering::Relaxed);
    }

    #[test]
    fn an_entry_reaches_the_installed_observer_and_is_a_no_op_without_one() {
        // Before any install the report must be inert rather than a null
        // call: a port reports on every entry, including on an image whose
        // kernel wants no observer.
        note_user_entry(0, 0x1111, 0, false);
        assert_eq!(OBSERVED.load(core::sync::atomic::Ordering::Relaxed), 0);

        set_user_entry_observer(record);
        assert!(user_entry_observer().is_some());
        note_user_entry(0, 0xfeed, 0x2100, true);
        assert_eq!(OBSERVED.load(core::sync::atomic::Ordering::Relaxed), 0xfeed);
    }
}
