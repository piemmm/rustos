//! `plans/SOUND.md` SND4 QEMU integration test: boot the production
//! x86_64 `tairix-kernel` pipeline against the shared whole-disk audio-root
//! image — whose read-only `/System` volume carries the kernel-signed virtio
//! sound driver bundle and the test-only `audiotone` fixture — with a virtio
//! sound device attached behind QEMU's `wav` audio backend, and prove the
//! whole audio path end to end with an exact numeric assertion.
//!
//! ## What this vertical asserts
//!
//! The production boot path discovers the sound device, `devmgr` autoloads
//! the signed driver into its **own user process** (it claims a reserved
//! `audiochan-v1` endpoint and publishes its channel node), `devmgr` hands
//! that endpoint to **`audiod`**, and the scripted root shell runs
//! `audiotone`, which opens an `audio-v1` stream, queues a deterministic
//! signal, starts the device and drains. Three processes and two shared PCM
//! rings are between the program and the hardware, so a frame provably
//! crosses two real process boundaries here.
//!
//! The guest's own `AUDIO PASS` witness says the stack reported success with
//! no lost frames. The **host-side** assertion says more: QEMU's `wav`
//! backend wrote what the emulated card actually received, and the harness
//! checks it holds exactly the samples the guest played. A mixer that
//! silently substituted, resampled or dropped frames would still print the
//! witness, so the run needs both.
//!
//! The run is deterministic rather than a race: the ring is sized to hold the
//! whole signal, so every frame is queued before the device is clocked and
//! the device cannot run dry however slowly the emulated machine runs.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production x86_64 boot pipeline unchanged. The only
//! difference is that it is a dedicated test bin the harness drives; there is
//! the audit-stream observer that ends the run on the scripted exit; it lives
//! in this test bin, so no QEMU-exit shortcut leaks into a production build.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(itest_x86_64)]
mod kernel {
    use core::panic::PanicInfo;

    use core::sync::atomic::{AtomicBool, Ordering};

    use tairix_arch_x86_64::qemu_exit;
    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use tairix_log::{Event, EventId, FieldValue, Sink};
    use tairix_test_audio_wire::COMMAND;

    /// `SyscallInvoked`, the audited record the finisher below counts.
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    /// Whether the fixture's own audited `exit` has been seen.
    static TONE_EXITED: AtomicBool = AtomicBool::new(false);

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

    /// Sink that replays every event through [`SERIAL_SINK`] and finishes
    /// the run once the fixture's audited `exit` has been seen and the
    /// shell's own scripted `exit` follows it.
    ///
    /// The second exit is what makes the witness sound: the host script
    /// sends it only after the guest has printed its PASS line, so a
    /// fixture that failed never reaches it and the run times out instead.
    struct AudioSink;

    impl Sink for AudioSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id != SYSCALL_INVOKED_EVENT_ID || field_str(event, "sc") != Some("exit") {
                return;
            }
            if field_str(event, "comm") == Some(COMMAND) {
                TONE_EXITED.store(true, Ordering::Release);
            } else if TONE_EXITED.load(Ordering::Acquire) {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: AudioSink = AudioSink;

    /// Static heap for the bump allocator (identical to the production bin's
    /// declaration; `#[global_allocator]` is per-binary).
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

    /// Forward to the shared bridge in `tairix_kernel::x86_64::panic_ctx`.
    /// A panic halts the guest; it never self-exits, so the run times out and
    /// the harness reports `Outcome::Timeout` (fail-loud).
    #[panic_handler]
    fn tairix_audio_virtio_qemu_x86_64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// The symbol the arch crate's boot trampoline calls. Forwards to
    /// [`tairix_kernel::boot`]. [`SERIAL_SINK`] carries the log stream so
    /// every boot/autoload/bind record reaches the QEMU transcript, and
    /// [`AudioSink`] observes the audit stream to finish the run once the
    /// fixture and then the shell have exited.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(
            multiboot_info,
            &ALLOCATOR,
            &SERIAL_SINK,
            &AUDIT_SINK,
            // `SyscallInvoked` is `Debug`, below the default filter, and
            // the finisher above counts it.
            tairix_log::Level::Debug,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
