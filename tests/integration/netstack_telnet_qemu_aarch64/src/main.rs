//! `plans/TELNET.md` QEMU integration test: boot the *production* aarch64
//! `tairix-kernel` pipeline on the `virt` board with the planted whole-disk
//! encrypted-root image, a virtio-net device attached, and the harness-side
//! **telnet-server** peer on its `dgram` netdev, then prove the RFC 854
//! client end to end over the live two-process network — the `telnet` command
//! process, the network stack, and the virtio-net driver each in its own
//! address space.
//!
//! ## What this test asserts
//!
//! The vertical's disk is the shared net-tool encrypted-root fixture whose
//! read-only `/System` volume carries the **standard** signed store bundles —
//! including the real `telnet` command bundle, discovered from
//! `userland/apps/telnet` by the same `tools/xtask` composer as every other
//! bundle — **plus** the signed virtio-net driver bundle (so `devmgr`
//! autoloads it into its own process and `netstack` binds it). It plants no
//! test-only fixture: `telnet` is a shipping tool, exercised exactly as a user
//! would run it.
//!
//! The runner unlocks the root at the passphrase prompt, authenticates
//! `root`/`root` at the console login, and types `telnet fe80::2` at the shell
//! — the host peer's link-local address, formed from the shared wire
//! identifier, with **no** port operand so the tool's own default port is what
//! is exercised. The store-then-`PATH` resolution finds
//! `/System/Commands/telnet.app/Run`, the disk-backed spawn path verifies the
//! signed bundle, and the tool runs with `manifest ∩ administrator-ceiling`
//! authority (`CAP_NET` for the stream socket, `CAP_CONSOLE_READ` for the
//! raw-mode relay), retrying `connect` through the boot window while the NIC
//! driver is still autoloading.
//!
//! ## Why the serial gates are a proof, not a reachability check
//!
//! Three markers stand between the connection and the PASS, each of which a
//! merely-connected client cannot produce:
//!
//! * The peer's banner is sent **only** after the client accepted `DO SUPPRESS
//!   GO AHEAD`, named its terminal type, reported its window size over NAWS,
//!   agreed `WILL LINEMODE`, stated an RFC 1184 `MODE` mask, and exported its
//!   SLC table. A client that connected but ignored the negotiation never sees
//!   it, so the script never types the next line and the run times out
//!   fail-loud with the transcript.
//! * The peer's upper-cased answer to the probe line the script then types
//!   proves the bytes made a full round trip through the telnet data path in
//!   both directions. The client's own local echo of the probe is lower case,
//!   so it cannot be mistaken for the answer.
//! * The escape character `^]` and the interpreter's `quit` are what end the
//!   session, so the escape recognition and the `telnet>` command interpreter
//!   are exercised on the live wire and the tool exits of its own accord
//!   rather than being killed.
//!
//! ## Why the PASS keys on the tool's exit *then* the shell's exit
//!
//! `telnet` exits when the operator quits, which the script does only after
//! the echoed answer appeared — but its exit alone still does not prove the
//! transcript carried that answer before the run ended. So the guest audit
//! sink arms on the tool's own audited `exit` (`SyscallInvoked`,
//! `EventId(5000)`, `sc=exit`, `comm=telnet`) and reports PASS on the **next**
//! audited `exit` — the shell's. The harness additionally requires the host
//! peer's own verdict, which names the first negotiation step the client
//! failed to complete, so neither side can pass alone.
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

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// The audited `comm` of the `telnet` command process — the `AppInfo.toml`
    /// bundle name (`userland/apps/telnet/AppInfo.toml`), which the kernel
    /// spawn path derives the process command stem from and the audit
    /// dispatcher reports as `comm`. `telnet` is a shipping command app, not a
    /// test fixture, so there is no fixture crate to import this from; it is
    /// the literal command word the runner also types at the shell.
    const TELNET_COMMAND: &str = "telnet";

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

    /// Set once the `telnet` tool's own audited `exit` has been observed. The
    /// PASS finisher fires on the next audited `exit`: the shell's, typed by
    /// the runner only after the peer's echoed answer appeared, so the round
    /// trip provably reached the transcript before the run ended.
    static TELNET_EXITED: AtomicBool = AtomicBool::new(false);

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
    /// once the `telnet` tool's audited `exit` has been observed and the
    /// shell's subsequent scripted `exit` dispatches (see the module docs for
    /// why the PASS is deferred to the second exit).
    struct TelnetSink;

    impl Sink for TelnetSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records the
            // full boot + autoload + connect + negotiate + relay timeline.
            SerialSink::new().write_event(event);
            if event.id != SYSCALL_INVOKED_EVENT_ID || field_str(event, "sc") != Some("exit") {
                return;
            }
            if field_str(event, "comm") == Some(TELNET_COMMAND) {
                TELNET_EXITED.store(true, Ordering::Release);
            } else if TELNET_EXITED.load(Ordering::Acquire) {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: TelnetSink = TelnetSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_netstack_telnet_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
            &ALLOCATOR,
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
