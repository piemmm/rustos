//! `plans/APPS.md` "Immediate work" I2/I3 QEMU integration test: boot the
//! *production* aarch64 `tairix-kernel` pipeline on the `virt` board with
//! the planted whole-disk encrypted-root image, log in as the seeded `root`
//! account, and prove — with a **numeric** `KernelMemoryStats` comparison on
//! the live system — that the per-cycle process footprint is exactly
//! reclaimed (the I2 teardown) and that the `top -d0` refresh shape retains
//! no kernel memory (the I3 live re-test).
//!
//! ## What this test asserts
//!
//! The vertical's disk is the shared encrypted-root fixture whose read-only
//! `/System` volume carries the standard signed store bundles **plus** the
//! test-only `memsoak` fixture bundle
//! (`tests/integration/memsoak_program`), composed and signed by the same
//! `tools/xtask` composer as every store bundle and planted only on this
//! vertical's disk — no production image ever ships it. The runner unlocks
//! the root at the passphrase prompt, authenticates `root`/`root` at the
//! console login, and types the bare command word `memsoak` at the shell:
//! the store-then-`PATH` resolution finds `/System/Commands/memsoak.app/Run`,
//! the disk-backed spawn path verifies the signed bundle, and the fixture
//! runs with `manifest ∩ administrator-ceiling` authority (its manifest
//! requests `CAP_SYSINFO_KERNEL` for the memory query; sysinfod enforces it
//! against the kernel-attested origin).
//!
//! The fixture (`tairix-test-memsoak`) then soaks the live system: warmup
//! cycles pay every once-per-boot cost, a baseline `KERNEL_MEMORY_STATS`
//! sample — free memory plus user residency, so another process allocating
//! meanwhile cannot move it (`tairix_test_memsoak::sample_bytes`) — is taken
//! through sysinfod, and each
//! of the measured cycles spawns and reaps a `true.app` child (the full
//! spawn → exit → reap → teardown path: user frames, startup block, and
//! page-table hierarchy reclaimed), parks on a timed `stream_read` whose
//! bound elapses (the `top -d0` refresh park), walks the self-scoped
//! process list, and rides a live sysinfod IPC round trip. The final sample
//! must equal the baseline **exactly**; the strict verdict is the
//! host-tested `tairix_test_memsoak::verdict`.
//!
//! ## Why the PASS keys on the fixture's exit *then* the shell's exit
//!
//! On a stable soak the fixture prints its `MEMSOAK PASS baseline=… final=…`
//! line and exits `0`; on **any** failure it prints the reason and parks
//! forever — it never exits — so the fixture's audited `exit`
//! (`SyscallInvoked`, `EventId(5000)`, `sc=exit`, `comm=memsoak`) is itself
//! the kernel-side witness of a stable verdict. Exiting QEMU on that record
//! would tear the run down before the runner observed the marker and sent
//! its final line, so the sink only *arms* there and reports PASS on the
//! **next** audited `exit` — the shell's, which the runner types only after
//! the `MEMSOAK PASS` marker appeared past the fixture's own output. The
//! runner additionally fails the run if the guest exits before every
//! scripted marker appeared and every line was sent. A drifted soak, a
//! refused query, a failed spawn/wait cycle, or a fixture crash therefore
//! never reaches the armed exit: the run times out with the diagnosis (the
//! `MEMSOAK FAIL baseline=… final=…` line or the failing step's reason) in
//! the serial transcript — the documented fail-loud behaviour.
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
    use tairix_test_memsoak::COMMAND;

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

    /// Set once the fixture's own audited `exit` has been observed — the
    /// kernel-side witness that the soak judged itself stable (a failed
    /// soak parks forever and never exits). The PASS finisher fires on the
    /// next audited `exit`: the shell's, typed by the runner only after the
    /// `MEMSOAK PASS` marker appeared, so the numeric verdict provably
    /// reached the transcript before the run ended.
    static SOAK_EXITED: AtomicBool = AtomicBool::new(false);

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
    /// PASS once the memsoak fixture's audited `exit` has been observed and
    /// the shell's subsequent scripted `exit` dispatches (see the module
    /// docs for why the PASS is deferred to the second exit).
    struct MemsoakSink;

    impl Sink for MemsoakSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records
            // the full boot + soak timeline.
            SerialSink::new().write_event(event);
            if event.id != SYSCALL_INVOKED_EVENT_ID || field_str(event, "sc") != Some("exit") {
                return;
            }
            if field_str(event, "comm") == Some(COMMAND) {
                SOAK_EXITED.store(true, Ordering::Release);
            } else if SOAK_EXITED.load(Ordering::Acquire) {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: MemsoakSink = MemsoakSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_memsoak_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
