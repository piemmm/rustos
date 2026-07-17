//! Stage 4 QEMU integration test: boot the production `tairix-kernel`
//! pipeline to `AuditEvent::BootCompleted`, load the signed PS/2 input
//! driver through `tairix_drvhost::Host`, then drive a real
//! `tairix_drv_input_ps2::Ps2Keyboard` over the emulated i8042
//! controller through `load -> use -> unload -> reload`, and signal
//! QEMU success.
//!
//! The boot pipeline reuse pattern mirrors `tests/integration/
//! drvhost_qemu/src/main.rs`: the audit sink is the integration test's
//! hook point, the rest of the kernel is production code.
//!
//! "Use the device" is **interrupt-driven**, not polled: the test binds
//! the keyboard line (ISA IRQ-1 → GSI 1) in the production
//! `tairix_kernel_irq::IrqTable`, enables the i8042's keyboard-interrupt
//! config bit, masks the legacy 8259 PIC, and unmasks GSI 1 through the
//! published `IoApicController`. It then makes a keypress deterministic
//! without physical hardware via the controller's `0xD2` ("write
//! keyboard output buffer") command — writing `0xD2` to the command
//! port and a scancode to the data port presents the byte on the output
//! buffer exactly as if the keyboard had produced it, which asserts the
//! IRQ-1 line. After `sti`, the test waits on `IrqTable::try_wait_step`
//! until the real IO-APIC → LAPIC → IDT → dispatcher → `IrqTable::fire`
//! round-trip reports `WaitStep::Ready`, then drains the byte through
//! the driver's `poll` and decodes it into an `InputEvent`. The driver
//! itself only ever *reads* the controller; the interrupt merely tells
//! the test *when* a byte is waiting. The injection, mask, and drain
//! paths all use the same `PortIo8` backend the driver reads through, so
//! the whole round trip exercises real port I/O and the real external-
//! IRQ trap glue (the same path `tests/integration/irq_qemu_x86_64`
//! validates against the PIT).
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
    use tairix_abi::driver::input::{Input, InputEvent, InputEventKind};
    use tairix_abi::{CapabilityId, DriverHandle, Errno, IrqHandle, PortIo8};
    use tairix_arch_x86_64::irq as arch_irq;
    use tairix_arch_x86_64::pio::{x86_port_io8, X86PortIo8};
    use tairix_arch_x86_64::qemu_exit;
    use tairix_caps::CapabilitySet;
    use tairix_crypto::Ed25519PublicKey;
    use tairix_drv_input_ps2::Ps2Keyboard;
    use tairix_drvhost::{
        DriverSpawner, Host, HostConfig, ImageSource, SpawnContext, SpawnRegisterError,
    };
    use tairix_kernel::kalloc::{Heap, HEAP_BYTES};
    use tairix_kernel::x86_64::arch_wrapper::published_irq_table;
    use tairix_kernel::x86_64::ioapic_controller::published_typed;
    use tairix_kernel::{
        boot, handle_panic_via_kernel_core, FreeListAllocator, SerialSink, SERIAL_SINK,
    };
    use tairix_kernel_irq::WaitStep;
    use tairix_kernel_sec::TaskId as SecTaskId;
    use tairix_log::{Event, EventId, Sink};

    use crate::fixture::{PS2_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

    /// Static heap backing the bump allocator. Sized identically to the
    /// `drvhost_qemu` vertical's — the workload is the same shape (one
    /// boot pipeline plus a handful of `Vec` allocations from the host's
    /// load path).
    static mut HEAP: Heap = Heap::ZERO;

    /// Global allocator backed by [`HEAP`]. The pointer to `HEAP`
    /// outlives the binary, and the allocator is the only consumer
    /// (deterministic OOM via `FreeListAllocator`).
    #[global_allocator]
    static ALLOCATOR: FreeListAllocator =
        unsafe { FreeListAllocator::new(core::ptr::addr_of!(HEAP) as *mut u8, HEAP_BYTES) };

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
    /// controller can never make the test spin; the
    /// QEMU wall-clock budget is the backstop if a bound is ever hit.
    const SPIN_BUDGET: u32 = 1_000_000;
    /// Controller command: copy the next data-port byte into the
    /// controller's command/config byte.
    const CMD_WRITE_CONFIG: u8 = 0x60;
    /// Controller command: present the current command/config byte on
    /// the output buffer for the host to read.
    const CMD_READ_CONFIG: u8 = 0x20;
    /// Config-byte bit 0: assert IRQ-1 whenever the keyboard output
    /// buffer fills. The test sets it so the injected scancode raises a
    /// real interrupt; every other config bit (notably bit 6, scancode-
    /// set translation, which the driver relies on) is preserved by the
    /// read-modify-write in [`enable_keyboard_interrupts`].
    const CONFIG_KEYBOARD_INTERRUPT: u8 = 1 << 0;

    // ---- External-IRQ wiring ----

    /// GSI the keyboard delivers on. QEMU's q35/PIIX firmware leaves the
    /// legacy ISA IRQ-1 (i8042 keyboard) identity-mapped to GSI 1 — only
    /// the timer (IRQ-0) carries an MADT `InterruptSourceOverride`
    /// (`source = 0 → gsi = 2`). The boot pipeline programs every
    /// IO-APIC pin masked with a vector from the `0x30..=0xFE` range, so
    /// GSI 1 is left bound to a vector and `masked = true`.
    const KEYBOARD_GSI: u32 = 1;
    /// Synthesised owner for the keyboard IRQ binding. No real task runs
    /// in this test; the bind only needs an opaque owner identity.
    const IRQ_OWNER: SecTaskId = SecTaskId(0);
    /// Polling deadline for the [`WaitStep`] loop, in nanoseconds against
    /// the synthetic 1 GHz [`rdtsc_ns`] clock. One second is three orders
    /// of magnitude longer than the sub-millisecond IRQ latency, so a
    /// healthy run never observes `WaitStep::TimedOut`; the deadline only
    /// exists so a wedged line fails loud instead of hanging.
    const WAIT_DEADLINE_NS: u64 = 1_000_000_000;
    /// Master-PIC command/data port pair used to mask the legacy 8259s.
    const PIC_MASTER_DATA: u16 = 0x21;
    /// Slave-PIC data port.
    const PIC_SLAVE_DATA: u16 = 0xA1;
    /// "All lines masked" OCW1 image written to both PIC data ports.
    const PIC_MASK_ALL: u8 = 0xFF;

    // ---- Mock fixtures (no_std) ----

    /// Always returns the baked-in [`PS2_IMAGE`] regardless of path.
    struct BakedSource;

    impl ImageSource for BakedSource {
        fn read(&self, _path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
            buf.extend_from_slice(PS2_IMAGE);
            Ok(())
        }
    }

    /// Spawner that registers every manifest in-process through the real
    /// PS/2 driver entry point, so the host's load path runs the
    /// production `tairix_drv_input_ps2::register` capability gate.
    struct ResolvePs2;
    impl DriverSpawner for ResolvePs2 {
        fn spawn_and_register(
            &self,
            ctx: &SpawnContext<'_>,
        ) -> Result<DriverHandle, SpawnRegisterError> {
            tairix_drv_input_ps2::register(ctx.host).map_err(SpawnRegisterError::Register)
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

    /// Set the keyboard-interrupt bit in the i8042 command/config byte,
    /// preserving every other bit (read-modify-write through the `0x20` /
    /// `0x60` command pair). Without it the controller fills the output
    /// buffer but never asserts IRQ-1, so the interrupt-driven wait below
    /// would never wake. Returns `false` if any controller wait bound is
    /// hit.
    fn enable_keyboard_interrupts(io: X86PortIo8) -> bool {
        if !wait_input_clear(io) {
            return false;
        }
        io.write8(STATUS_PORT, CMD_READ_CONFIG);
        if !wait_output_full(io) {
            return false;
        }
        let config = io.read8(DATA_PORT);
        if !wait_input_clear(io) {
            return false;
        }
        io.write8(STATUS_PORT, CMD_WRITE_CONFIG);
        if !wait_input_clear(io) {
            return false;
        }
        io.write8(DATA_PORT, config | CONFIG_KEYBOARD_INTERRUPT);
        // The `0x20` response left the config byte in the output buffer;
        // drop it (and anything else stale) so the first injected
        // scancode is the only byte the driver sees.
        drain_controller(io);
        true
    }

    /// Mask every line on the legacy 8259 PIC pair so the keyboard pulse
    /// only delivers through the IO-APIC (QEMU leaves the PIC in its
    /// power-on state). Uses the same `PortIo8` backend the driver reads
    /// through rather than a raw `outb`.
    fn mask_legacy_pic(io: X86PortIo8) {
        io.write8(PIC_MASTER_DATA, PIC_MASK_ALL);
        io.write8(PIC_SLAVE_DATA, PIC_MASK_ALL);
    }

    /// Enable maskable interrupts on the current CPU.
    ///
    /// SAFETY: `Phase::Irq` of the boot pipeline fully populated the IDT
    /// (every external-vector slot points at the asm trampoline) and
    /// installed the production external-IRQ dispatcher; the legacy PIC
    /// is masked and only GSI 1 has been unmasked at the IO-APIC. A
    /// delivery here is routed through the same Rust dispatcher every
    /// production interrupt uses. `sti` itself only sets `EFLAGS.IF`.
    #[inline]
    unsafe fn sti() {
        unsafe {
            core::arch::asm!("sti", options(nomem, nostack, preserves_flags));
        }
    }

    /// Disable maskable interrupts on the current CPU.
    ///
    /// SAFETY: `cli` only clears `EFLAGS.IF`; a well-defined privileged
    /// instruction with no other side effects at CPL 0.
    #[inline]
    unsafe fn cli() {
        unsafe {
            core::arch::asm!("cli", options(nomem, nostack, preserves_flags));
        }
    }

    /// Park the CPU until the next interrupt fires.
    ///
    /// SAFETY: `hlt` requires CPL 0 and waits for the next unmasked
    /// interrupt. After the keyboard pulse is armed and `sti` has run the
    /// line is guaranteed to fire; the `hlt` returns when its ISR exits
    /// (or when the always-on LAPIC timer ticks), and the deadline bound
    /// in the wait loop is the backstop.
    #[inline]
    unsafe fn hlt() {
        unsafe {
            core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
        }
    }

    /// Read the TSC and convert to nanoseconds against a synthetic 1 GHz
    /// frequency. The `WaitStep` deadline is three orders of magnitude
    /// longer than the real IRQ latency, so the exact TSC frequency does
    /// not affect the pass/fail decision (mirrors the `irq_qemu_x86_64`
    /// vertical, which cannot reach the boot-measured `Calibration` from
    /// outside `KernelState`).
    fn rdtsc_ns() -> u64 {
        // SAFETY: RDTSC is unprivileged on every x86_64 CPU TAIRiX
        // supports and has no architectural side effects beyond producing
        // the timestamp.
        unsafe {
            let lo: u32;
            let hi: u32;
            core::arch::asm!(
                "rdtsc",
                out("eax") lo,
                out("edx") hi,
                options(nomem, nostack, preserves_flags),
            );
            (u64::from(hi) << 32) | u64::from(lo)
        }
    }

    /// Bind the keyboard line in the production `IrqTable`, mask the
    /// legacy PIC, and enable the controller's keyboard-interrupt bit.
    ///
    /// Returns the minted [`IrqHandle`] on success, or `None` on any
    /// environment defect (no published table/controller, GSI 1 not
    /// programmed, bind rejected, or a wedged controller) — the caller
    /// fails closed.
    fn setup_keyboard_irq() -> Option<IrqHandle> {
        let table = published_irq_table()?;
        // The controller must be published, and GSI 1 must carry a vector
        // the boot pipeline allocated; a `None` here means QEMU advertised
        // an MADT without the expected IO-APIC pin, an environment defect.
        published_typed()?;
        arch_irq::global_routing().vector_for_gsi(KEYBOARD_GSI)?;

        let outcome = table.bind(KEYBOARD_GSI, IRQ_OWNER).ok()?;

        let io = x86_port_io8();
        mask_legacy_pic(io);
        if !enable_keyboard_interrupts(io) {
            return None;
        }
        Some(outcome.handle)
    }

    /// Inject one scancode, wait for the keyboard IRQ to drive
    /// `IrqTable::fire`, then drain and decode the byte through a fresh
    /// `Ps2Keyboard`, confirming exactly one `Key` event with the
    /// expected `code`/`value`.
    ///
    /// The boot pipeline (and `IrqTable::fire`'s mask-before-wake step)
    /// leave GSI 1 masked, so each call re-unmasks it before arming the
    /// pulse.
    fn await_keypress_and_expect(handle: IrqHandle, scancode: u8, code: u16, value: i32) -> bool {
        let Some(table) = published_irq_table() else {
            return false;
        };
        let Some(controller) = published_typed() else {
            return false;
        };
        let io = x86_port_io8();

        // Clear any stale byte, then unmask the line and arm the pulse.
        drain_controller(io);
        if controller.unmask(KEYBOARD_GSI).is_err() {
            return false;
        }
        if !inject_scancode(io, scancode) {
            return false;
        }

        // Enable interrupts and wait for the IO-APIC → LAPIC → IDT →
        // dispatcher → `IrqTable::fire` round-trip to report `Ready`.
        // SAFETY: see `sti`.
        unsafe { sti() };
        let deadline_ns = rdtsc_ns().saturating_add(WAIT_DEADLINE_NS);
        let ready = loop {
            match table.try_wait_step(handle, IRQ_OWNER, rdtsc_ns(), deadline_ns) {
                WaitStep::Ready => break true,
                // SAFETY: see `hlt`.
                WaitStep::Continue => unsafe { hlt() },
                WaitStep::TimedOut | WaitStep::NotFound => break false,
            }
        };
        // SAFETY: see `cli`.
        unsafe { cli() };
        if !ready {
            return false;
        }

        // The dispatcher only EOIs and flips the ready flag; the scancode
        // is still in the output buffer for the driver's polled drain.
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
                ev.kind == InputEventKind::Key && ev.code == code && ev.value == value
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
        let spawner = ResolvePs2;
        let cfg = HostConfig {
            trusted_signers: &trusted,
            syscall_table_hash: SYSCALL_TABLE_HASH,
            accepted_abi_version: tairix_abi::ABI_VERSION_CURRENT,
            source: &source,
            spawner: &spawner,
            sink: &SerialSink::new(),
            // The PS/2 driver consumes no virtio transport.
            virtio_host_factory: None,
            mmio_mapper: None,
        };
        let mut host = Host::new(cfg);

        // load: the signed PS/2 `.rxe` clears the load gate through
        // the real `tairix_drv_input_ps2::register`.
        let Ok(h1) = host.load("/d/ps2", &caller) else {
            qemu_exit::exit_failure();
        };
        if host.loaded_count() != 1 || host.snapshot()[0].handle != h1 {
            qemu_exit::exit_failure();
        }

        // Bind the keyboard line in the production IrqTable and arm the
        // controller + PIC so the injected scancode delivers a real IRQ.
        let Some(irq_handle) = setup_keyboard_irq() else {
            qemu_exit::exit_failure();
        };

        // use: inject a key *press* and decode it once the keyboard IRQ
        // reports `WaitStep::Ready`.
        if !await_keypress_and_expect(irq_handle, SCANCODE_A_MAKE, u16::from(SCANCODE_A_MAKE), 1) {
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
        // decode it (same keycode, value 0) on the next keyboard IRQ.
        if !await_keypress_and_expect(irq_handle, SCANCODE_A_BREAK, u16::from(SCANCODE_A_MAKE), 0) {
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

    /// Panic handler — forwards through `tairix_kernel`'s shared bridge.
    #[panic_handler]
    fn ps2_qemu_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_kernel_core(info)
    }

    /// Boot entry point — same surface the production `tairix-kernel`
    /// bin exposes, but with our audit sink in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(multiboot_info: u64) -> ! {
        boot(
            multiboot_info,
            &SERIAL_SINK,
            &AUDIT_SINK,
            tairix_log::Level::Info,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_x86_64))]
fn main() {}
