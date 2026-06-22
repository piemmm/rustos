//! The in-kernel root-unlock service (`plans/PI.md` P11 Chunk B-2
//! INCREMENT (2)).
//!
//! Once the boot path has bound the bootstrap root block device
//! ([`crate::root_storage`]) and the console keyboard is live, the
//! encrypted root must be unlocked — the operator types the passphrase
//! ([`crate::root_mount::unlock_root_disk_interactively`]) — before
//! `login` can authenticate (the loaded database is published into
//! [`crate::root_mount::LATE_USERS_DB`]). A blocking console read before
//! the dispatch loop runs would deadlock, so the unlock runs as a
//! **scheduler kthread** admitted at the init seam, exactly like the
//! USB-keyboard service ([`crate::keyboard_service`]).
//!
//! This module is the host-compiled, host-tested, device-independent core
//! (`AGENTS.md` §2.2): the post-MMU boot stash ([`record_boot`] /
//! [`take_boot`]) carrying the resolved [`RootBlockBinding`], the firmware
//! DTB pointer, and the discovered hardware tree to the init seam, plus the
//! console-0 ownership gate ([`Console0Gate`] / [`CONSOLE0_GATE`] /
//! [`GatedConsoleRead`]) that keeps `login` from stealing the passphrase
//! bytes while the unlock is in progress.
//!
//! The freestanding, architecture-specific other half — the live block
//! bring-up that runs the unlock policy inside the kthread — lives beside
//! the rest of the aarch64 boot pipeline in `crate::aarch64::root_unlock`
//! (virtio-blk-MMIO for the QEMU `virt` path, and the Raspberry Pi 4 EMMC2
//! SD host for the Pi-metal root), so this arch-neutral core names no
//! architecture (`AGENTS.md` §17.2 / §2.20).

use alloc::boxed::Box;

use rustos_abi::{CapabilityId, Errno, HwNode};
use rustos_caps::CapabilitySet;
use rustos_kernel_core::{ConsoleRead, CooperativeYield};
use rustos_kernel_sec::captable::TaskId;
use rustos_log::{log, Event, EventId, Level, Sink};
use rustos_sync::SpinLock;

use crate::root_storage::RootBlockBinding;

/// The audit message the unlock kthread logs once it has brought the root
/// block device up, mounted the encrypted root, and installed the users
/// database into [`crate::root_mount::LATE_USERS_DB`] (the `UNLOCK_SERVICE`
/// event, logged from the `crate::aarch64::root_unlock` kthread body).
///
/// Exposed as a stable `pub const` so the `-M virt` admission vertical can
/// key its PASS on the production message — the witness that the in-kernel
/// kthread (not a directly-driven policy) reached a mounted, installed root
/// — without re-declaring the literal (`AGENTS.md` §2.2).
pub const USERS_DB_INSTALLED_MESSAGE: &str =
    "root-unlock: users database installed; login can authenticate";

/// The boot facts the init seam hands the unlock kthread: which discovered
/// node bound the root block driver, and the firmware device-tree pointer
/// the live bring-up walks.
///
/// Carried by value (a [`RootBlockBinding`] is a fixed-size record and the
/// DTB pointer is a `u64`) so the init seam reads it once without holding a
/// lock across the kthread admission.
#[derive(Copy, Clone)]
pub struct UnlockBoot {
    /// The resolved root block binding, or [`None`] when no single block
    /// device was bound (headless / no disk / ambiguous — the unlock is a
    /// no-op and `login` finds no accounts, `AGENTS.md` §18.4).
    pub binding: Option<RootBlockBinding>,
    /// The firmware/loader device-tree pointer (`0` when none was handed
    /// over), used by the live bring-up to construct the virtio-MMIO bus
    /// and resolve the device's GIC SPI.
    pub dtb: u64,
}

impl UnlockBoot {
    /// The empty stash: nothing bound, no DTB.
    const EMPTY: Self = Self {
        binding: None,
        dtb: 0,
    };
}

