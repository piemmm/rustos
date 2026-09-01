//! `plans/TIMESYNC.md` TS-7 QEMU integration test: boot the production aarch64
//! `tairix-kernel` pipeline on the `virt` board against the shared
//! `dhcp-net-root` whole-disk image — whose read-only `/System` volume carries
//! the **kernel-signed virtio-net driver bundle** and a planted
//! `/System/Settings/Network/network.conf` selecting `ipv4.method dhcp`, and
//! which carries **no configured time server at all** — with a
//! `virtio-net-device` attached and the harness-side
//! **DHCP-server-plus-NTP-server** link peer on the QEMU `dgram` netdev, and
//! prove the wall clock is established from the time server the *lease* named.
//!
//! ## What this vertical asserts — and why it cannot pass by accident
//!
//! The guest's `time.servers` is empty, so the only server it can reach is
//! whichever one its DHCP lease named in RFC 2132 option 42 — the peer itself.
//! Its built-in fallback tier names the public NTP pool, whose hosts cannot
//! resolve (there is no DNS) and could not be reached (there is no route off
//! this wire) even if they did. So a guest that ignored option 42 sets no clock
//! and the run fails loud on the deadline; only a guest that read the option
//! out of its own lease, published it through the stack, and preferred it over
//! the fallback reaches the witness. That is the whole property under test, and
//! nothing else in the run can supply it.
//!
//! Two further discriminators come free with the shape:
//!
//! * The peer answers each time request **twice, spoof first** — a well-formed
//!   reply whose origin timestamp does not echo the request's CSPRNG nonce and
//!   which reports a plainly different instant, then the truthful reply. The
//!   witness requires the applied second to be the fixture's, so a guest that
//!   believed the spoof lands a million seconds away and a guest that let the
//!   spoof cancel its outstanding transaction never sets the clock at all.
//! * A network-supplied server arrives as an *address*, so no resolver is
//!   involved: this run also proves the learned tier needs no DNS, which is
//!   what makes it useful on a network whose only DNS advice came from the
//!   same lease.
//!
//! The production boot path (all **before** any root unlock — the `/System`
//! store and its `Settings/` config are on the read-only volume mounted before
//! the passphrase, so the guest needs no console dialogue):
//!
//! 1. **Discovers** the virtio-block root *and* the virtio-net node
//!    (bootstrap-floor virtio-MMIO enumeration), each carrying its register
//!    window, DMA constraint, and GICv2 interrupt line as capability-grant
//!    requests.
//! 2. **Autoloads** the signed virtio-net bundle from the mounted `/System`
//!    store into its own user-space process; the driver brings the device up
//!    and publishes a `netchan` hardware-tree node.
//! 3. **`devmgr`** observes the node, calls **`netstack`** `BindDriver`, and
//!    delivers the planted `wan` DHCPv4 configuration.
//! 4. **`netstack`** leases the interface its only address from the peer, and
//!    keeps the lease's option-42 time server in its live lease state.
//! 5. **`timed`**, which started on its fallback tier because nothing had
//!    named a server yet, re-selects on its bounded ladder, reads the learned
//!    server through the ungated `net_time_servers` system-information query
//!    `sysinfod` fronts, finds a strictly better tier, rebuilds on it, queries
//!    it, evaluates both replies in a capability-empty sandbox worker, and
//!    applies the validated sample.
//!
//! ## How the run completes
//!
//! There is no serial script: nothing in this chain needs a console dialogue.
//! The guest exits itself on `timed`'s `CLOCK_SET` audit record whose applied
//! `wall_secs=` is the fixture's sample
//! (`tairix_test_netstack_wire::applied_is_fixture_sample`, the base instant
//! plus the delay compensation a validated sample can carry — never the base
//! alone, which would assert the host's speed rather than the guest's clock),
//! and the peer must additionally report that it leased the address and served
//! a time request — so neither side passes alone. The peer cannot know which reply the guest believed, and the guest's
//! witness cannot appear unless the peer both leased and answered. A run that
//! never earns the witness fails loud on the runner's inactivity/absolute
//! deadline.
//!
//! ## How it differs from a production kernel
//!
//! It reuses the entire production aarch64 boot pipeline unchanged. The only
//! difference is that it is a dedicated test bin whose audit sink exits on the
//! witness — there is no in-kernel QEMU-exit shortcut to leak into a
//! production build (fail closed).

