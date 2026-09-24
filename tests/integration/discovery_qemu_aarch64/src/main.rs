//! `plans/ZEROCONF.md` Z4 QEMU integration test: boot the *production*
//! aarch64 `tairix-kernel` pipeline on the `virt` board with the planted
//! whole-disk encrypted-root image, a virtio-net device attached, and the
//! harness-side **multicast DNS responder** on its `dgram` netdev, then prove
//! link-local discovery end to end: `dns-sd` through `lib/discovery`, the
//! discovery channel, `discoveryd`'s front, and the capability-empty decoder
//! that parsed the peer's datagrams.
//!
//! The disk is the shared net-tool fixture — the **standard** signed store
//! bundles, so the real `dns-sd` command and the `discoveryd` service PID 1
//! starts at boot are present, plus the signed virtio-net driver. It plants no
//! test-only bundle.
//!
//! ## Why the serial gates are a proof
//!
//! The runner types three lookups, each only once the one before printed what
//! the peer published: a browse of the peer's type (gated on the instance
//! label), a resolve of that instance (gated on its `SRV` target and port), and
//! a lookup of that host (gated on its address). None of those strings is in
//! any line typed before it, so only an answer that crossed the whole path can
//! print it.
//!
//! ## Why the PASS keys on the lookups' exits *then* the shell's exit
//!
//! The audit sink counts the three `dns-sd` exits and reports PASS on the
//! **next** audited `exit` — the shell's, typed only after the last marker
//! appeared, so every answer provably reached the transcript first.
//! `discoveryd`'s own start records reach the transcript for diagnosis; the
//! answers subsume them, since none can be printed without the service and its
//! decoder. The harness also requires the peer's verdict that the guest asked
//! the wire for every record the lookups need, so neither side can pass
//! alone.

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;
    use core::sync::atomic::{AtomicU8, Ordering};

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, EventId, FieldValue, Sink};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time: QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

    /// The audited `comm` of a `dns-sd` process: the bundle's name, and the
    /// command word the runner types.
    const DNS_SD_COMMAND: &str = "dns-sd";

    /// The lookups the runner types: the browse, the resolve, the host.
    const LOOKUPS: u8 = 3;

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

    /// `EventId` the syscall dispatcher emits for an audited syscall that
    /// passed every check (pinned in `kernel/syscall/src/audit.rs`).
    const SYSCALL_INVOKED_EVENT_ID: EventId = EventId(5000);

    /// The `dns-sd` exits seen so far.
    static LOOKUPS_EXITED: AtomicU8 = AtomicU8::new(0);

    /// The string value of `event`'s field `key`, if present.
    fn field_str<'e>(event: &Event<'e>, key: &str) -> Option<&'e str> {
        event.fields.iter().find_map(|field| match field.value {
            FieldValue::Str(s) if field.key == key => Some(s),
            _ => None,
        })
    }

    /// Sink that replays every audit record through the serial sink and
    /// reports PASS on the first audited `exit` after the last lookup's.
    struct DiscoverySink;

    impl Sink for DiscoverySink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id != SYSCALL_INVOKED_EVENT_ID || field_str(event, "sc") != Some("exit") {
                return;
            }
            if field_str(event, "comm") == Some(DNS_SD_COMMAND) {
                LOOKUPS_EXITED.fetch_add(1, Ordering::AcqRel);
            } else if LOOKUPS_EXITED.load(Ordering::Acquire) >= LOOKUPS {
                qemu_exit::exit_success();
            }
        }
    }

    static AUDIT_SINK: DiscoverySink = DiscoverySink;

    /// Forward to the shared aarch64 panic bridge. A panic before the PASS
    /// parks the CPU and the run times out: the fail-loud outcome.
    #[panic_handler]
    fn tairix_discovery_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point, called by the arch crate's `boot.s` trampoline.
    ///
    /// The embedded `virt` blob stands in for the DTB pointer QEMU does not
    /// hand over. `SyscallInvoked` is a `Debug` record, so the filter is
    /// lowered for this observer to count it.
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

#[cfg(not(itest_aarch64))]
fn main() {}