/// Post-MMU boot stash the boot path fills and the init seam drains.
///
/// Set once after the MMU is enabled (the `SpinLock`'s atomic
/// read-modify-write is UNPREDICTABLE on the MMU-off Device memory the
/// boot CPU runs on, `plans/PI.md` P6c-2 — the same constraint as
/// [`crate::keyboard_service`]'s discovery stash), read once at the init
/// seam. Single producer, single consumer, so the lock never contends.
static UNLOCK_BOOT: SpinLock<UnlockBoot> = SpinLock::new(UnlockBoot::EMPTY);

/// Record the resolved root binding and the firmware DTB pointer for the
/// init seam, and seed the authoritative hardware-inventory store
/// ([`crate::hwtree_store::HW_TREE`]) with the discovered `tree`.
///
/// `tree` is the full discovered hardware tree the kthread matches against
/// the signed driver store during autoload (`AGENTS.md` §18.1 / §18.3). It
/// is copied into the store (the single source of truth, `AGENTS.md` §2.2),
/// so the boot path no longer needs to leak it to `'static`; later
/// bus-enumerated children are appended through [`augment_boot_tree`] and
/// the autoload reader takes a [`boot_tree_snapshot`].
///
/// MUST be called **after** the MMU is enabled (see `UNLOCK_BOOT` and
/// [`crate::hwtree_store`]).
pub fn record_boot(binding: Option<RootBlockBinding>, dtb: u64, tree: &[HwNode]) {
    crate::hwtree_store::HW_TREE.seed(tree);
    *UNLOCK_BOOT.lock() = UnlockBoot { binding, dtb };
}

/// Read the boot stash once at the init seam.
#[must_use]
pub fn take_boot() -> UnlockBoot {
    *UNLOCK_BOOT.lock()
}

/// An owned-then-leaked `'static` view of the current hardware inventory
/// ([`crate::hwtree_store::HW_TREE`]), for the `'static + Send` autoload
/// reader (the unlock kthread captures it by value).
///
/// Snapshotting *after* the floor bring-up has [`augment_boot_tree`]d its
/// enumerated children yields the full discovered tree the autoload walk
/// matches against the signed store. The one-shot leak is the boot publish
/// the boot path used to perform itself, not a per-event allocation
/// (`AGENTS.md` §2.1).
///
/// MUST be called **after** the MMU is enabled (see [`crate::hwtree_store`]).
#[must_use]
pub fn boot_tree_snapshot() -> &'static [HwNode] {
    Box::leak(crate::hwtree_store::HW_TREE.snapshot().into_boxed_slice())
}

/// Append one bus-enumerated child `node` to the discovered hardware
/// inventory ([`crate::hwtree_store::HW_TREE`]), so the pre-unlock autoload
/// (reading [`boot_tree_snapshot`]) matches it against the signed
/// `/System/Drivers/` store like every other discovered device
/// (`AGENTS.md` §18.2 — bus children are enumerated by the floor bus
/// drivers and attached to the tree as further nodes).
///
/// Design B (`plans/PI.md` B3): the bootstrap-floor USB bring-up enumerates
/// the HID keyboard behind the VL805 controller **once** and emits its
/// [`describe_device`](rustos_abi::hwtree) node here, *before* the unlock
/// kthread snapshots the inventory, so the §18 discovery path sees the
/// keyboard rather than it living only inside the imperative bring-up. The
/// keyboard's signed driver bundle is not in the store until the D5 flip,
/// so `devmgr` leaves the node unbound (`AGENTS.md` §18.4) and the in-kernel
/// bring-up keeps driving the keyboard (`AGENTS.md` §2.17) until then.
///
/// The append never drops a node and grows the store on demand (`AGENTS.md`
/// §24.1).
///
/// MUST be called **after** the MMU is enabled and **before** the unlock
/// kthread snapshots the inventory (see [`record_boot`] / [`boot_tree_snapshot`]).
pub fn augment_boot_tree(node: &HwNode) {
    crate::hwtree_store::HW_TREE.append(node);
}

