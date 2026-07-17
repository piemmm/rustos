//! `plans/SPAWN.md` `SP10b` QEMU integration test: boot the *production*
//! aarch64 `tairix-kernel` pipeline on the `virt` board with the planted
//! whole-disk encrypted-root image, log in as the seeded `root` account,
//! and drive the spawned shell through real **pipelines and redirections**
//! over the spawn attach block and kernel pipes.
//!
//! ## What this test asserts
//!
//! The production boot path unlocks and mounts the encrypted `ARXFS`
//! root, login authenticates `root`/`root`, and the session shell runs
//! the runner's ordered script:
//!
//! 1. `yes | head -n 2` — a two-member kernel pipeline whose consumer
//!    exits first: `head` prints two lines and exits, the pipe loses its
//!    last reader, `yes`'s next write fails `BrokenPipe` and it exits,
//!    and the shell reaps the non-leader member after the leader. The
//!    witness is the next prompt appearing at all — a missed broken-pipe
//!    delivery or an unreaped member would hang the session and time the
//!    run out.
//! 2. `seq 1 1000 | wc -c` — the consumer's `3893` on the transcript is
//!    byte-exact arithmetic over the pipe's entire payload (the 2893
//!    digits plus 1000 newlines of `1..=1000`), output the typed line
//!    itself never contains, so it proves every byte crossed the kernel
//!    pipe in order.
//! 3. `lspci --help` — the resource-carrying `lspci.app` bundle end to
//!    end (`plans/DEVICES.md` DEVICE1 V2): the spawn's load gate re-hashes
//!    the whole on-disk bundle — including the planted
//!    `Resources/pci.ids.bin` table — against the signed `AppInfo` content
//!    hash, and the help summary's `PCI/PCIe` on the transcript witnesses
//!    the tool ran. The `virt` image drives virtio-mmio devices, so the
//!    tree carries no PCI-function nodes to list yet; the listing path is
//!    host-proven in `tairix-lspci`'s tests.
//! 4. `lsusb --help` — the same proof for the resource-carrying
//!    `lsusb.app` bundle (`plans/DEVICES.md` DEVICE1 V3): the load gate
//!    re-hashes it — including the planted `Resources/usb.ids.bin` table —
//!    and the help summary's `USB devices` witnesses the tool ran; the
//!    listing path is host-proven in `tairix-lsusb`'s tests.
//! 5. `seq 776001 776005 > /Users/root/nums.txt` — the shell pre-opens
//!    the target (create + truncate) in its own table and wires it as the
//!    child's stdout through the spawn attach block.
//! 6. `cat < /Users/root/nums.txt` — the round trip back: the shell opens
//!    the file read-only and wires it as `cat`'s stdin; `776005` on the
//!    transcript is content only step 5's write could have produced.
//! 7. `exit` — typed only after the content marker appeared.
//!
//! ## Why the PASS keys on `cat`'s exit *then* the shell's exit
//!
//! `pipe_create` is deliberately unaudited, so the kernel-side witness is
//! the round trip's `cat` exiting (`SyscallInvoked`, `EventId(5000)`,
//! `sc=exit`, `comm=cat`) — the last scripted tool, which only runs after
//! every pipeline step completed. Exiting QEMU there would tear the run
//! down before the runner observed the content marker and sent its final
//! line, so the sink only *arms* on it and reports PASS on the **next**
//! audited `exit` — the shell's, typed only after `776005` appeared — so
//! the verified bytes provably reached the transcript before the run
//! ended (the session-ceiling arm-then-exit discipline). The runner
//! additionally fails the run if the guest exits before every scripted
//! marker appeared and every line was sent. A refused spawn wire, a hung
//! pipe, an unreaped member, or a wrong byte count never reaches the
//! armed exit: the run times out with the failing step in the serial
//! transcript — the documented fail-loud behaviour.
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

    /// The scripted round trip's final tool: `cat < /Users/root/nums.txt`.
    /// Its audited `exit` is the kernel-side witness that every scripted
    /// pipeline and redirection step ran to completion.
    const FINAL_TOOL_COMM: &str = "cat";

    /// Set once `cat`'s audited `exit` has been observed. The PASS
    /// finisher fires on the next audited `exit`: the shell's, typed by
    /// the runner only after the `776005` content marker appeared, so the
    /// verified round-trip bytes provably reached the transcript before
    /// the run ended.
    static ROUND_TRIP_DONE: AtomicBool = AtomicBool::new(false);

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
    /// PASS once the round trip's `cat` has exited and the shell's
    /// subsequent scripted `exit` dispatches (see the module docs for why
    /// the PASS is deferred to the second exit).
    struct PipelineSink;

    impl Sink for PipelineSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records
            // the full boot + session timeline.
            SerialSink::new().write_event(event);
            if event.id != SYSCALL_INVOKED_EVENT_ID || field_str(event, "sc") != Some("exit") {
                return;
            }
            if field_str(event, "comm") == Some(FINAL_TOOL_COMM) {
                ROUND_TRIP_DONE.store(true, Ordering::Release);
            } else if ROUND_TRIP_DONE.load(Ordering::Acquire) {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: PipelineSink = PipelineSink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_pipeline_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
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