#![cfg_attr(itest_aarch64, no_std)]
#![cfg_attr(itest_aarch64, no_main)]
#![deny(missing_docs)]

// --- Freestanding test bin (`aarch64-unknown-none`) ----------------

#[cfg(itest_aarch64)]
mod kernel {
    use core::panic::PanicInfo;

    use tairix_arch_aarch64::{handle_panic_via_serial, qemu_exit, SerialSink, SERIAL_SINK};
    use tairix_kalloc::{FreeListAllocator, Heap, HEAP_BYTES};
    use tairix_kernel::aarch64::boot as boot_aarch64;
    use tairix_log::{Event, FieldValue, Sink};

    // The canonical QEMU `virt` device tree, dumped and embedded at build
    // time (`build.rs`). The boot pipeline discovers the board from it
    // because QEMU passes no `x0` DTB pointer at an ELF `-kernel` entry.
    include!(concat!(env!("OUT_DIR"), "/dtb_fixture.rs"));

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

    /// Sink that replays every event through [`SERIAL_SINK`] and reports PASS
    /// once `timed` applied the fixture instant.
    ///
    /// It gates on the *value*, not the event: the clock can only carry this
    /// second if the guest queried the peer, which it can only have found in
    /// option 42 of its own lease, and only by gating the peer's spoof on its
    /// nonce without letting it cancel the transaction.
    struct ClockSetExitSink;

    impl Sink for ClockSetExitSink {
        fn write_event(&self, event: &Event<'_>) {
            SerialSink::new().write_event(event);
            if event.id != tairix_timesync::events::CLOCK_SET {
                return;
            }
            let applied = event.fields.iter().find(|f| f.key == "wall_secs");
            if let Some(FieldValue::SignedInt(secs)) = applied.map(|f| f.value) {
                if tairix_test_netstack_wire::applied_is_fixture_sample(secs) {
                    qemu_exit::exit_success();
                }
            }
        }
    }

    static WITNESS_SINK: ClockSetExitSink = ClockSetExitSink;

    /// Forward to the shared aarch64 panic bridge. A panic parks the CPU
    /// before the witness can fire, so the run times out and the harness
    /// reports `Outcome::Timeout` — the documented fail-loud behaviour.
    #[panic_handler]
    fn tairix_timed_dhcp_qemu_aarch64_panic(info: &PanicInfo<'_>) -> ! {
        handle_panic_via_serial(info)
    }

    /// Boot entry point — the symbol the arch crate's `boot.s` trampoline
    /// calls (via `tairix_arch_aarch64_main`).
    ///
    /// QEMU hands no DTB pointer (`_dtb == 0`), so the embedded `virt` blob's
    /// address is forwarded to the production boot pipeline. The witness sits
    /// on the **diagnostic** sink because a service's own records reach only
    /// that one, so every boot/autoload/bind/lease/clock record is captured
    /// and `timed`'s `CLOCK_SET`, carrying the applied `wall_secs=`, is the
    /// run's gating witness. Boot at the default `Info` filter: the witness
    /// records are `Info` records.
    #[no_mangle]
    pub extern "C" fn kernel_main(_dtb: u64) -> ! {
        let dtb = DTB_BLOB.as_ptr() as u64;
        boot_aarch64::boot(
            dtb,
            &ALLOCATOR,
            &WITNESS_SINK,
            &SERIAL_SINK,
            tairix_log::Level::Info,
            &tairix_kernel::hwtree_store::HW_TREE_SOURCE,
        )
    }
}

// --- Host stub -----------------------------------------------------
#[cfg(not(itest_aarch64))]
fn main() {}
