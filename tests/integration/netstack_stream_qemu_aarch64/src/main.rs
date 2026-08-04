//! `plans/NETWORK.md` N5c QEMU integration test: boot the *production*
//! aarch64 `tairix-kernel` pipeline on the `virt` board with the planted
//! whole-disk encrypted-root image, a virtio-net device attached, and the
//! harness-side passive TCP echo peer on its `dgram` netdev, then prove the
//! **stream-socket** (`SocketType::Stream`) path end to end over the live
//! two-process network — client process, network stack, and virtio-net driver
//! each in its own address space.
//!
//! ## What this test asserts
//!
//! The vertical's disk is the shared encrypted-root fixture whose read-only
//! `/System` volume carries the standard signed store bundles **plus** the
//! signed virtio-net driver bundle (so `devmgr` autoloads it into its own
//! process and `netstack` binds it) **plus** the test-only `tcpecho` fixture
//! bundle (`tests/integration/tcpecho_program`), all composed and signed by
//! the same `tools/xtask` composer as every store bundle and planted only on
//! this vertical's disk — no production image ever ships the fixtures.
//!
//! The runner unlocks the root at the passphrase prompt, authenticates
//! `root`/`root` at the console login, and types the bare command word
//! `tcpecho` at the shell: the store-then-`PATH` resolution finds
//! `/System/Commands/tcpecho.app/Run`, the disk-backed spawn path verifies the
//! signed bundle, and the client runs with `manifest ∩ administrator-ceiling`
//! authority (its manifest requests `CAP_NET`, enforced by the netstack socket
//! dispatcher against the kernel-attested origin). The client opens a stream
//! socket, connects to the peer's echo server over the shared IPv6 link-local
//! wire, streams a fixed deterministic byte run, and verifies the peer echoes
//! every byte back in order. The peer injects bounded frame loss, so a passing
//! run proves RFC 9293 retransmission carried the stream across the
//! two-process boundary — not merely a clean link.
//!
//! ## Why the PASS keys on the client's exit *then* the shell's exit
//!
//! On a fully verified transfer the client prints its `TCPECHO PASS …` line
//! and exits `0`; on **any** shortfall it prints the reason and parks forever
//! — it never exits — so the client's audited `exit` (`SyscallInvoked`,
//! `EventId(5000)`, `sc=exit`, `comm=tcpecho`) is itself the kernel-side
//! witness of a verified transfer. Exiting QEMU on that record would tear the
//! run down before the runner observed the marker and sent its final line, so
//! the sink only *arms* there and reports PASS on the **next** audited `exit`
//! — the shell's, which the runner types only after the `TCPECHO PASS` marker
//! appeared past the client's own output. The runner additionally fails the
//! run if the guest exits before every scripted marker appeared, and the
//! harness requires the echo peer to report the whole transfer received and
//! echoed, so neither side can pass alone. A refused connect, a mismatched or
//! truncated echo, or a client crash therefore never reaches the armed exit:
//! the run times out with the diagnosis in the serial transcript — the
//! documented fail-loud behaviour.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline and only replaces the
//! audit sink. Splitting the audit-observer behaviour into a separate bin
//! (instead of a Cargo feature on a production crate) prevents feature
//! unification from leaking the QEMU-exit shortcut into any production build
//! (fail closed; the harness never decides what the kernel does next).

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
    use tairix_test_tcpecho::COMMAND;

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// Static boot heap, mirroring the production aarch64 kernel binary's
    /// `.bss`-resident heap (zeroed by the boot trampoline).
    ///
    /// `static mut` because the free-list allocator hands out disjoint slices
    /// via an atomic cursor; the storage is otherwise never aliased.
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

    /// Set once the `tcpecho` client's own audited `exit` has been observed —
    /// the kernel-side witness that the client judged the transfer fully
    /// verified (a failed transfer parks forever and never exits). The PASS
    /// finisher fires on the next audited `exit`: the shell's, typed by the
    /// runner only after the `TCPECHO PASS` marker appeared, so the report
    /// provably reached the transcript before the run ended.
    static CLIENT_EXITED: AtomicBool = AtomicBool::new(false);

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

    /// Sink that replays every event through [`SERIAL_SINK`] and reports PASS
    /// once the `tcpecho` client's audited `exit` has been observed and the
    /// shell's subsequent scripted `exit` dispatches (see the module docs for
    /// why the PASS is deferred to the second exit).
    struct StreamSink;

    impl Sink for StreamSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records the
            // full boot + autoload + connect + transfer timeline.
            SerialSink::new().write_event(event);
            if event.id != SYSCALL_INVOKED_EVENT_ID || field_str(event, "sc") != Some("exit") {
                return;
            }
            if field_str(event, "comm") == Some(COMMAND) {
                CLIENT_EXITED.store(true, Ordering::Release);
            } else if CLIENT_EXITED.load(Ordering::Acquire) {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: StreamSink = StreamSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_netstack_stream_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline with the
    /// audit-observer sink in place. `SyscallInvoked` (`EventId(5000)`) is a
    /// `Debug` record, below the default `Info` filter; this observer counts
    /// it, so boot with the filter lowered.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &SERIAL_SINK,
            &AUDIT_SINK,
            tairix_log::Level::Debug,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
