//! Stage 4 QEMU integration test: boot the production `rustos-kernel`
//! pipeline to `AuditEvent::BootCompleted`, instantiate
//! `rustos_drvhost::Host`, drive a baked-in signed mock `.rxe`
//! image through `load → snapshot → unload → reload`, and signal QEMU
//! success.
//!
//! The boot pipeline reuse pattern mirrors `tests/integration/
//! kernel_arch_boot/src/main.rs` (Stage 3a (c7-bin)): the audit sink
//! is the integration test's hook point, the rest of the kernel is
//! production code.
//!
//! On the host (non-`x86_64-unknown-none`) target the bin is a no-op
//! so that `cargo build --workspace` does not require the
//! `x86_64-unknown-none` toolchain at every check.

#![cfg_attr(all(target_arch = "x86_64", target_os = "none"), no_std)]
#![cfg_attr(all(target_arch = "x86_64", target_os = "none"), no_main)]
#![deny(missing_docs)]

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
mod fixture {
    //! Pull in the build-time generated mock driver fixture.
    include!(concat!(env!("OUT_DIR"), "/mock_fixture.rs"));
}

// --- Freestanding test bin (`x86_64-unknown-none`) -----------------

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
mod kernel {
    extern crate alloc;

    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicBool, Ordering};

    use alloc::vec::Vec;
    use rustos_abi::{CapabilityId, DriverError, DriverHandle, DriverHost, DriverManifest, Errno};
    use rustos_arch_x86_64::qemu_exit;
    use rustos_caps::CapabilitySet;
    use rustos_crypto::Ed25519PublicKey;
    use rustos_drvhost::{EntryResolver, Host, HostConfig, ImageSource};
    use rustos_kernel::bumpalloc::{Heap, HEAP_BYTES};
    use rustos_kernel::{
        boot, handle_panic_via_kernel_core, BumpAllocator, SerialSink, SERIAL_SINK,
    };
    use rustos_log::{Event, EventId, Sink};

    use crate::fixture::{MOCK_IMAGE, SYSCALL_TABLE_HASH, TRUSTED_SIGNER_PUBKEY};

    /// Static heap backing the bump allocator. Sized identically to
    /// `kernel_arch_boot`'s — the test workload is similar (a single
    /// boot pipeline plus a handful of `Vec` allocations from the
    /// host's load path).
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

    /// Latch so the drvhost exercise runs exactly once.
    static DRVHOST_RAN: AtomicBool = AtomicBool::new(false);

    // ---- Mock fixtures (no_std) ----

    /// Always returns the baked-in `MOCK_IMAGE` regardless of path.
    struct BakedSource;

    impl ImageSource for BakedSource {
        fn read(&self, _path: &str, buf: &mut Vec<u8>) -> Result<(), Errno> {
            buf.extend_from_slice(MOCK_IMAGE);
            Ok(())
        }
    }

    fn mock_register(_host: &dyn DriverHost) -> Result<DriverHandle, DriverError> {
        // The freshly-minted handle is the host's own; the value
        // returned here is informational. Pick any non-zero u64.
        DriverHandle::from_raw(0x00C0_FFEE)
    }

    /// Resolver that binds every manifest to [`mock_register`].
    struct AlwaysOk;
    impl EntryResolver for AlwaysOk {
        fn resolve(
            &self,
            _manifest: &DriverManifest,
            _payload: &[u8],
        ) -> Option<rustos_drvhost::DriverEntry> {
            Some(mock_register as rustos_drvhost::DriverEntry)
        }
    }

    /// Exercise the drvhost public surface on the boot completion edge.
    fn drive_drvhost() {
        // Build the host's trust anchor list.
        let Ok(pubkey) = Ed25519PublicKey::from_bytes(&TRUSTED_SIGNER_PUBKEY) else {
            qemu_exit::exit_failure();
        };
        let trusted = [pubkey];

        // Caller capability set: hold CAP_DRV_LOAD only.
        let mut caller = CapabilitySet::empty();
        caller.insert(CapabilityId::DRV_LOAD);

        let source = BakedSource;
        let resolver = AlwaysOk;
        let cfg = HostConfig {
            trusted_signers: &trusted,
            syscall_table_hash: SYSCALL_TABLE_HASH,
            accepted_abi_version: rustos_abi::ABI_VERSION_CURRENT,
            source: &source,
            resolver: &resolver,
            sink: &SerialSink::new(),
            // Stage 4.D Item 0-tail: this integration runs against a
            // bumpalloc-backed kernel that has no kernel-side
            // `DmaPool` yet (the per-process DMA facility is wired
            // separately in the production binary). `None` keeps the
            // pre-Item-0-tail behaviour: the mock driver loaded
            // below does not consume virtio.
            virtio_host_factory: None,
        };
        let mut host = Host::new(cfg);

        // load → snapshot → unload → reload — every transition flips
        // `qemu_exit::exit_failure` on any error so a misbehaving host
        // is loud, not silent (`AGENTS.md` §7 — no flaky tests).
        let Ok(h1) = host.load("/d/mock", &caller) else {
            qemu_exit::exit_failure();
        };
        if host.loaded_count() != 1 {
            qemu_exit::exit_failure();
        }
        if host.snapshot()[0].handle != h1 {
            qemu_exit::exit_failure();
        }
        let Ok(h2) = host.reload(h1, &caller) else {
            qemu_exit::exit_failure();
        };
        if h2 == h1 || host.loaded_count() != 1 {
            qemu_exit::exit_failure();
        }
        if host.unload(h2).is_err() {
            qemu_exit::exit_failure();
        }
        if host.loaded_count() != 0 {
            qemu_exit::exit_failure();
        }
    }

    /// Audit observer sink. Forwards every event to [`SerialSink`] and,
    /// on observing `BootCompleted`, exercises the driver host then
    /// flips QEMU to `exit_success`.
    struct BootObserverSink;
    impl Sink for BootObserverSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id == BOOT_COMPLETED_EVENT_ID && !DRVHOST_RAN.swap(true, Ordering::SeqCst) {
                drive_drvhost();
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: BootObserverSink = BootObserverSink;

    /// Panic handler — forwards through `rustos_kernel`'s shared bridge.
    #[panic_handler]
    fn drvhost_qemu_panic(info: &PanicInfo<'_>) -> ! {
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
#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn main() {}
