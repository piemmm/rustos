//! `plans/PI.md` P10 5d-2-ii(b-2-iii) + `plans/DISPLAY.md` D7d (first
//! stage) QEMU integration test: boot the production aarch64
//! `tairix-kernel` pipeline on the `virt` board **as a display world** —
//! a `ramfb` display beside the virtio keyboard and mouse — with a
//! planted whole-disk encrypted-root image that carries the
//! **kernel-signed virtio-input driver bundle and the framebuffer
//! display-service bundle** in `/System/Drivers/`, and prove the full
//! **driver-loading-by-discovery autoload path**: one user-space driver
//! instance per discovered virtio-input node (keyboard + mouse)
//! delivering typed keys and an injected mouse motion to the kernel
//! input-focus arbiter, the typed passphrase unlocking the encrypted
//! root through the video console end to end, and the display service
//! autoloading against the boot display node the kernel publishes for
//! its ramfb scan-out surface and binding the reserved
//! `DISPLAY_ENDPOINT`.
//!
//! ## What this test asserts — and how it differs from its siblings
//!
//! * `root_unlock_admission_qemu_aarch64` proves the in-kernel unlock kthread
//!   mounts the encrypted root and installs the users database (the
//!   *root-mount* path). It attaches no keyboard and plants no driver store.
//! * `input_virtio_mmio_qemu_aarch64` proves a discovered virtio-input device
//!   reaches the *in-kernel* scaffold decode path.
//! * `driver_spawn_qemu_aarch64` proves a discovered node → signed gate →
//!   process spawn handshake with a stub program.
//!
//! This vertical composes them on the production boot path: it attaches the
//! shared `tairix_test_encrypted_root_image` whole-disk image, additionally
//! planted with the autoload driver bundles the `image_drivers` pipeline
//! cross-compiles and signs (a three-partition disk whose **read-only
//! `/System` volume** carries the signed `virtio_kbd` bundle at the
//! volume-relative `Drivers/input/virtio_kbd/Run` and the signed framebuffer
//! display-service bundle at `Drivers/display/framebuffer/Run`, design B) as a
//! virtio-blk-mmio device
//! **plus** a `ramfb` display, a `virtio-keyboard-device`, and a
//! `virtio-mouse-device`, and boots `boot_aarch64::boot` verbatim. The
//! production path then:
//!
//! 1. **Discovers** the virtio-block root *and* the virtio-input nodes
//!    (bootstrap-floor virtio-MMIO enumeration). Each input node carries its
//!    register window, a coherent DMA constraint, **and** its discovered GICv2
//!    interrupt line as capability-grant requests; the framebuffer boot
//!    console comes up on the `ramfb` scan-out and the boot publishes the
//!    surface as the boot display node (a `Framebuffer` grant request keyed
//!    `simple-framebuffer`); the full hardware tree is stashed for the init
//!    seam.
//! 2. **Admits the unlock kthread**, which brings the root block device up over
//!    the device-IRQ path, mounts the read-only `/System` volume, and serves
//!    its signed driver store over the capability-gated IPC endpoint.
//! 3. **Reactive user-space autoload (Design D)**: the long-running
//!    `devmgr` service reads the hardware tree, lists the `/System` store over
//!    the IPC service, matches each signed bundle to its discovered node
//!    (`lib/devmatch` — the `virtio_kbd` bundle to each virtio-input node,
//!    the display bundle to the boot display node), and asks the kernel to
//!    load each; the kernel re-runs the full signed gate (verified against
//!    the embedded `KERNEL_DRIVER_SIGNER_PUBKEY`) and **spawns each into its
//!    own user-space process** with exactly its node's resource grants.
//! 4. Each spawned driver instance maps its register window, brings its
//!    virtio-input device up, **then binds its granted interrupt line and
//!    parks on `irq_wait`** (interrupt-driven, never a busy poll; the bind
//!    is `VirtioInput::open_armed`'s arm step, issued only once the eventq
//!    is live so the audited bind is a truthful readiness witness), and on
//!    each device interrupt pumps decoded events into the arbiter — key
//!    edges via `key_inject`, pointer records via `pointer_inject`.
//!
//! ## Why the PASS keys on six witnesses
//!
//! The audit sink reports PASS once it has seen all of:
//!
//! 1. `AuditEvent::InputDelivered` (`EventId` 4050) with `kind=key` — the
//!    one-shot witness the `key_inject` handler emits the first time a
//!    keyboard-class driver delivers to the arbiter; here, the first
//!    typed passphrase character.
//! 2. `AuditEvent::InputDelivered` with `kind=pointer` — its pointer
//!    sibling, from the injected mouse motion (the shared
//!    `PointerInput::from_device_event` mapping).
//! 3. `AuditEvent::UsersDbLoaded` — the users database was read off the
//!    unlocked encrypted root, so the passphrase **typed at the virtio
//!    keyboard** traversed the seat text sink, the video console's
//!    keyboard queue, and the unlock kthread's prompt, and unlocked the
//!    root end to end (the typed-dialogue facility the D7d login stage
//!    builds on).
//! 4. The kernel/ipc `CallEndpointCreated` (`EventId` 3040) whose
//!    `endpoint` field is the reserved `DISPLAY_ENDPOINT` — the
//!    autoloaded framebuffer display service resolved its granted
//!    scan-out surface and bound its rendezvous under
//!    `CAP_IPC_BIND_PRIVILEGED` (only the display service may bind a
//!    reserved endpoint id, so the witness is unforgeable by any other
//!    process in the image).
//! 5. The `appmgr` `APP_LOADED` record for [`TERMINAL_ROUND_TRIP_BUNDLE`]
//!    (`/System/Apps/sleep.app`) — the shell resolved and ran the typed
//!    command, attributed by the loaded bundle's own name rather than a
//!    fragile delivery count: the
//!    desktop session — logged in at the seat keyboard and driven by
//!    injected pointer clicks — opened its start menu, spawned the files
//!    bundle from the on-disk system app store, served its window over
//!    the reserved window rendezvous, toggled the appearance, routed the
//!    scripted in-window clicks app-ward (`plans/APPWIN.md` AW3), then
//!    spawned the terminal bundle, focused its served window, and
//!    delivered the typed command's every key edge — whereupon the
//!    windowed terminal wrote the line to its hosted shell over its
//!    pipe, and the shell resolved and **spawned** the typed program:
//!    the AW4 shell round trip, every hop kernel-attested (the only
//!    spawn that can occur after the typing gate is the shell executing
//!    the typed command).
//! 6. The `appmgr` `APP_LOADED` record for [`CTRL_C_RECOVERY_BUNDLE`]
//!    (`/System/Apps/true.app`), after the `sleep` round trip — the pty
//!    `Ctrl-C` job-control round trip (`plans/PTY.md`), attributed by the
//!    recovered bundle's own name. Once witness 5's `sleep` load latched the
//!    guest emitted [`CTRL_C_ARM_MARKER`], the runner injected a `Ctrl-C`
//!    (which the terminal encodes as the `0x03` interrupt byte through the
//!    shared `lib/keymap` rule) and then [`TERMINAL_CTRL_C_RECOVERY`]'s
//!    `true` + Enter. The shell is parked in `wait` on `sleep`, so it can
//!    load and run `true` only once the pty's cooked-mode line discipline
//!    signalled the foreground `sleep` dead — an end-to-end witness of
//!    keyboard → session → terminal → pty cooked `^C` → foreground
//!    `Signal::Interrupt` → job death → shell recovery, every hop
//!    kernel-attested. A failed interrupt leaves `sleep` blocking past the
//!    run budget, so the witness never latches and the run times out (fail
//!    loud).
//!
//! [`TERMINAL_ROUND_TRIP_BUNDLE`]: tairix_test_autoload_input_qemu_aarch64::TERMINAL_ROUND_TRIP_BUNDLE
//! [`CTRL_C_RECOVERY_BUNDLE`]: tairix_test_autoload_input_qemu_aarch64::CTRL_C_RECOVERY_BUNDLE
//! [`CTRL_C_ARM_MARKER`]: tairix_test_autoload_input_qemu_aarch64::CTRL_C_ARM_MARKER
//! [`TERMINAL_CTRL_C_RECOVERY`]: tairix_test_autoload_input_qemu_aarch64::TERMINAL_CTRL_C_RECOVERY
//!
//! Reaching them requires every preceding step to have succeeded: the
//! `/System` volume mounted and served, the store listed, each signed
//! bundle verified, each node matched (the virtio-input transports and
//! the boot display node the kernel publishes for its ramfb surface), one
//! user-space process spawned per matched node with exactly its node's
//! grants, each device brought up, the typed keys decoded and delivered,
//! the passphrase accepted, and the display service's surface resolved
//! from its `Framebuffer` grant. The harness types only once both input
//! driver instances have armed their interrupts (the audited `irq_bind`
//! syscall, twice), and injects the mouse motion only once the key
//! witness's `kind=key` line appears on serial — so each witness is
//! attributable to its own injection. A run where any step fails never
//! reaches all six witnesses, so the harness times out — the documented
//! fail-loud behaviour.
//!
//! ## Embedded `virt` device tree
//!
//! QEMU's `-kernel <ELF>` aarch64 path passes no DTB pointer (`x0 = 0`), so the
//! canonical `virt` device tree is dumped and embedded at build time
//! (`build.rs`) and its address handed to the boot pipeline. The tree describes
//! the board's `virtio,mmio` transport slots; the planted disk and the attached
//! keyboard populate two slots' live `DeviceID`s, which the bootstrap-floor
//! enumeration reads.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline and only replaces the
//! audit sink. Splitting the audit-observer behaviour into a separate bin
//! (instead of a Cargo feature on a production crate) prevents feature
//! unification from leaking the QEMU-exit shortcut into any production build
//! (fail closed; the harness never decides what the kernel
//! does next).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_kernel_core::AuditEvent;
    use tairix_log::{Event, Sink};
    use tairix_util::fmt::format_hex_u64;

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

    /// Sink that replays every event through [`SERIAL_SINK`] and reports PASS
    /// to QEMU once all six witnesses have appeared: the per-kind
    /// first-input-delivery one-shots (`kind=key` and `kind=pointer` — the
    /// autoloaded user-space virtio-input driver instances delivering), the
    /// users-database load (the passphrase typed at the virtio keyboard
    /// unlocked the encrypted root end to end), the reserved
    /// `DISPLAY_ENDPOINT` bind (the autoloaded framebuffer display service
    /// came up on its granted surface), the AW4 shell round trip (the
    /// `appmgr` load of `/System/Apps/sleep.app` — the windowed terminal
    /// received every typed key edge, wrote the line to its hosted shell, and
    /// the shell resolved and ran the typed program), and the pty `Ctrl-C`
    /// job-control round trip (the `appmgr` load of `/System/Apps/true.app`
    /// after the `sleep` round trip — the recovered `true` the shell could
    /// only run once `Ctrl-C` interrupted the parked foreground `sleep`,
    /// `plans/PTY.md`). The guest exits only after the host has everything it
    /// needs (`plans/APPWIN.md` AW3 + AW4, `plans/PTY.md`).
    ///
    /// The file-manager stages (`plans/NEW-FILEMANAGER.md` FM9-a/-b/-c,
    /// FM10, FM11) are deliberately *not* driven here: that application UI
    /// logic is proven by `lib/browse`'s host unit tests, and folding a
    /// long, blind pointer-injection choreography of it into this vertical
    /// added only fragility (the FONT-SERVICE delivery-count drift of
    /// `plans/OPEN-DEFECTS.md` D20). This vertical proves what only QEMU
    /// can: driver autoload, encrypted-root unlock, display bind, and the
    /// keyboard → session → terminal → pty → shell round trip.
    struct AutoloadInputSink {
        key_delivered: AtomicBool,
        pointer_delivered: AtomicBool,
        users_db_loaded: AtomicBool,
        display_endpoint_bound: AtomicBool,
        /// First distinct window-event destination port (the files window);
        /// `0` until the first app-ward delivery.
        first_window_port: AtomicU64,
        /// One-shot: [`TERMINAL_FOCUSED_MARKER`] emitted on the first delivery
        /// to the second distinct window port (the terminal gaining focus).
        terminal_focus_marked: AtomicBool,
        shell_round_trip: AtomicBool,
        ctrl_c_recovered: AtomicBool,
    }

    impl AutoloadInputSink {
        const fn new() -> Self {
            Self {
                key_delivered: AtomicBool::new(false),
                pointer_delivered: AtomicBool::new(false),
                users_db_loaded: AtomicBool::new(false),
                display_endpoint_bound: AtomicBool::new(false),
                first_window_port: AtomicU64::new(0),
                terminal_focus_marked: AtomicBool::new(false),
                shell_round_trip: AtomicBool::new(false),
                ctrl_c_recovered: AtomicBool::new(false),
            }
        }

        /// Latch the per-kind input witness from an `InputDelivered`
        /// record's `kind` field; an unrecognised value flips neither latch
        /// (fail closed — a malformed witness can never satisfy PASS).
        fn note_input_delivered(&self, event: &Event<'_>) {
            for field in event.fields {
                if field.key != "kind" {
                    continue;
                }
                match field.value {
                    tairix_log::FieldValue::Str("key") => {
                        self.key_delivered.store(true, Ordering::Release);
                    }
                    tairix_log::FieldValue::Str("pointer") => {
                        self.pointer_delivered.store(true, Ordering::Release);
                    }
                    _ => {}
                }
            }
        }

        /// Latch the terminal round-trip and the pty `Ctrl-C` job-control
        /// witnesses from an `appmgr` `APP_LOADED` record's `bundle` field,
        /// each attributed to the *exact bundle the shell loaded* — never a
        /// cumulative delivery count (the drift `plans/OPEN-DEFECTS.md` D20
        /// removed):
        ///
        /// * loading [`TERMINAL_ROUND_TRIP_BUNDLE`] (`sleep`) is the AW4 round
        ///   trip — the shell resolved and ran the typed command. On that
        ///   latch the guest emits [`CTRL_C_ARM_MARKER`] so the host runner
        ///   injects its `Ctrl-C` recovery step against a live, parked
        ///   foreground job (never before one exists).
        /// * loading [`CTRL_C_RECOVERY_BUNDLE`] (`true`) *after* that is the
        ///   recovered job the shell could reach only once `Ctrl-C`
        ///   interrupted the parked `sleep` (it is blocked in `wait` until
        ///   then), so it witnesses the pty cooked-mode job-control path end
        ///   to end (`plans/PTY.md`). On that latch the guest emits
        ///   [`CTRL_C_RECOVERED_MARKER`], the readiness boundary the whole FM9
        ///   file-manager stage waits on. `sleep` is loaded only by the typed
        ///   command and `true` only by the recovery, so each witness is
        ///   uniquely attributable — no overlapping `≥` threshold.
        fn note_bundle_loaded(&self, event: &Event<'_>) {
            let mut bundle = "";
            for field in event.fields {
                if field.key == "bundle" {
                    if let tairix_log::FieldValue::Str(value) = field.value {
                        bundle = value;
                    }
                }
            }
            if bundle == tairix_test_autoload_input_qemu_aarch64::TERMINAL_ROUND_TRIP_BUNDLE
                && !self.shell_round_trip.swap(true, Ordering::AcqRel)
            {
                // Arm the runner's Ctrl-C injection: a parked foreground job
                // (`sleep`) now exists to interrupt.
                SerialSink::new().write_event(&Event {
                    level: tairix_log::Level::Info,
                    id: tairix_log::EventId(0),
                    message: tairix_test_autoload_input_qemu_aarch64::CTRL_C_ARM_MARKER,
                    fields: &[],
                });
            } else if bundle == tairix_test_autoload_input_qemu_aarch64::CTRL_C_RECOVERY_BUNDLE
                && self.shell_round_trip.load(Ordering::Acquire)
            {
                // The recovered `true` loaded: `Ctrl-C` interrupted the parked
                // `sleep` and the shell ran its next command — the pty
                // job-control PASS witness. Latch it.
                self.ctrl_c_recovered.store(true, Ordering::Release);
            }
        }

        /// Latch the display-service witness when a `CallEndpointCreated`
        /// record names the reserved `DISPLAY_ENDPOINT` — compared against
        /// the exact hex spelling the kernel/ipc audit fields render
        /// (`format_hex_u64`), so the match can neither false-positive on a
        /// different endpoint nor drift from the emitter.
        fn note_endpoint_created(&self, event: &Event<'_>) {
            let mut expected_buf = [0u8; 16];
            let expected =
                format_hex_u64(tairix_abi::display_ipc::DISPLAY_ENDPOINT, &mut expected_buf);
            for field in event.fields {
                if field.key != "endpoint" {
                    continue;
                }
                if let tairix_log::FieldValue::Str(value) = field.value {
                    if value == expected {
                        self.display_endpoint_bound.store(true, Ordering::Release);
                    }
                }
            }
        }

        /// On the first app-ward window-event delivery to the *second*
        /// distinct destination port, emit [`TERMINAL_FOCUSED_MARKER`] once —
        /// the lone port sender serves the files window first and the terminal
        /// second, so a delivery to any port other than the first-seen one is
        /// the terminal gaining focus (the fact the typed command gates on,
        /// not a count).
        fn note_window_delivery(&self, event: &Event<'_>) {
            for field in event.fields {
                if field.key != "port" {
                    continue;
                }
                let tairix_log::FieldValue::Str(value) = field.value else {
                    continue;
                };
                let Ok(port) = u64::from_str_radix(value, 16) else {
                    continue;
                };
                let first = self.first_window_port.load(Ordering::Acquire);
                if first == 0 {
                    self.first_window_port.store(port, Ordering::Release);
                } else if port != first && !self.terminal_focus_marked.swap(true, Ordering::AcqRel)
                {
                    SerialSink::new().write_event(&Event {
                        level: tairix_log::Level::Info,
                        id: tairix_log::EventId(0),
                        message: tairix_test_autoload_input_qemu_aarch64::TERMINAL_FOCUSED_MARKER,
                        fields: &[],
                    });
                }
            }
        }
    }

    impl Sink for AutoloadInputSink {
        fn write_event(&self, event: &Event<'_>) {
            // Replay through the serial sink so the QEMU transcript records the
            // full boot + unlock + autoload + input timeline (the harness also
            // gates its mouse injection on the `kind=key` line of this replay).
            SerialSink::new().write_event(event);
            if event.id.0 == AuditEvent::InputDelivered.id().0 {
                self.note_input_delivered(event);
            } else if event.id.0 == AuditEvent::UsersDbLoaded.id().0 {
                self.users_db_loaded.store(true, Ordering::Release);
            } else if event.id.0 == tairix_kernel_ipc::AuditEvent::CallEndpointCreated.id().0 {
                self.note_endpoint_created(event);
            } else if event.id.0 == tairix_kernel_ipc::AuditEvent::MessageDelivered.id().0 {
                self.note_window_delivery(event);
            } else if event.id.0 == tairix_appload::events::APP_LOADED.0 {
                // Attributable by the loaded bundle's own name: `sleep` is
                // loaded only by the shell running the typed command (the AW4
                // round trip) and `true` only by the Ctrl-C recovery, so each
                // witness is unambiguous — no fragile delivery-count threshold.
                self.note_bundle_loaded(event);
            } else {
                return;
            }
            if self.key_delivered.load(Ordering::Acquire)
                && self.pointer_delivered.load(Ordering::Acquire)
                && self.users_db_loaded.load(Ordering::Acquire)
                && self.display_endpoint_bound.load(Ordering::Acquire)
                && self.shell_round_trip.load(Ordering::Acquire)
                && self.ctrl_c_recovered.load(Ordering::Acquire)
            {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: AutoloadInputSink = AutoloadInputSink::new();

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// finisher parks the CPU, the run times out, and the harness reports
    /// `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_autoload_input_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline with the
    /// audit-observer sink in place.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &SERIAL_SINK,
            &AUDIT_SINK,
            // `SyscallInvoked` (`EventId(5000)`) is `Debug`, below the
            // default `Info` filter; the harness waits for this record's
            // `sc=irq_bind` serial marker before injecting the key, so
            // boot with the filter lowered.
            tairix_log::Level::Debug,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}

#[cfg(not(itest_aarch64))]
#[allow(dead_code)]
fn _suppress_no_main() {}