/// The console-0 input ownership gate (`plans/PI.md` P11 Chunk B-2 item 5).
///
/// Both the in-kernel unlock kthread and the per-console `login` would
/// otherwise drain console index 0's input concurrently, racing for the
/// passphrase bytes. The gate resolves that without an ABI change: the
/// console-0 `login` reads through a [`GatedConsoleRead`] that yields no
/// input (so kernel-core's `BlockingConsoleRead` parks the login) until
/// the gate is **opened**, while the unlock kthread reads the raw device
/// directly. The kthread opens the gate the instant the unlock resolves
/// (installed or gave up) — and immediately when there is no disk to
/// unlock — so `login` then takes over console 0 with no byte contention.
///
/// It is a one-way latch (closed → open, never back), so once `login`
/// owns the console no later code can re-gate it (`AGENTS.md` §5.4 — fail
/// closed: a gate that never opened would only ever *withhold* input, it
/// can never grant unauthorized access).
pub struct Console0Gate {
    open: core::sync::atomic::AtomicBool,
}

impl Console0Gate {
    /// A fresh, **closed** gate: console-0 input is withheld from `login`
    /// until the unlock kthread opens it.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            open: core::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Open the gate, releasing console-0 input to `login`. Idempotent.
    pub fn open(&self) {
        self.open.store(true, core::sync::atomic::Ordering::Release);
    }

    /// Whether console-0 input has been released to `login`.
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.open.load(core::sync::atomic::Ordering::Acquire)
    }
}

impl Default for Console0Gate {
    fn default() -> Self {
        Self::new()
    }
}

/// The single `'static` console-0 ownership gate (see [`Console0Gate`]).
///
/// The boot path's console-0 read half is a [`GatedConsoleRead`] over this
/// gate; the unlock kthread opens it once the unlock resolves.
pub static CONSOLE0_GATE: Console0Gate = Console0Gate::new();

/// A [`ConsoleRead`] adapter that withholds input until a [`Console0Gate`]
/// is opened, then delegates to the wrapped device.
///
/// While the gate is closed every read reports a zero-length read, which
/// kernel-core's `BlockingConsoleRead` turns into a scheduler park
/// (`AGENTS.md` §20) — so the console-0 `login` waits rather than draining
/// the passphrase bytes the unlock kthread is reading off the same device.
/// Once the gate opens, reads delegate verbatim to `inner`.
///
/// `Sync` (it holds only `&'static` references and an atomic gate), so it
/// is storable in the shared `'static` console list.
pub struct GatedConsoleRead {
    inner: &'static (dyn ConsoleRead + Sync + 'static),
    gate: &'static Console0Gate,
}

impl GatedConsoleRead {
    /// Wrap `inner` so its reads are withheld until `gate` opens.
    #[must_use]
    pub const fn new(
        inner: &'static (dyn ConsoleRead + Sync + 'static),
        gate: &'static Console0Gate,
    ) -> Self {
        Self { inner, gate }
    }
}

impl ConsoleRead for GatedConsoleRead {
    fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if !self.gate.is_open() {
            // Withhold input: a zero-length read parks the caller in
            // `BlockingConsoleRead` until the gate opens (`AGENTS.md` §20).
            return Ok(0);
        }
        self.inner.read(buf)
    }
}

/// Audit event: the in-kernel root-unlock service lifecycle (started /
/// skipped / device bring-up result), logged at the PID 1 spawn seam and
/// from the kthread (`AGENTS.md` §19.4). Sits beside the root-mount audit
/// ids (`4135`–`4138`, [`crate::root_mount`] / [`crate::root_storage`]).
///
/// Architecture-neutral (`AGENTS.md` §2.2): the one lifecycle event id
/// every port's live bring-up (`crate::aarch64::root_unlock` and its
/// future x86_64 / riscv64 siblings) logs through [`note`], never a
/// per-arch copy.
pub const UNLOCK_SERVICE: EventId = EventId(4139);

