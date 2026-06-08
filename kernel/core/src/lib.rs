//! RustOS microkernel core (Stage 2.6 of `PLAN.md`).
//!
//! `kernel/core` ships the **architecture-neutral kernel entry point,
//! init sequencing, and panic policy**. Architecture crates
//! (`kernel/arch/*`, Stage 3) build a [`BootInfo`] from whatever
//! protocol the platform exposes (multiboot2, UEFI, DTB,
//! `wasm-bindgen`, …) and hand it to [`kernel_main`]; everything else
//! in this crate exists to support that hand-off.
//!
//! # Module map
//!
//! | Module          | Role                                                   |
//! | --------------- | ------------------------------------------------------ |
//! | [`aspace`]      | Per-task address-space registry for the copy path.     |
//! | [`audit`]       | Stable audit-event IDs in the `4_000..5_000` range.    |
//! | [`bootinfo`]    | [`BootInfo`] hand-off type + [`KernelArch`] trait.     |
//! | [`init`]        | [`kernel_main`] and the documented init order.         |
//! | [`mod@panic`]   | [`handle_panic`] and [`PanicContext`].                 |
//!
//! # Init order
//!
//! [`kernel_main`] runs the following phases in this exact order:
//!
//! 1. **log** — install the global level filter.
//! 2. **mem** — build the physical [`rustos_kernel_mem::FrameAllocator`].
//! 3. **sec** — verify the bootstrap [`rustos_kernel_sec::IdentityTable`].
//! 4. **sched** — construct the SMP [`crate::sched::Scheduler`] (the
//!    build-time-selected scheduler policy, `AGENTS.md` §17.1).
//! 5. **ipc** — `kernel/ipc` has no global state at this stage; the
//!    phase event still fires so external log consumers see a uniform
//!    boot timeline.
//!
//! Stage 2.7 will extend the chain with a syscall-registration phase
//! and replace the trailing [`KernelArch::halt`] with the scheduler
//! dispatch loop.
//!
//! # Panic policy
//!
//! The arch port owns the `#[panic_handler]` attribute and delegates
//! to [`handle_panic`] (see the example in [`mod@panic`] module docs).
//! The handler logs one [`audit::AuditEvent::Panic`] record carrying
//! the failing CPU id, file, line, and column, then calls
//! [`KernelArch::halt`]. It **never** silently resets — `AGENTS.md`
//! §2 / Stage 2 deliverables.
//!
//! # No global mutable static
//!
//! Per `AGENTS.md` §2, *"No global mutable static beyond the per-CPU
//! bootstrap area"*. `kernel/core` declares **zero** global mutable
//! statics: every subsystem the kernel constructs lives on
//! [`kernel_main`]'s stack inside `KernelState`, which never escapes
//! the entry function. The per-CPU bootstrap area itself lives in the
//! arch crates (`kernel/arch/*`, Stage 3) and is reviewed there.
//!
//! # Documentation
//!
//! Architectural detail (entry contract, init order, panic policy,
//! `BootInfo` schema) is in `docs/src/architecture/kernel.md`. Public
//! items here carry rustdoc per `AGENTS.md` §13.

#![no_std]
#![forbid(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

extern crate alloc;

// Host tests need `std` for `Box::leak`, `catch_unwind`, and
// `String::from_utf8`. The crate itself remains `no_std` for
// production builds (`AGENTS.md` §1 — no hacks).
#[cfg(test)]
extern crate std;

pub mod aspace;
pub mod audit;
pub mod bootinfo;
pub mod console;
pub mod dispatch_slot;
pub mod fs;
pub mod init;
pub mod kthread;
pub mod memmap;
pub mod panic;
pub mod procwait;
pub mod random;
// The single scheduler selection point (`AGENTS.md` §17.1). Internal:
// the concrete policy must not leak to crates that should depend on the
// `rustos_kernel_sched_api` contract instead.
pub(crate) mod sched;
pub mod spawn;
pub mod syscalls;

#[cfg(any(test, feature = "test-arch"))]
pub mod test_arch;
#[cfg(any(test, feature = "test-arch"))]
pub mod test_sink;

pub use aspace::{AddressSpaceRegistry, AspaceError};
pub use audit::AuditEvent;
pub use bootinfo::{BootInfo, BootInfoError, IrqRouting, KernelArch, MAX_COMMAND_LINE_BYTES};
pub use console::{
    ConsoleRead, ConsoleWrite, NullConsole, NullConsoleRead, NULL_CONSOLE, NULL_CONSOLE_READ,
};
pub use dispatch_slot::{
    AlreadyInstalledError, DispatchCallbackSlot, DispatchHook, DispatchOutcome, RescheduleAction,
};
pub use fs::{
    Access, AclEntry, AclWho, Credentials, Metadata, Mode, MountPoint, MountTable, Path, Vfs,
    VfsError,
};
pub use init::{kernel_main, InitError, Phase};
pub use kthread::{
    reschedule_current, spawn_kthread, spawn_kthread_with_stack, spawn_user_kthread,
    spawn_user_kthread_with_stack, BoxStack, KernelStack, Yielder, KTHREAD_MAX_CPUS,
    KTHREAD_STACK_BYTES,
};
pub use memmap::{MemMap, NullMemMap, NULL_MEM_MAP};
pub use panic::{handle_panic, panic_dump, PanicContext};
pub use procwait::{NullProcessWait, ProcessWait, ReapedChild, NULL_PROCESS_WAIT};
pub use random::{reserve_errno, BootReserve, NullEntropy, RandomReserve};
pub use spawn::{
    spawn_and_enter, spawn_image, AdmitError, EmbeddedProgram, InitSpawn, InitSpawnCtx,
    NullProcessSpawn, ProcessSpawn, ProgramRegistry, SpawnCallerError, SpawnCtx, SpawnRequest,
    EMPTY_PROGRAM_REGISTRY, NULL_PROCESS_SPAWN,
};
pub use syscalls::{KernelDispatchHook, KernelSyscallHandlers};
