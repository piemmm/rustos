//! `plans/ALIAS.md` §6.2 acceptance QEMU integration test: boot the
//! production aarch64 `tairix-kernel` pipeline on the `virt` board with the
//! planted encrypted-root image, log in as the seeded `root` account, and
//! prove the shell's **value pipe** reads a value-backed resource reference
//! end to end — while the *write* direction stays refused by the kernel.
//!
//! ## What this test asserts
//!
//! Three claims need a live machine, and they are why this vertical exists:
//! that a child holding no `CAP_SYSINFO_*` still reads a gated fact through
//! the shell's pipe, that a tool holding one reads the same fact as its own
//! operand, and that both manifests actually arm under `manifest ∩ ceiling`.
//! A unit test injects a transport and so spans none of them.
//!
//! The runner's ordered serial script (`tools/xtask`) types each line only
//! after its marker appeared:
//!
//! 1. `cat < info:system/machine-id` — an ungated value. The kernel reports
//!    the unprovisioned sentinel, so the value is sixteen zero bytes in
//!    lowercase hex: a deterministic marker.
//! 2. `cat < info:mem/page-size` — gated on `CAP_SYSINFO_KERNEL`. `4096` on
//!    the `virt` board, so its arrival proves the intersection armed the
//!    capability and `sysinfod` served the read.
//! 3. `cat < info:mem/physical && echo …` — the reference from the original
//!    defect report. Its value is machine-dependent, so the assertion is on
//!    `cat`'s exit status: `&&` runs the `echo` only if the read succeeded.
//! 4. `cat info:mem/physical && echo …` — the same reference as a bare
//!    **operand**, which `cat` resolves itself under its own manifest rather
//!    than reading a pipe the shell filled. The two readers are separate code
//!    paths, so both are exercised.
//! 5. `ls > info:mem/physical` — the write direction, still refused by the
//!    kernel resolver with `NotSupported`.
//! 6. `exit` — typed after the refusal message appeared.
//!
//! ## Why the PASS keys on "refusal, then exit"
//!
//! The audit sink arms on the dispatcher's `SYSCALL_HANDLER_REJECTED` record
//! carrying `sc=resource_open` and `err=NotSupported` — the same record the
//! original defect report showed — and passes on the **next** audited `exit`.
//! Exiting on the rejection itself would tear QEMU down inside the syscall,
//! before the shell printed the refusal and before the runner sent its last
//! line. The earlier `cat`/`ls` exits cannot false-trigger: the flag is not
//! armed until the rejection is seen, and only step 5 can produce one — the
//! value reads call the broker, never `resource_open`.
//!
//! The positive steps are asserted by the runner's own "every marker appeared
//! and every line was sent" rule, which is what makes a silently empty value
//! read fail rather than pass.
//!
//! The **denial** path — an account whose ceiling lacks the capability — is
//! covered by unit tests in `tairix_procinfo::valueread`, because this image
//! seeds one account and it holds the administrator ceiling.
//!
//! QEMU's `-kernel <ELF>` aarch64 path passes no DTB pointer, so the
//! canonical `virt` device tree is dumped and embedded at build time
//! (`build.rs`). The test reuses the entire production boot pipeline and only
//! replaces the audit sink; keeping the QEMU-exit shortcut in a dedicated bin
//! rather than a feature on a production crate stops feature unification
//! leaking it into a production build.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use tairix_abi::{Errno, FieldValue};
    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, EventId, Sink};
    use tairix_util::fmt::format_i32;

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

    /// `EventId` emitted by the syscall dispatcher when the owning subsystem
    /// rejected a call — here `resource_open` refusing a *write* of a
    /// value-backed reference. Pinned by the audit-id test in
    /// `kernel/syscall/src/audit.rs`.
    const SYSCALL_HANDLER_REJECTED_EVENT_ID: EventId = EventId(5004);

    /// Set once the audited `resource_open` rejection with `NotSupported`
    /// has been observed — the kernel-side witness that a value-backed
    /// reference is still not writable. The PASS finisher fires on the next
    /// audited `exit`.
    static WRITE_REFUSED: AtomicBool = AtomicBool::new(false);

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

    /// Sink that replays every event through [`SERIAL_SINK`] and reports PASS
    /// once the write refusal has been observed and the shell's subsequent
    /// `exit` dispatches (the module docs say why it is deferred).
    struct ValuePipeSink;

    impl Sink for ValuePipeSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replayed so the transcript records the whole timeline.
            SerialSink::new().write_event(event);
            if event.id == SYSCALL_HANDLER_REJECTED_EVENT_ID
                && field_str(event, "sc") == Some("resource_open")
                && err_field_is(event, Errno::NotSupported)
            {
                WRITE_REFUSED.store(true, Ordering::Release);
            } else if event.id == SYSCALL_INVOKED_EVENT_ID
                && field_str(event, "sc") == Some("exit")
                && WRITE_REFUSED.load(Ordering::Acquire)
            {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: ValuePipeSink = ValuePipeSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_value_pipe_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt`
    /// blob's address is forwarded to the production boot pipeline with the
    /// audit-observer sink in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &ALLOCATOR,
            &SERIAL_SINK,
            &AUDIT_SINK,
            // `SyscallInvoked` (`EventId(5000)`) is `Debug`, below the
            // default `Info` filter; this observer watches it, so boot
            // with the filter lowered.
            tairix_log::Level::Debug,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

#[cfg(not(itest_aarch64))]
fn main() {}