/// Synthetic owner task id for the unlock kthread's capability context and
/// IRQ binding. Distinct from the keyboard service's so an audit observer
/// can tell the two in-kernel services apart. The single definition every
/// port shares (`AGENTS.md` §2.2).
pub const UNLOCK_TASK: TaskId = TaskId(0x5b4);

/// The capabilities the unlock kthread holds: [`CapabilityId::MMIO_MAP`]
/// (the virtio register window), [`CapabilityId::MEM_DMA`] (the request
/// DMA), and [`CapabilityId::DRV_LOAD`] (the signed driver-load gate). No
/// more — every map/alloc/load is re-checked against this set (§5.4).
///
/// Architecture-neutral: every port's unlock kthread runs under the same
/// minimal capability set (`AGENTS.md` §2.2 / §5.4).
#[must_use]
pub fn service_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::MMIO_MAP);
    caps.insert(CapabilityId::MEM_DMA);
    caps.insert(CapabilityId::DRV_LOAD);
    caps
}

/// The capability set the post-mount driver autoload presents to the signed
/// load gate — the driver-loading authority's **delegatable** superset, which
/// each driver's manifest request is intersected with (`AGENTS.md` §5.2).
///
/// This is deliberately broader than [`service_caps`] (the unlock kthread's
/// *own* minimal bring-up authority, §5.4): the kthread, standing in for
/// `devmgr`, must be able to hand an autoloaded driver the resource
/// capabilities its class needs, but never holds them ambiently itself. It is
/// [`service_caps`] plus [`CapabilityId::INPUT_INJECT`] and
/// [`CapabilityId::IRQ_BIND`], so an autoloaded input driver (e.g. the
/// virtio-input keyboard) can be granted the keyboard-injection authority
/// `key_inject` requires and the `irq_bind`/`irq_wait` authority its
/// interrupt-driven event loop parks on, while a storage or other driver —
/// whose manifest does not request them — receives nothing extra (the
/// per-driver intersection still binds, `AGENTS.md` §18.3 / §4 — no ambient
/// authority). The driver never receives `CAP_DRV_LOAD`: it is the *caller's*
/// key to the gate, not a capability any driver's manifest requests.
///
/// Architecture-neutral (`AGENTS.md` §2.2): every port's unlock kthread
/// autoloads under the same delegatable set.
#[must_use]
pub fn autoload_caps() -> CapabilitySet {
    let mut caps = service_caps();
    caps.insert(CapabilityId::INPUT_INJECT);
    // An interrupt-driven user-space input driver parks on its device line
    // through `irq_bind`/`irq_wait`, so the delegatable set carries
    // `CAP_IRQ_BIND` too; the per-driver manifest intersection still binds, so
    // a driver that does not request it receives nothing extra (`AGENTS.md`
    // §18.3 / §4 — no ambient authority).
    caps.insert(CapabilityId::IRQ_BIND);
    caps
}

/// The capability set the signed driver-load gate is presented with:
/// `CAP_DRV_LOAD` + `CAP_DRV_KERNEL` (the bootstrap block-device manifest
/// is `kind = InKernel`). Each driver receives only the intersection with
/// its manifest request (`AGENTS.md` §5.2).
///
/// Architecture-neutral: every port admits its bootstrap in-kernel block
/// driver through the same gate caps (`AGENTS.md` §2.2 / §5.2).
#[must_use]
pub fn loader_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::DRV_LOAD);
    caps.insert(CapabilityId::DRV_KERNEL);
    caps
}

