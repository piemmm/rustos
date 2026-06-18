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
//! The freestanding, architecture-specific other half — the live
//! virtio-blk-MMIO bring-up that runs the unlock policy inside the kthread
//! — lives beside the rest of the aarch64 boot pipeline in
//! `crate::aarch64::root_unlock` (the QEMU `virt` path; EMMC2 on the
//! Raspberry Pi 4 is the staged metal increment), so this arch-neutral core
//! names no architecture (`AGENTS.md` §17.2 / §2.20).

use rustos_abi::{CapabilityId, Errno, HwNode};
use rustos_caps::CapabilitySet;
use rustos_crypto::Ed25519PublicKey;
use rustos_kernel_core::{ConsoleRead, CooperativeYield};
use rustos_kernel_sec::captable::TaskId;
use rustos_log::{log, Event, EventId, Level, Sink};
use rustos_sync::SpinLock;

use crate::driver_autoload::autoload_from_mounted_root;
use crate::driver_spawn_loader::DriverProcessSpawn;
use crate::root_mount::{MountedRootHook, RootVolume};
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
    /// The full discovered hardware tree (`AGENTS.md` §18.1), buffered once
    /// by the boot path and leaked to `'static`, so the unlock kthread can
    /// drive `devmgr` autoload over it once the encrypted root is mounted —
    /// the input-device node included, not just the root block binding
    /// (`plans/PI.md` P11). Empty (`&[]`) when no tree was discovered
    /// (headless / no firmware tree), which autoloads nothing (§18.4).
    pub tree: &'static [HwNode],
}

impl UnlockBoot {
    /// The empty stash: nothing bound, no device tree.
    const EMPTY: Self = Self {
        binding: None,
        dtb: 0,
        tree: &[],
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
/// init seam.
///
/// `tree` is the full discovered hardware tree the kthread matches against
/// the signed driver store during autoload (`AGENTS.md` §18.1 / §18.3); the
/// boot path leaks it to `'static` so this `Copy` stash can carry it.
///
/// MUST be called **after** the MMU is enabled (see `UNLOCK_BOOT`).
pub fn record_boot(binding: Option<RootBlockBinding>, dtb: u64, tree: &'static [HwNode]) {
    *UNLOCK_BOOT.lock() = UnlockBoot { binding, dtb, tree };
}

/// Read the boot stash once at the init seam.
#[must_use]
pub fn take_boot() -> UnlockBoot {
    *UNLOCK_BOOT.lock()
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
            self.yielder.yield_now();
        }
    }
}

/// The [`MountedRootHook`] the unlock kthread runs the instant the
/// encrypted root mounts: it autoloads user-space drivers off the volume's
/// signed `/System/Drivers/` store (`AGENTS.md` §18.3 / §18.6).
///
/// It holds only borrowed/`'static`/`Copy` state, adds no authority of its
/// own, and runs inside the mount scope so the autoload reads the store off
/// the live volume. A failed autoload never fails the unlock — a node that
/// matches nothing is left unbound and a bad bundle fails *that* node closed
/// inside the pipeline (`AGENTS.md` §18.4 / §5.4); only an unopenable root
/// volume is surfaced (logged, then swallowed) (`AGENTS.md` §2.9).
///
/// Architecture-neutral (`AGENTS.md` §2.2): the autoload policy is the same
/// for every port; the only per-arch input is the [`DriverProcessSpawn`]
/// seam each port hands [`AutoloadHook::new`] (its `InitCtxDriverProcessSpawn`
/// over that architecture's process producer), so a winning driver is
/// spawned through that architecture's process mechanism while this hook
/// names none of it (`AGENTS.md` §17.1 / §2.20).
pub struct AutoloadHook<'a> {
    /// The architecture process-creation seam each winning driver is
    /// spawned through, behind the [`DriverProcessSpawn`] abstraction so
    /// this hook stays scheduler- and arch-agnostic (`AGENTS.md` §17.1 /
    /// §2.2).
    spawn: &'a dyn DriverProcessSpawn,
    /// The discovered hardware tree every node is matched against
    /// (`AGENTS.md` §18.1).
    tree: &'static [HwNode],
    /// The driver-signing trust anchor(s) the load gate verifies each
    /// winning bundle against — the kernel's embedded key (`AGENTS.md` §8 /
    /// §9).
    trusted: &'a [Ed25519PublicKey],
    /// The capability set the load gate intersects each manifest request
    /// with; holds `CAP_DRV_LOAD`, so a user-space driver can be admitted
    /// (`AGENTS.md` §5.2 / §5.4).
    caps: CapabilitySet,
    /// The audit sink every scan / match / load / spawn decision is logged
    /// through (`AGENTS.md` §18.3 / §19.4).
    audit: &'a dyn Sink,
}

