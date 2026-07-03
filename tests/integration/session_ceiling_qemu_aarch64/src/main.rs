//! `plans/CAPABILITY_USE.md` CU3 acceptance QEMU integration test: boot the
//! production aarch64 `rustos-kernel` pipeline on the `virt` board with the
//! planted whole-disk encrypted-root image, log in as the seeded `root`
//! account, and drive the spawned shell through a real session under the
//! account's **administrator capability ceiling** — then prove the
//! `user ceiling ∩ manifest request` intersection still binds the shell.
//!
//! ## What this test asserts
//!
//! The production boot path discovers and binds the virtio-blk root,
//! admits the in-kernel unlock kthread, and (once the runner types the
//! fixture passphrase at `Root passphrase: `) mounts the encrypted `RustFS`
//! root and installs the users database. PID 1's console login then
//! prompts; the runner authenticates `root`/`root` against the planted
//! account — whose grant is the shared administrator ceiling
//! (`rustos_users::administrator_ceiling`, the same set
//! `tools/mkimage::debug_users_db` seeds a debug image with) — and login
//! spawns the account's shell **as that user** through `spawn_as`. The
//! runner's ordered serial script (`tools/xtask`) then holds a real
//! session with the shell, each line typed only after its marker appeared:
//!
//! 1. `cd /Users/root` — the shell's effective set is
//!    `SESSION_BASELINE ∩ administrator ceiling = SESSION_BASELINE`, so
//!    `CAP_FS_ACCESS` admits `fs_chdir` and the secured VFS authorises the
//!    account's home directory (the B3 regression: before CU3 the seeded
//!    grant omitted `CAP_FS_ACCESS` and every filesystem call was denied).
//! 2. `pwd` — prints `/Users/root`, proving the chdir actually moved the
//!    process.
//! 3. `/Apps/Ps.app/Run` — `CAP_PROC_SPAWN` admits `spawn`; `ps` runs as
//!    the same user, queries its self-scoped process list through
//!    `sysinfod`, prints the shared `PID  PPID …` header, and exits.
//! 4. `ulimit -H processes 1000` — **lowering** a hard bound needs no
//!    capability and succeeds.
//! 5. `ulimit -H processes 2000` — **raising** the hard bound needs
//!    `CAP_RLIMIT_RAISE`. The administrator *ceiling* carries it, but the
//!    shell's *manifest* (the session baseline) does not request it, so
//!    the effective set lacks it: the kernel refuses the `rlimit_set` with
//!    `PermissionDenied` (fail closed) and the shell prints the denial.
//!    This is the negative half: holding an administrator account does not
//!    widen any one program past its own manifest.
//! 6. `exit` — typed after the denial message appeared, ending the shell.
//!
//! ## Why the PASS keys on "denial, then exit"
//!
//! The audit sink arms on the dispatcher's `SYSCALL_HANDLER_REJECTED`
//! record (`EventId(5004)`) carrying `sc=rlimit_set` and
//! `err=PermissionDenied` — the kernel-side witness that the raise was
//! refused by the capability check, not by an argument error — and reports
//! PASS on the **next** audited `exit` dispatch (`EventId(5000)`,
//! `sc=exit`). Exiting on the rejection itself would tear QEMU down inside
//! the syscall, before the shell printed the denial and before the runner
//! sent the final scripted line; keying on the exit that only the runner's
//! post-denial `exit` command produces guarantees the denial reached the
//! transcript. `ps`'s own earlier `exit` cannot false-trigger: the flag is
//! not armed until the rejection has been seen. The runner additionally
//! fails the run if the guest exits before every scripted marker appeared
//! and every line was sent, so a session that dies mid-dialogue cannot
//! pass. A run where the root never mounts, login never authenticates, the
//! shell is denied `fs_chdir` (the B3 defect), `ps` never spawns, or the
//! raise is *not* refused never reaches the armed exit, so the harness
//! times out — the documented fail-loud behaviour.
//!
//! ## Embedded `virt` device tree
//!
//! QEMU's `-kernel <ELF>` aarch64 path passes no DTB pointer (`x0 = 0`),
//! so the canonical `virt` device tree is dumped and embedded at build
//! time (`build.rs`) and its address handed to the boot pipeline, which
//! discovers the board from it exactly as it would from real firmware.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline and only replaces
//! the audit sink. Splitting the audit-observer behaviour into a separate
//! bin (instead of a Cargo feature on a production crate) prevents feature
//! unification from leaking the QEMU-exit shortcut into any production
//! build (fail closed; the harness never decides what the kernel does
//! next).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use rustos_abi::{Errno, FieldValue};
    use rustos_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use rustos_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use rustos_kernel::aarch64::boot as boot_aarch64;
    use rustos_log::{Event, EventId, Sink};
    use rustos_util::fmt::format_i32;

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap, mirroring the production aarch64 kernel binary's
    /// `.bss`-resident heap (zeroed by the boot trampoline).
    ///
    /// `static mut` because the bump allocator hands out disjoint slices via
    /// an atomic cursor; the storage is otherwise never aliased.
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`].
    ///
    /// SAFETY: the page-aligned `HEAP` static outlives the binary and the
    /// allocator is its only consumer.
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// `EventId` emitted by the syscall dispatcher for an audited syscall
    /// that passed every check. Pinned by the audit-id test in
    /// `kernel/syscall/src/audit.rs`.
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    /// `EventId` emitted by the syscall dispatcher when the owning
    /// subsystem rejected a call after the dispatcher checks passed —
    /// here, `rlimit_set` refusing a hard-bound raise from a caller whose
    /// effective set lacks `CAP_RLIMIT_RAISE`. Pinned by the audit-id test
    /// in `kernel/syscall/src/audit.rs`.
    const SYSCALL_HANDLER_REJECTED_EVENT_ID: EventId = EventId(5004);

    /// Set once the audited `rlimit_set` rejection with `PermissionDenied`
    /// has been observed — the kernel-side witness of the scripted raise
    /// denial. The PASS finisher fires on the next audited `exit`.
    static RAISE_DENIED: AtomicBool = AtomicBool::new(false);

    /// The string value of `event`'s field `key`, if present.
    fn field_str<'e>(event: &Event<'e>, key: &str) -> Option<&'e str> {
        event.fields.iter().find_map(|field| {
            if field.key == key {
                match field.value {
                    FieldValue::Str(s) => Some(s),
                    _ => None,
                }
            } else {
                None
            }
        })
    }

    /// `true` if `event` carries the dispatcher's `err` field with exactly
    /// `errno`'s decimal value — the same `format_i32` rendering the
    /// dispatcher itself writes, so the comparison cannot drift from the
    /// producer.
    fn err_field_is(event: &Event<'_>, errno: Errno) -> bool {
        let mut buf = [0u8; 12];
        let expected = format_i32(errno.as_i32(), &mut buf);
        field_str(event, "err") == Some(expected)
    }

    /// Sink that replays every event through [`SERIAL_SINK`] and reports
    /// PASS once the scripted session's raise denial has been observed and
    /// the shell's subsequent `exit` dispatches (see the module docs for
    /// why the PASS is deferred to the exit).
    struct SessionCeilingSink;

    impl Sink for SessionCeilingSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records
            // the full boot + session timeline.
            SerialSink::new().write_event(event);
            if event.id == SYSCALL_HANDLER_REJECTED_EVENT_ID
                && field_str(event, "sc") == Some("rlimit_set")
                && err_field_is(event, Errno::PermissionDenied)
            {
                RAISE_DENIED.store(true, Ordering::Release);
            } else if event.id == SYSCALL_INVOKED_EVENT_ID
                && field_str(event, "sc") == Some("exit")
                && RAISE_DENIED.load(Ordering::Acquire)
            {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: SessionCeilingSink = SessionCeilingSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn rustos_session_ceiling_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `rustos_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt`
    /// blob's address is forwarded to the production boot pipeline with the
    /// audit-observer sink in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &SERIAL_SINK,
            &AUDIT_SINK,
            &rustos_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