/// The capability set the disk-owning kthread binds the well-known
/// driver-store call endpoint under (Design D D2b —
/// [`crate::driver_store_server::create_driver_store_endpoint`]).
///
/// The endpoint restricts its callers to holders of [`CapabilityId::DRV_LOAD`]
/// (the device manager's authority to read the store, `AGENTS.md` §5.2), and
/// binding such a *restricted-sender* endpoint is by definition privileged:
/// [`rustos_kernel_ipc::CallEndpoint::create`] requires the binder to hold
/// [`CapabilityId::IPC_BIND_PRIVILEGED`]. That bind authority is **not** part
/// of [`service_caps`] — the kthread's minimal device bring-up set, which
/// holds no IPC authority (§5.4 — no ambient authority) — so the one-shot
/// binder context is derived from this distinct, deliberately narrow set:
/// `IPC_BIND_PRIVILEGED` and nothing else. The kthread never posts to or
/// reads the store endpoint as a *caller* (it is the bound *server*), so it
/// needs no `CAP_DRV_LOAD` here.
///
/// Architecture-neutral (`AGENTS.md` §2.2): every port's driver-store
/// kthread binds the endpoint under the same single capability.
#[must_use]
pub fn store_endpoint_binder_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::IPC_BIND_PRIVILEGED);
    caps
}

/// The scheduler task id of the disk-owning driver-store service kthread,
/// published once at admission (Design D D2b-2c).
///
/// The driver-store server parks on [`rustos_kernel_core::SERVE_WAITQ`]
/// between requests (a real park, never a busy-yield — `AGENTS.md` §2.1);
/// the `ipc_call` handler's [`rustos_kernel_core::serve_wake`] unparks
/// **by id**, so the kthread's scheduler id must be reachable from the
/// serve loop. The init seam learns the id only when
/// [`rustos_kernel_core::InitSpawnCtx::spawn_kernel_service`] returns
/// (after the body that runs the serve loop was already built), so it is
/// stashed here and read by the loop on its first park. Single producer
/// (the admission seam) writes it once before the body ever runs; the loop
/// only reads — the lock never contends.
static STORE_SERVICE_TASK: SpinLock<Option<rustos_kernel_sched_api::TaskId>> = SpinLock::new(None);

/// Publish the disk-owning driver-store service kthread's scheduler task
/// id, so its serve loop can register on [`rustos_kernel_core::SERVE_WAITQ`]
/// to be unparked when a request is posted (see `STORE_SERVICE_TASK`).
pub fn set_store_service_task(id: rustos_kernel_sched_api::TaskId) {
    *STORE_SERVICE_TASK.lock() = Some(id);
}

/// The disk-owning driver-store service kthread's scheduler task id, or
/// [`None`] before admission published it (see `STORE_SERVICE_TASK`).
#[must_use]
pub fn store_service_task() -> Option<rustos_kernel_sched_api::TaskId> {
    *STORE_SERVICE_TASK.lock()
}

/// Log an unlock-service lifecycle decision onto the service's audit sink
/// under the shared [`UNLOCK_SERVICE`] event id (`AGENTS.md` §19.4).
pub fn note(audit: &dyn Sink, level: Level, message: &'static str) {
    log(
        audit,
        &Event {
            level,
            id: UNLOCK_SERVICE,
            message,
            fields: &[],
        },
    );
}

/// Like [`note`], but carries the `stage` naming the bring-up step a
/// failure was localised to and the `error` it failed with.
///
/// Used by the per-device root bring-up to surface *which* step of a
/// floor block-device bring-up stalled and *how* (e.g. the EMMC2
/// SD-identification command on a real Raspberry Pi 4, which `raspi4b`
/// cannot emulate, so the metal UART log is the only signal —
/// `plans/PI.md` §0.4 / P8 / B4). Both values are stable `&'static str`s
/// from the driver (`rustos_drv_storage_emmc2::BringUpStage::as_str` and
/// the caller's `DriverError` name), so no name is re-spelled here
/// (`AGENTS.md` §2.2). The `error` distinguishes a controller/command
/// fault from a decode rejection at the same stage (e.g. CMD9 `SEND_CSD`
/// timing out vs. returning an unsupported CSD).
pub fn note_stage(
    audit: &dyn Sink,
    level: Level,
    message: &'static str,
    stage: &'static str,
    error: &'static str,
) {
    log(
        audit,
        &Event {
            level,
            id: UNLOCK_SERVICE,
            message,
            fields: &[
                rustos_log::Field {
                    key: "stage",
                    value: stage,
                },
                rustos_log::Field {
                    key: "error",
                    value: error,
                },
            ],
        },
    );
}