impl<'a> AutoloadHook<'a> {
    /// Build the post-mount autoload hook over the port's `spawn` seam and
    /// the boot-discovered `tree`, verifying winning bundles against
    /// `trusted`, granting them at most `caps`, and auditing to `audit`.
    #[must_use]
    pub fn new(
        spawn: &'a dyn DriverProcessSpawn,
        tree: &'static [HwNode],
        trusted: &'a [Ed25519PublicKey],
        caps: CapabilitySet,
        audit: &'a dyn Sink,
    ) -> Self {
        Self {
            spawn,
            tree,
            trusted,
            caps,
            audit,
        }
    }
}

impl MountedRootHook for AutoloadHook<'_> {
    fn after_mount(&mut self, volume: &mut dyn RootVolume) {
        // Match every discovered node against the signed store and spawn each
        // winner with exactly its node's resource grants. A missing / empty /
        // all-malformed store binds nothing in `Ok`, and an unmatched node is
        // left unbound — never an error (`AGENTS.md` §18.4). Only the
        // private-root-mount failure is `Err`; it must never fail the root
        // unlock, so it is logged and swallowed (`AGENTS.md` §2.9).
        if autoload_from_mounted_root(
            volume,
            self.tree,
            self.trusted,
            self.spawn,
            &[],
            &self.caps,
            self.audit,
        )
        .is_err()
        {
            note(
                self.audit,
                Level::Error,
                "root-unlock: driver autoload could not open the root volume; no drivers \
                 autoloaded",
            );
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
    fn the_boot_stash_round_trips_the_dtb_and_an_absent_binding() {
        record_boot(None, 0xDEAD_0000, &[]);
        let boot = take_boot();
        assert!(boot.binding.is_none());
        assert_eq!(boot.dtb, 0xDEAD_0000);
        assert!(boot.tree.is_empty());
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

    /// A [`DriverProcessSpawn`] that must never be invoked: an empty driver
    /// store binds nothing, so the autoload hook spawns no driver.
    struct NoSpawn;

    impl DriverProcessSpawn for NoSpawn {
        fn spawn_driver(
            &self,
            _rxe: &[u8],
            _granted: CapabilitySet,
            _grants: &[rustos_abi::hwtree::HwResource],
            _args: &[&[u8]],
        ) -> Result<u64, Errno> {
            panic!("an empty driver store must not spawn any driver");
        }
    }

    #[test]
    fn the_autoload_hook_binds_nothing_off_an_empty_store_and_never_errors() {
        // The post-mount hook over a volume with no `/System/Drivers/` store
        // and an empty hardware tree must match nothing, spawn nothing, and
        // log no failure — the autoload never fails the unlock (§18.4 /
        // §2.9).
        let mut fs = crate::test_support::MockRootFs::new();
        let sink = CapturingSink::new();
        let spawn = NoSpawn;
        let trusted: [Ed25519PublicKey; 0] = [];
        let mut hook = AutoloadHook::new(&spawn, &[], &trusted, service_caps(), &sink);
        hook.after_mount(&mut fs);
        // No driver-volume-open failure was logged (the only `Err` path).
        assert!(
            !sink
                .events
                .borrow()
                .iter()
                .any(|(_, m)| m.contains("could not open the root volume")),
            "an empty store must not surface a volume-open failure"
        );
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
