//! Stage 4 QEMU integration test: boot the production `rustos-kernel`
//! pipeline to `AuditEvent::BootCompleted`, load the signed PS/2 input
//! driver through `rustos_drvhost::Host`, then drive a real
//! `rustos_drv_input_ps2::Ps2Keyboard` over the emulated i8042
//! controller through `load -> use -> unload -> reload`, and signal
//! QEMU success.
//!
//! The boot pipeline reuse pattern mirrors `tests/integration/
//! drvhost_qemu/src/main.rs`: the audit sink is the integration test's
//! hook point, the rest of the kernel is production code.
//!
//! "Use the device" is made deterministic without a physical keypress
//! by the i8042 controller's `0xD2` ("write keyboard output buffer")
//! command: the test writes `0xD2` to the command port and a scancode
//! to the data port, which the controller then presents on its output
//! buffer exactly as if the keyboard had produced it. The driver — which
//! only ever *reads* the controller — decodes it into an
//! `InputEvent`. The injection path uses the same `PortIo8` backend the
//! driver reads through, so the whole round trip exercises real port
//! I/O.
//!
//! On the host (non-`x86_64-unknown-none`) target the bin is a no-op so
//! that `cargo build --workspace` does not require the
//! `x86_64-unknown-none` toolchain at every check.

#![cfg_attr(itest_x86_64, no_std)]
#![cfg_attr(itest_x86_64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_x86_64)]
mod fixture {
    //! Pull in the build-time generated PS/2 driver fixture.
    include!(concat!(env!("OUT_DIR"), "/ps2_fixture.rs"));
}

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(itest_x86_64)]
mod kernel {
    extern crate alloc;

    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use alloc::vec::Vec;
    use rustos_abi::driver::input::{Input, InputEvent, InputEventKind};
    use rustos_abi::{CapabilityId, DriverManifest, Errno, PortIo8};
    use rustos_arch_x86_64::pio::{x86_port_io8, X86PortIo8};
    use rustos_arch_x86_64::qemu_exit;
    use rustos_caps::CapabilitySet;
    use rustos_crypto::Ed25519PublicKey;
    use rustos_drv_input_ps2::Ps2Keyboard;
    use rustos_drvhost::{EntryResolver, Host, HostConfig, ImageSource};
    use rustos_kernel::bumpalloc::{Heap, HEAP_BYTES};
    use rustos_kernel::{
        boot, handle_panic_via_kernel_core, BumpAllocator, SerialSink, SERIAL_SINK,
    };
    use rustos_log::{Event, EventId, Sink};