/// A cooperative blocking console reader for the unlock kthread.
///
/// The kthread analogue of kernel-core's `BlockingConsoleRead` (which parks
/// only a *user* kthread, via `reschedule_current`): an empty device poll
/// suspends the kthread through its shared [`CooperativeYield`] cell and
/// re-polls on the next dispatch, so the passphrase prompt blocks for input
/// without busy-spinning (`AGENTS.md` §2.1) and never fabricates an end of
/// input. It drives the kthread's single
/// [`YieldHandle`](rustos_kernel_core::YieldHandle) through the
/// [`CooperativeYield`] cell the port's bring-up lends it (`!Sync`, never
/// shared across CPUs); the device-IRQ wait parks separately, so the
/// console poll and the block-I/O wait do not share a suspend mechanism.
///
/// Architecture-neutral (`AGENTS.md` §2.2): the one cooperative
/// console-read shape every port's unlock kthread reads the passphrase
/// through — the device backing differs, the blocking discipline does not.
pub struct KthreadConsoleRead<'a> {
    inner: &'static (dyn ConsoleRead + Sync + 'static),
    yielder: &'a CooperativeYield<'a>,
}

impl<'a> KthreadConsoleRead<'a> {
    /// Wrap the raw console-input device `inner`, suspending the kthread
    /// through `yielder` between empty polls.
    #[must_use]
    pub fn new(
        inner: &'static (dyn ConsoleRead + Sync + 'static),
        yielder: &'a CooperativeYield<'a>,
    ) -> Self {
        Self { inner, yielder }
    }
}

