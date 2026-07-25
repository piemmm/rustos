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
//! ## Why the PASS keys on eleven witnesses
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
//! 8. `AuditEvent::SyscallInvoked` (`EventId` 5000) with `sc=fd_grant`,
//!    observed after the FM9-a rename (`plans/NEW-FILEMANAGER.md` FM9-b):
//!    the desktop session launched the Viewer from the start menu, the
//!    Viewer (handed no document) asked the session's trusted file picker
//!    (`plans/APPWIN.md` AW5), the picker opened at the user's home
//!    `/Users/root`, and a scripted click on the planted document row
//!    concluded the pick — so the session delegated the user's chosen file
//!    to the Viewer through the CU6 one-shot `fd_grant`. In this image the
//!    trusted picker is the only `fd_grant` caller, so the witness is
//!    unambiguous.
//! 9. `AuditEvent::SyscallInvoked` with `sc=fd_redeem` (after witness 8):
//!    the Viewer redeemed the one-shot delegation handle, installing the
//!    delegated read-only descriptor under the session's captured identity
//!    — it now reads exactly the one file the user chose, with no
//!    filesystem capability of its own. The CU6 delegation, end to end and
//!    kernel-attested; the pick-click is gated on the test kernel's
//!    picker-open marker so it lands only once the picker is composited.
//! 10. `AuditEvent::FsNodeMutated` with `op=rename` whose `to` is under
//!     `<home>/Library/Trash/` (after witness 9): the file-manager stage
//!     (`plans/NEW-FILEMANAGER.md` FM9-c/FM10) right-clicked the FM9-a folder
//!     to open the context menu, clicked its **Delete** row to open the
//!     confirmation dialog, and clicked the dialog's Delete button — and
//!     because the folder is in the user's home, on the same volume as the
//!     user's Trash, the confirmed delete is a recoverable **move to Trash**
//!     (one real permission-checked `fs_rename` into `<home>/Library/Trash/`)
//!     under the logged-in user's own identity. It is gated on the FM9-b
//!     delegation (witness 9) **and** a `Library/Trash` destination, so it
//!     fires strictly after the whole FM9-a/-b sequence and no earlier
//!     mutation can satisfy it; the right-click reaches the app through the
//!     fixed `tools/qemu` secondary-button mask and the compositor's
//!     secondary-press routing.
//! 11. `AuditEvent::FsNodeMutated` with `op=rmdir` whose `path` is under
//!     `<home>/Library/Trash/` (after witness 10): the file-manager stage
//!     (`plans/NEW-FILEMANAGER.md` FM11) clicked the **Go to Trash** tool to
//!     navigate into the user's Trash (now holding the FM10-trashed folder),
//!     clicked the **Empty Trash** tool, and confirmed the *Delete
//!     Permanently* dialog — so the manager permanently removed the trashed
//!     folder (`fs_unlink` with the directory-only flag) under the user's own
//!     identity. It is gated on the FM10 move having latched (witness 10), so
//!     no earlier removal — of the folder before it reached Trash, or any
//!     boot/login rmdir — can satisfy it (fail closed). The empty clicks are
//!     held behind the `FM11_TRASH_FILLED_MARKER` the test kernel emits once
//!     the move latches, so they land only after the folder is in the Trash.
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
//! reaches all eleven witnesses, so the harness times out — the documented
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

    /// The path fragment that identifies a rename destination as landing in the
    /// user's Trash (`plans/NEW-FILEMANAGER.md` FM10). The file manager spells
    /// its Trash destination from `lib/browse`'s `trash_dir` —
    /// `<home>/Library/Trash/<name>` — so a `FsNodeMutated op=rename` whose
    /// `to` contains this fragment is the recoverable move-to-Trash delete, and
    /// never the FM9-a naming rename (whose destination is the home folder).
    const TRASH_PATH_MARKER: &str = "/Library/Trash/";

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
    /// to QEMU once all eleven witnesses have appeared: the per-kind
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
    /// identity), the FM9-b CU6 delegation (`fd_grant` then `fd_redeem` —
    /// the session handing the user's chosen file to the Viewer), and the
    /// FM10 recoverable delete (`FsNodeMutated op=rename` into
    /// `<home>/Library/Trash/` after the delegation — the manager removing the
    /// FM9-a folder through its right-click context menu and confirmation
    /// dialog, which, the folder being on the same volume as the user's Trash,
    /// is carried out as a recoverable move to Trash rather than an
    /// irreversible unlink), and the FM11 empty-Trash removal
    /// (`FsNodeMutated op=rmdir` under `<home>/Library/Trash/` after the move —
    /// the manager navigating to the Trash via the Go-to-Trash tool, clicking
    /// Empty Trash, and confirming the *Delete Permanently* dialog, so the
    /// trashed folder is permanently removed). The guest exits only after the
    /// host has everything it needs (`plans/APPWIN.md` AW3 + AW4,
    /// `plans/NEW-FILEMANAGER.md` FM9-a/-b/FM10/FM11).
    struct AutoloadInputSink {
        key_delivered: AtomicBool,
        pointer_delivered: AtomicBool,
        users_db_loaded: AtomicBool,
        display_endpoint_bound: AtomicBool,
        window_events_delivered: AtomicU32,
        shell_round_trip: AtomicBool,
        fs_folder_created: AtomicBool,
        fs_folder_renamed: AtomicBool,
        fd_delegation_granted: AtomicBool,
        fd_delegation_redeemed: AtomicBool,
        fs_node_deleted: AtomicBool,
        fs_trash_emptied: AtomicBool,
        picker_open_marked: AtomicBool,
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
                fd_delegation_granted: AtomicBool::new(false),
                fd_delegation_redeemed: AtomicBool::new(false),
                fs_node_deleted: AtomicBool::new(false),
                fs_trash_emptied: AtomicBool::new(false),
                picker_open_marked: AtomicBool::new(false),
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
        /// record's `op` (and, for a rename, `to`; for a removal, `path`)
        /// fields: `mkdir`, then `rename` (FM9-a), then the FM9-c/FM10
        /// recoverable delete — a `rename` whose destination is under the
        /// user's `Library/Trash` — then the FM11 empty-Trash removal (an
        /// `rmdir`/`unlink` whose target is under `Library/Trash`).
        /// Only mutations that occur *after* the AW4 terminal round trip are
        /// counted (the delivery counter is at or past
        /// [`TERMINAL_ROUND_TRIP_DELIVERIES`] by then), so the many boot- and
        /// login-time directory creations — all strictly before the desktop
        /// click-through — can never latch these. The FM9-a `rename` requires
        /// `mkdir` first; the FM10 move-to-Trash requires the FM9-b delegation
        /// redeemed **and** a `Library/Trash` destination, so neither the
        /// FM9-a naming rename (its destination is the home folder, not Trash)
        /// nor any earlier mutation can satisfy it; the FM11 empty requires the
        /// FM10 move to have latched first, so no earlier removal can satisfy
        /// it. The refusal event (`FsMutationDenied`) is a different id that
        /// never reaches here (fail closed).
        fn note_fs_mutation(&self, event: &Event<'_>) {
            if self.window_events_delivered.load(Ordering::Acquire)
                < tairix_test_autoload_input_qemu_aarch64::TERMINAL_ROUND_TRIP_DELIVERIES
            {
                return;
            }
            // Read the `op`, the removal target `path`, and (for a rename) the
            // `to` destination once.
            let mut op = "";
            let mut path = "";
            let mut to = "";
            for field in event.fields {
                match (field.key, field.value) {
                    ("op", tairix_log::FieldValue::Str(value)) => op = value,
                    ("path", tairix_log::FieldValue::Str(value)) => path = value,
                    ("to", tairix_log::FieldValue::Str(value)) => to = value,
                    _ => {}
                }
            }
            match op {
                "mkdir" => {
                    self.fs_folder_created.store(true, Ordering::Release);
                }
                "rename" if self.fs_folder_created.load(Ordering::Acquire) => {
                    self.fs_folder_renamed.store(true, Ordering::Release);
                    // The FM10 move-to-Trash delete witness
                    // (`plans/NEW-FILEMANAGER.md` FM10): the file manager
                    // removed the FM9-a folder via the right-click context menu
                    // → confirm dialog. The folder is in the user's home, on
                    // the same volume as the user's Trash, so the confirmed
                    // delete is a recoverable `fs_rename` of the folder into
                    // `<home>/Library/Trash/` (in place of the old irreversible
                    // `rmdir`). It is gated on the FM9-b delegation having been
                    // redeemed **and** a `Library/Trash` destination, so it
                    // fires strictly after the whole FM9-a/-b sequence and
                    // neither the FM9-a naming rename (destination in the home
                    // folder, not Trash) nor any earlier boot/login mutation
                    // can satisfy it (fail closed — `FsMutationDenied` is a
                    // different id that never reaches here). The app spells the
                    // destination from `lib/browse`'s `trash_dir`
                    // (`<home>/Library/Trash`), so this marker matches it. On
                    // the *first* time the move latches, emit the deterministic
                    // marker the runner gates its FM11 empty-Trash clicks on,
                    // so those clicks land in a later wake — strictly after the
                    // trashed folder is in the Trash, never before.
                    if self.fd_delegation_redeemed.load(Ordering::Acquire)
                        && to.contains(TRASH_PATH_MARKER)
                        && !self.fs_node_deleted.swap(true, Ordering::AcqRel)
                    {
                        SerialSink::new().write_event(&Event {
                            level: tairix_log::Level::Info,
                            id: tairix_log::EventId(0),
                            message:
                                tairix_test_autoload_input_qemu_aarch64::FM11_TRASH_FILLED_MARKER,
                            fields: &[],
                        });
                    }
                }
                // The FM11 empty-Trash witness (`plans/NEW-FILEMANAGER.md`
                // FM11): the file manager navigated to the Trash and emptied
                // it, permanently removing the FM10-trashed folder — an
                // `fs_unlink` with the directory flag, so `op=rmdir`, whose
                // `path` is the trashed item under `<home>/Library/Trash/`.
                // Gated on the FM10 move having latched first, so no earlier
                // removal (of the folder before it reached Trash, or any
                // boot/login rmdir) can satisfy it (fail closed).
                "rmdir" | "unlink"
                    if self.fs_node_deleted.load(Ordering::Acquire)
                        && path.contains(TRASH_PATH_MARKER) =>
                {
                    self.fs_trash_emptied.store(true, Ordering::Release);
                }
                _ => {}
            }
        }

        /// Latch the file-manager open-a-file witnesses (`plans/NEW-FILEMANAGER.md`
        /// FM9-b) from a `SyscallInvoked` record's `sc` field: `fd_grant` then,
        /// once it has, `fd_redeem`. These fire only *after* the FM9-a rename
        /// committed (`fs_folder_renamed`), so no earlier delegation could
        /// satisfy them — and in this image the trusted file picker is the only
        /// `fd_grant` caller and the viewer the only `fd_redeem` caller, both
        /// reached only by this stage's scripted Viewer launch and pick. The
        /// grant is the session delegating the user's chosen file; the redemption
        /// is the viewer installing the delegated read-only descriptor — the CU6
        /// one-shot delegation, end to end and kernel-attested. `fd_redeem`
        /// requires `fd_grant` first, so a stray redemption cannot satisfy PASS
        /// on its own.
        fn note_syscall(&self, event: &Event<'_>) {
            if !self.fs_folder_renamed.load(Ordering::Acquire) {
                return;
            }
            // Read the record's `comm` (calling process) and `sc` (syscall
            // name) once; both are `SyscallInvoked` fields.
            let mut comm = "";
            let mut sc = "";
            for field in event.fields {
                match (field.key, field.value) {
                    ("comm", tairix_log::FieldValue::Str(value)) => comm = value,
                    ("sc", tairix_log::FieldValue::Str(value)) => sc = value,
                    _ => {}
                }
            }
            match sc {
                "fd_grant" => {
                    self.fd_delegation_granted.store(true, Ordering::Release);
                }
                "fd_redeem" if self.fd_delegation_granted.load(Ordering::Acquire) => {
                    self.fd_delegation_redeemed.store(true, Ordering::Release);
                }
                // The desktop session's first directory read after the FM9-a
                // rename is the trusted picker's `open_at` listing of the
                // user's home (done synchronously inside the `PickFile`
                // serve). Emit the deterministic marker the runner gates its
                // pick-click on, exactly once, so the click lands in a later
                // wake with the picker composited.
                "fs_open"
                    if comm == tairix_test_autoload_input_qemu_aarch64::SESSION_COMM
                        && !self.picker_open_marked.swap(true, Ordering::AcqRel) =>
                {
                    SerialSink::new().write_event(&Event {
                        level: tairix_log::Level::Info,
                        id: tairix_log::EventId(0),
                        message: tairix_test_autoload_input_qemu_aarch64::FM9B_PICKER_OPEN_MARKER,
                        fields: &[],
                    });
                }
                _ => {}
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
            } else if event.id.0 == tairix_kernel_syscall::AuditEvent::SyscallInvoked.id().0 {
                self.note_syscall(event);
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
                && self.fd_delegation_granted.load(Ordering::Acquire)
                && self.fd_delegation_redeemed.load(Ordering::Acquire)
                && self.fs_node_deleted.load(Ordering::Acquire)
                && self.fs_trash_emptied.load(Ordering::Acquire)
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
