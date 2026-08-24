//! `plans/STRESSTEST.md` ST4 QEMU integration test: boot the *production*
//! aarch64 `tairix-kernel` pipeline on the `virt` board with the planted
//! whole-disk encrypted-root image, log in as the seeded `root` account,
//! start **`sysmon`** on the console, drive a refresh, watch the rendered
//! kernel-statistics figures on the transcript, and quit back to an intact
//! shell prompt.
//!
//! ## What this test asserts
//!
//! The production boot path unlocks and mounts the encrypted `ARXFS`
//! root, login authenticates `root`/`root`, and the session shell runs the
//! runner's ordered script:
//!
//! 1. `sysmon` — the store bundle spawns through the full signature +
//!    capability + interface-hash load gate; the monitor pins itself (`mem_pin`
//!    under the granted `CAP_MEM_PIN`), enters the alternate screen, and paints
//!    its first frame. The `Pres` gauge label on the transcript witnesses the
//!    gated `MEMORY_PRESSURE` figures rendered; the runner then types `r` (an
//!    immediate refresh — the `plans/STRESSTEST.md` §6 keys work over the raw
//!    console) and waits for `hit%` (the cache-hit-ratio column header of the
//!    default `RECLAIM_STATS` ledger panel).
//! 2. `q` — the monitor quits, leaves the alternate screen (restoring the
//!    covered shell content), and the shell prompt reappearing at all is
//!    the intact-screen witness: a monitor that died, hung, or wedged the
//!    console line discipline would never show it.
//! 3. `exit` — typed only after the prompt reappeared.
//!
//! ## Why the PASS keys on `sysmon`'s exit *then* the shell's exit
//!
//! The kernel-side witness is `sysmon`'s audited `exit` (`SyscallInvoked`,
//! `EventId(5000)`, `sc=exit`, `comm=sysmon`) — which only lands after the
//! whole interactive session ran. Exiting QEMU there would tear the run
//! down before the runner observed the restored prompt and sent its final
//! line, so the sink only *arms* on it and reports PASS on the **next**
//! audited `exit` — the shell's, typed only after the prompt reappeared —
//! so the verified frames provably reached the transcript before the run
//! ended (the session-ceiling arm-then-exit discipline). The runner
//! additionally fails the run if the guest exits before every scripted
//! marker appeared and every line was sent. A refused spawn, a denied
//! query rendered as a refusal the markers do not match, or a hung loop
//! never reaches the armed exit: the run times out with the failing step
//! in the serial transcript — the documented fail-loud behaviour.
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

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, EventId, FieldValue, Sink};

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

    /// The scripted session's tool under test. Its audited `exit` is the
    /// kernel-side witness that the whole interactive monitor session —
    /// spawn, pin, first frame, refresh, panel render, quit — ran to
    /// completion.
    const MONITOR_COMM: &str = "sysmon";

    /// Set once `sysmon`'s audited `exit` has been observed. The PASS
    /// finisher fires on the next audited `exit`: the shell's, typed by
    /// the runner only after the restored prompt appeared, so the verified
    /// frames provably reached the transcript before the run ended.
    static MONITOR_DONE: AtomicBool = AtomicBool::new(false);

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

    /// Sink that replays every event through [`SERIAL_SINK`] and reports
    /// PASS once `sysmon` has exited and the shell's subsequent scripted
    /// `exit` dispatches (see the module docs for why the PASS is deferred
    /// to the second exit).
    struct SysmonSink;

    impl Sink for SysmonSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records
            // the full boot + session timeline.
            SerialSink::new().write_event(event);
            if event.id != SYSCALL_INVOKED_EVENT_ID || field_str(event, "sc") != Some("exit") {
                return;
            }
            if field_str(event, "comm") == Some(MONITOR_COMM) {
                MONITOR_DONE.store(true, Ordering::Release);
            } else if MONITOR_DONE.load(Ordering::Acquire) {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: SysmonSink = SysmonSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_sysmon_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
            &SERIAL_SINK,
            &AUDIT_SINK,
            // `SyscallInvoked` (`EventId(5000)`) is `Debug`, below the
            // default `Info` filter; this observer counts it, so boot
            // with the filter lowered.
            tairix_log::Level::Debug,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