impl ConsoleRead for KthreadConsoleRead<'_> {
    fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            let read = self.inner.read(buf)?;
            if read > 0 {
                return Ok(read);
            }
            #[cfg(all(freestanding, kernel_isa = "aarch64"))]
            {
                use core::fmt::Write as _;
                let mut w = rustos_arch_aarch64::serial::ConsoleWriter;
                let _ = write!(w, "JDBG CONSREAD yield\r\n");
            }
            self.yielder.yield_now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A console read source that hands out a fixed byte once per poll and
    /// records how many times it was polled, so a test can prove the gate
    /// withholds polls while closed.
    struct CountingRead {
        polls: core::sync::atomic::AtomicUsize,
    }

    impl CountingRead {
        const fn new() -> Self {
            Self {
                polls: core::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl ConsoleRead for CountingRead {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            self.polls
                .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            if buf.is_empty() {
                return Ok(0);
            }
            buf[0] = b'x';
            Ok(1)
        }
    }

    #[test]
    fn a_closed_gate_withholds_input_without_polling_the_device() {
        static INNER: CountingRead = CountingRead::new();
        static GATE: Console0Gate = Console0Gate::new();
        let gated = GatedConsoleRead::new(&INNER, &GATE);
        let mut buf = [0u8; 4];
        // Closed: reports a zero-length read and never touches the device.
        assert_eq!(gated.read(&mut buf), Ok(0));
        assert_eq!(INNER.polls.load(core::sync::atomic::Ordering::Relaxed), 0);
    }

    #[test]
    fn an_open_gate_delegates_to_the_wrapped_device() {
        static INNER: CountingRead = CountingRead::new();
        static GATE: Console0Gate = Console0Gate::new();
        let gated = GatedConsoleRead::new(&INNER, &GATE);
        GATE.open();
        let mut buf = [0u8; 4];
        assert_eq!(gated.read(&mut buf), Ok(1));
        assert_eq!(buf[0], b'x');
        assert_eq!(INNER.polls.load(core::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn the_gate_is_a_one_way_latch() {
        let gate = Console0Gate::new();
        assert!(!gate.is_open());
        gate.open();
        assert!(gate.is_open());
        // A second open is idempotent and never re-closes.
        gate.open();
        assert!(gate.is_open());
    }

    #[test]
    fn the_boot_stash_and_inventory_round_trip_through_seed_and_augment() {
        use rustos_abi::hwtree::{HwDeviceClass, HwMatchKey, HwNode, HW_NODE_ROOT};

        // `record_boot` stashes the binding + DTB and seeds the
        // authoritative inventory with a minimal discovered tree (a root +
        // a discovered bus), as the floor leaves it before the USB bring-up
        // enumerates a child. (The single test touching the `HW_TREE` /
        // `UNLOCK_BOOT` globals, so it never races a sibling.)
        let seed = [
            HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
            HwNode::new(2, 1, HwDeviceClass::Bus),
        ];
        record_boot(None, 0xDEAD_0000, &seed);
        let boot = take_boot();
        assert!(boot.binding.is_none());
        assert_eq!(boot.dtb, 0xDEAD_0000);

        // The bus-enumerated HID child (`AGENTS.md` §18.2), keyed by the USB
        // interface-class match key the bring-up reads (never fabricated,
        // §18.5), is appended last; the snapshot the autoload reader takes
        // reflects seed + child in discovery order.
        let mut hid = HwNode::new(3, 2, HwDeviceClass::Input);
        hid.push_match_key(HwMatchKey::usb(0x1234, 0x5678, 0x03_01_01))
            .expect("match key fits");
        augment_boot_tree(&hid);

        let snap = boot_tree_snapshot();
        assert_eq!(snap.len(), 3, "the child is appended, nothing dropped");
        assert_eq!(snap[0], seed[0], "existing nodes keep their order");
        assert_eq!(snap[1], seed[1]);
        assert_eq!(snap[2], hid, "the enumerated child lands last");
    }

    /// A [`Sink`] that records each logged event's id and message so a test
    /// can prove `note` stamps the shared [`UNLOCK_SERVICE`] event id.
    struct CapturingSink {
        events: core::cell::RefCell<alloc::vec::Vec<(u32, alloc::string::String)>>,
    }

    impl CapturingSink {
        fn new() -> Self {
            Self {
                events: core::cell::RefCell::new(alloc::vec::Vec::new()),
            }
        }
    }

    impl Sink for CapturingSink {
        fn write_event(&self, event: &Event<'_>) {
            use alloc::string::ToString;
            self.events
                .borrow_mut()
                .push((event.id.0, event.message.to_string()));
        }
    }

    #[test]
    fn note_stamps_the_shared_unlock_service_event_id() {
        let sink = CapturingSink::new();
        note(&sink, Level::Info, "root-unlock: a decision");
        let events = sink.events.borrow();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].0, UNLOCK_SERVICE.0);
        assert_eq!(events[0].1, "root-unlock: a decision");
    }

    #[test]
    fn the_unlock_kthread_holds_exactly_its_minimal_capability_set() {
        // §5.4 — no ambient authority: the kthread maps MMIO, allocs DMA,
        // and drives the signed load gate, and nothing more.
        let caps = service_caps();
        assert!(caps.contains(CapabilityId::MMIO_MAP));
        assert!(caps.contains(CapabilityId::MEM_DMA));
        assert!(caps.contains(CapabilityId::DRV_LOAD));
        assert!(!caps.contains(CapabilityId::DRV_KERNEL));
    }

    #[test]
    fn autoload_caps_extends_service_caps_with_input_inject_only() {
        // The autoload gate's delegatable superset is the kthread's own
        // minimal caps plus `CAP_INPUT_INJECT` — so an autoloaded input
        // driver's manifest∩caller intersection can grant the keyboard
        // injection authority `key_inject` requires (`AGENTS.md` §5.2 /
        // §18.3), while the kthread's own `service_caps` never holds it
        // (§5.4 — no ambient authority for the bring-up context).
        let service = service_caps();
        let autoload = autoload_caps();
        assert!(!service.contains(CapabilityId::INPUT_INJECT));
        assert!(autoload.contains(CapabilityId::INPUT_INJECT));
        // Every bring-up capability is still present; nothing else is added.
        assert!(autoload.contains(CapabilityId::MMIO_MAP));
        assert!(autoload.contains(CapabilityId::MEM_DMA));
        assert!(autoload.contains(CapabilityId::DRV_LOAD));
        // The driver never receives `CAP_DRV_KERNEL` (an in-kernel-only gate
        // cap) through the autoload superset.
        assert!(!autoload.contains(CapabilityId::DRV_KERNEL));
    }

    #[test]
    fn the_loader_gate_is_presented_drv_load_plus_drv_kernel_only() {
        // The bootstrap block driver is `kind = InKernel`, so the gate sees
        // `CAP_DRV_LOAD` + `CAP_DRV_KERNEL` and no resource authority.
        let caps = loader_caps();
        assert!(caps.contains(CapabilityId::DRV_LOAD));
        assert!(caps.contains(CapabilityId::DRV_KERNEL));
        assert!(!caps.contains(CapabilityId::MMIO_MAP));
        assert!(!caps.contains(CapabilityId::MEM_DMA));
    }

    #[test]
    fn the_unlock_owner_task_id_is_the_fixed_shared_value() {
        // A distinct synthetic owner so an audit observer tells the two
        // in-kernel services apart; fixed and shared by every port (§2.2).
        assert_eq!(UNLOCK_TASK, TaskId(0x5b4));
    }

    /// A console source that reports `remaining_empty` zero-length reads
    /// (modelling "no key typed yet") before finally returning one byte, so
    /// a test can prove the kthread reader blocks across empty polls rather
    /// than fabricating an end of input.
    struct DelayedByte {
        remaining_empty: core::sync::atomic::AtomicUsize,
    }

    impl ConsoleRead for DelayedByte {
        fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
            if buf.is_empty() {
                return Ok(0);
            }
            if self
                .remaining_empty
                .load(core::sync::atomic::Ordering::Relaxed)
                > 0
            {
                self.remaining_empty
                    .fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
                return Ok(0);
            }
            buf[0] = b'k';
            Ok(1)
        }
    }

    /// A [`YieldHandle`](rustos_kernel_core::YieldHandle) that counts
    /// cooperative yields; it must never park — the kthread console reader
    /// only ever yields between empty polls (`AGENTS.md` §2.1).
    struct CountingYielder {
        yields: u32,
    }

    impl rustos_kernel_core::YieldHandle for CountingYielder {
        fn yield_now(&mut self) {
            self.yields += 1;
        }

        fn park(&mut self) {
            panic!("the kthread console reader yields, never parks");
        }
    }

    #[test]
    fn the_kthread_reader_blocks_across_empty_polls_then_returns_the_byte() {
        static INNER: DelayedByte = DelayedByte {
            remaining_empty: core::sync::atomic::AtomicUsize::new(3),
        };
        let mut yielder = CountingYielder { yields: 0 };
        {
            let coop = CooperativeYield::new(&mut yielder);
            let reader = KthreadConsoleRead::new(&INNER, &coop);
            let mut buf = [0u8; 4];
            // Blocks across the three empty polls (yielding each time) and
            // returns the byte on the fourth — never a fabricated EOF.
            assert_eq!(reader.read(&mut buf), Ok(1));
            assert_eq!(buf[0], b'k');
        }
        assert_eq!(yielder.yields, 3);
    }

    #[test]
    fn the_kthread_reader_reports_zero_for_an_empty_buffer_without_yielding() {
        static INNER: DelayedByte = DelayedByte {
            remaining_empty: core::sync::atomic::AtomicUsize::new(0),
        };
        let mut yielder = CountingYielder { yields: 0 };
        {
            let coop = CooperativeYield::new(&mut yielder);
            let reader = KthreadConsoleRead::new(&INNER, &coop);
            let mut empty: [u8; 0] = [];
            assert_eq!(reader.read(&mut empty), Ok(0));
        }
        assert_eq!(yielder.yields, 0);
    }
}
