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
//! ## Why the PASS keys on seven witnesses
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
//! 5. A kernel `ProcessSpawned` audit record observed once the
//!    kernel/ipc `MessageDelivered` count has reached the crate's shared
//!    interaction contract ([`TERMINAL_ROUND_TRIP_DELIVERIES`]): the
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
//! 6. `AuditEvent::FsNodeMutated` (`EventId` 4100) with `op=mkdir`,
//!    observed *after* the terminal round trip: the file-manager stage
//!    (`plans/NEW-FILEMANAGER.md` FM9-a) refocused the files window,
//!    descended into `/Users/root` by scripted pointer clicks and seat-
//!    keyboard `Enter`s, and clicked the New Folder tool, so the app made
//!    a real permission-checked `fs_mkdir` under the logged-in user's own
//!    identity. The count gate excludes every boot- and login-time
//!    directory creation.
//! 7. `AuditEvent::FsNodeMutated` with `op=rename` (after witness 6): the
//!    inline rename committed a distinct name through `fs_rename` — the
//!    manager's create-then-name flow, end to end and kernel-attested.
//!    (A refused mutation logs `FsMutationDenied`, a different id that
//!    never satisfies these witnesses — fail closed.)
//!
//! [`TERMINAL_ROUND_TRIP_DELIVERIES`]: tairix_test_autoload_input_qemu_aarch64::TERMINAL_ROUND_TRIP_DELIVERIES
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
//! reaches all seven witnesses, so the harness times out — the documented
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
    use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

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
    /// to QEMU once all seven witnesses have appeared: the per-kind
    /// first-input-delivery one-shots (`kind=key` and `kind=pointer` — the
    /// autoloaded user-space virtio-input driver instances delivering), the
    /// users-database load (the passphrase typed at the virtio keyboard
    /// unlocked the encrypted root end to end), the reserved
    /// `DISPLAY_ENDPOINT` bind (the autoloaded framebuffer display service
    /// came up on its granted surface), the AW4 shell round trip (a
    /// `ProcessSpawned` record observed once the kernel/ipc
    /// `MessageDelivered` count has reached the typed command's Enter
    /// press — the crate's shared interaction contract — the windowed
    /// terminal received every typed key edge, wrote the line to its
    /// hosted shell, and the shell spawned the typed program), and the
    /// FM9-a file-manager mutations (`FsNodeMutated op=mkdir` then
    /// `op=rename`, both after the terminal round trip — the manager
    /// created and named a folder in `/Users/root` under the user's own
    /// identity). The guest exits only after the host has everything it
    /// needs (`plans/APPWIN.md` AW3 + AW4, `plans/NEW-FILEMANAGER.md`
    /// FM9-a).
    struct AutoloadInputSink {
        key_delivered: AtomicBool,
        pointer_delivered: AtomicBool,
        users_db_loaded: AtomicBool,
        display_endpoint_bound: AtomicBool,
        window_events_delivered: AtomicU32,
        shell_round_trip: AtomicBool,
        fs_folder_created: AtomicBool,
        fs_folder_renamed: AtomicBool,
    }

    impl AutoloadInputSink {
        const fn new() -> Self {
            Self {
                key_delivered: AtomicBool::new(false),
                pointer_delivered: AtomicBool::new(false),
                users_db_loaded: AtomicBool::new(false),
                display_endpoint_bound: AtomicBool::new(false),
                window_events_delivered: AtomicU32::new(0),
                shell_round_trip: AtomicBool::new(false),
                fs_folder_created: AtomicBool::new(false),
                fs_folder_renamed: AtomicBool::new(false),
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

        /// Latch the file-manager mutation witnesses from a `FsNodeMutated`
        /// record's `op` field: `mkdir` then, once it has, `rename`. Only
        /// mutations that occur *after* the AW4 terminal round trip are
        /// counted (the delivery counter is at or past
        /// [`TERMINAL_ROUND_TRIP_DELIVERIES`] by then), so the many boot- and
        /// login-time directory creations — all strictly before the desktop
        /// click-through — can never latch these. `rename` requires `mkdir`
        /// first, so a stray rename cannot satisfy PASS on its own, and the
        /// refusal event (`FsMutationDenied`) is a different id that never
        /// reaches here (fail closed).
        fn note_fs_mutation(&self, event: &Event<'_>) {
            if self.window_events_delivered.load(Ordering::Acquire)
                < tairix_test_autoload_input_qemu_aarch64::TERMINAL_ROUND_TRIP_DELIVERIES
            {
                return;
            }
            for field in event.fields {
                if field.key != "op" {
                    continue;
                }
                match field.value {
                    tairix_log::FieldValue::Str("mkdir") => {
                        self.fs_folder_created.store(true, Ordering::Release);
                    }
                    tairix_log::FieldValue::Str("rename")
                        if self.fs_folder_created.load(Ordering::Acquire) =>
                    {
                        self.fs_folder_renamed.store(true, Ordering::Release);
                    }
                    _ => {}
                }
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
            } else if event.id.0 == AuditEvent::FsNodeMutated.id().0 {
                self.note_fs_mutation(event);
            } else if event.id.0 == tairix_kernel_ipc::AuditEvent::MessageDelivered.id().0 {
                self.window_events_delivered.fetch_add(1, Ordering::AcqRel);
            } else if event.id.0 == AuditEvent::ProcessSpawned.id().0 {
                // Attributable by ordering, not by name: every other spawn
                // in the image happens strictly before the typing gate, so
                // a spawn observed at or beyond the Enter press's delivery
                // count can only be the shell executing the typed command
                // (the contract crate's rationale).
                if self.window_events_delivered.load(Ordering::Acquire)
                    >= tairix_test_autoload_input_qemu_aarch64::TERMINAL_ROUND_TRIP_DELIVERIES
                {
                    self.shell_round_trip.store(true, Ordering::Release);
                }
            } else {
                return;
            }
            if self.key_delivered.load(Ordering::Acquire)
                && self.pointer_delivered.load(Ordering::Acquire)
                && self.users_db_loaded.load(Ordering::Acquire)
                && self.display_endpoint_bound.load(Ordering::Acquire)
                && self.shell_round_trip.load(Ordering::Acquire)
                && self.fs_folder_created.load(Ordering::Acquire)
                && self.fs_folder_renamed.load(Ordering::Acquire)
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