    use crate::fixture::{PS2_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

    /// Static heap backing the bump allocator. Sized identically to the
    /// `drvhost_qemu` vertical's — the workload is the same shape (one
    /// boot pipeline plus a handful of `Vec` allocations from the host's
    /// load path).
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`]. The pointer to `HEAP`
    /// outlives the binary, and the allocator is the only consumer
    /// (`AGENTS.md` §4 — deterministic OOM via `BumpAllocator`).
    #[global_allocator]
    static ALLOCATOR: BumpAllocator =
        unsafe { BumpAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

    /// `EventId(4004)` — `AuditEvent::BootCompleted`. Pinned by the
    /// `event_ids_are_unique` test in `kernel/core/src/audit.rs`.
    const BOOT_COMPLETED_EVENT_ID: EventId = EventId(4004);

    /// Latch so the exercise runs exactly once.
    static PS2_RAN: AtomicBool = AtomicBool::new(false);

    // ---- i8042 controller constants ----

    /// Status (read) / command (write) register.
    const STATUS_PORT: u16 = 0x64;
    /// Data register.
    const DATA_PORT: u16 = 0x60;
    /// Status bit: the output buffer holds a byte for the host.
    const STATUS_OUTPUT_FULL: u8 = 1 << 0;
    /// Status bit: the input buffer still holds a byte for the
    /// controller (a fresh command/data write must wait for it to
    /// clear).
    const STATUS_INPUT_FULL: u8 = 1 << 1;
    /// Controller command: the next data-port byte is placed into the
    /// keyboard output buffer as though the keyboard had sent it.
    const CMD_WRITE_OUTPUT_BUFFER: u8 = 0xD2;
    /// Scancode-set-1 make code for the `A` key.
    const SCANCODE_A_MAKE: u8 = 0x1E;
    /// Scancode-set-1 break code for the `A` key (make | release bit).
    const SCANCODE_A_BREAK: u8 = 0x9E;
    /// Iteration ceiling for every controller busy-wait, so a wedged
    /// controller can never make the test spin (`AGENTS.md` §2.1); the
    /// QEMU wall-clock budget is the backstop if a bound is ever hit.
    const SPIN_BUDGET: u32 = 1_000_000;

    // ---- Mock fixtures (no_std) ----

    /// Always returns the baked-in [`PS2_IMAGE`] regardless of path.
    struct BakedSource;

    impl ImageSource for BakedSource {
        fn read(&self, _path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
            buf.extend_from_slice(PS2_IMAGE);
            Ok(())
        }
    }

    /// Resolver that binds every manifest to the real PS/2 driver entry
    /// point, so the host's load path runs the production
    /// `rustos_drv_input_ps2::register` capability gate.
    struct ResolvePs2;
    impl EntryResolver for ResolvePs2 {
        fn resolve(
            &self,
            _manifest: &DriverManifest,
            _payload: &[u8],
        ) -> Option<rustos_drvhost::DriverEntry> {
            Some(rustos_drv_input_ps2::register as rustos_drvhost::DriverEntry)
        }
    }

    /// Spin until the controller's input buffer is empty (ready to
    /// accept a command/data byte). Returns `false` if the bound is
    /// exhausted first.
    fn wait_input_clear(io: X86PortIo8) -> bool {
        let mut budget = SPIN_BUDGET;
        while budget > 0 {
            if io.read8(STATUS_PORT) & STATUS_INPUT_FULL == 0 {
                return true;
            }
            budget -= 1;
        }
        false
    }

    /// Spin until the controller's output buffer holds a byte. Returns
    /// `false` if the bound is exhausted first.
    fn wait_output_full(io: X86PortIo8) -> bool {
        let mut budget = SPIN_BUDGET;
        while budget > 0 {
            if io.read8(STATUS_PORT) & STATUS_OUTPUT_FULL != 0 {
                return true;
            }
            budget -= 1;
        }
        false
    }

    /// Discard any bytes already sitting in the output buffer (firmware
    /// self-test results, etc.) so the subsequently injected scancode is
    /// the only event the driver sees.
    fn drain_controller(io: X86PortIo8) {
        let mut budget = 64u32;
        while budget > 0 && io.read8(STATUS_PORT) & STATUS_OUTPUT_FULL != 0 {
            let _ = io.read8(DATA_PORT);
            budget -= 1;
        }
    }

    /// Inject `scancode` into the keyboard output buffer via the `0xD2`
    /// controller command. Returns `false` if any wait bound is hit.
    fn inject_scancode(io: X86PortIo8, scancode: u8) -> bool {
        if !wait_input_clear(io) {
            return false;
        }
        io.write8(STATUS_PORT, CMD_WRITE_OUTPUT_BUFFER);
        if !wait_input_clear(io) {
            return false;
        }
        io.write8(DATA_PORT, scancode);
        wait_output_full(io)
    }

    /// Inject one scancode, decode it through a fresh `Ps2Keyboard`, and
    /// confirm exactly one `Key` event with the expected `code`/`value`.
    fn inject_and_expect(scancode: u8, expect_code: u16, expect_value: i32) -> bool {
        let io = x86_port_io8();
        drain_controller(io);
        if !inject_scancode(io, scancode) {
            return false;
        }
        let mut keyboard = Ps2Keyboard::new(x86_port_io8());
        let mut events = [InputEvent {
            kind: InputEventKind::Key,
            reserved0: 0,
            code: 0,
            value: 0,
        }; 8];
        match keyboard.poll(&mut events) {
            Ok(1) => {
                let ev = events[0];
                ev.kind == InputEventKind::Key && ev.code == expect_code && ev.value == expect_value
            }
            _ => false,
        }
    }

    /// Exercise the PS/2 driver end to end on the boot-completion edge.
    fn drive_ps2() {
        // Build the host's trust anchor list.
        let Ok(pubkey) = Ed25519PublicKey::from_bytes(&TRUSTED_SIGNER_PUBKEY) else {
            qemu_exit::exit_failure();
        };
        let trusted = [pubkey];

        // Caller capability set: hold CAP_DRV_LOAD only — the PS/2
        // driver requests no further capabilities.
        let mut caller = CapabilitySet::empty();
        caller.insert(CapabilityId::DRV_LOAD);

        let source = BakedSource;
        let resolver = ResolvePs2;
        let cfg = HostConfig {
            trusted_signers: &trusted,
            syscall_table_hash: SYSCALL_TABLE_HASH,
            accepted_abi_version: rustos_abi::ABI_VERSION_CURRENT,
            source: &source,
            resolver: &resolver,
            sink: &SerialSink::new(),
            // The PS/2 driver consumes no virtio transport.
            virtio_host_factory: None,
        };
        let mut host = Host::new(cfg);

        // load: the signed PS/2 `.rxe` clears the §8 load gate through
        // the real `rustos_drv_input_ps2::register`.
        let Ok(h1) = host.load("/d/ps2", &caller) else {
            qemu_exit::exit_failure();
        };
        if host.loaded_count() != 1 || host.snapshot()[0].handle != h1 {
            qemu_exit::exit_failure();
        }

        // use: inject a key *press* and decode it.
        if !inject_and_expect(SCANCODE_A_MAKE, u16::from(SCANCODE_A_MAKE), 1) {
            qemu_exit::exit_failure();
        }

        // unload -> reload through the host.
        let Ok(h2) = host.reload(h1, &caller) else {
            qemu_exit::exit_failure();
        };
        if h2 == h1 || host.loaded_count() != 1 {
            qemu_exit::exit_failure();
        }

        // use again after reload: inject the matching *release* and
        // decode it (same keycode, value 0).
        if !inject_and_expect(SCANCODE_A_BREAK, u16::from(SCANCODE_A_MAKE), 0) {
            qemu_exit::exit_failure();
        }

        // unload: tear the driver down cleanly.
        if host.unload(h2).is_err() || host.loaded_count() != 0 {
            qemu_exit::exit_failure();
        }
    }

    /// Audit observer sink. Forwards every event to [`SerialSink`] and,
    /// on observing `BootCompleted`, exercises the PS/2 driver then
    /// flips QEMU to `exit_success`.
    struct BootObserverSink;
    impl Sink for BootObserverSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id == BOOT_COMPLETED_EVENT_ID && !PS2_RAN.swap(true, Ordering::SeqCst) {
                drive_ps2();
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: BootObserverSink = BootObserverSink;

    /// Panic handler — forwards through `rustos_kernel`'s shared bridge.
    #[panic_handler]
    fn ps2_qemu_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// Boot entry point — same surface the production `rustos-kernel`
    /// bin exposes, but with our audit sink in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(multiboot_info, &SERIAL_SINK, &AUDIT_SINK)
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
