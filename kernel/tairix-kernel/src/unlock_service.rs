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
//! **scheduler kthread** admitted at the init seam, alongside PID 1.
//!
//! This module is the host-compiled, host-tested, device-independent core: the post-MMU boot stash ([`record_boot`] /
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
//! architecture.

use tairix_abi::{CapabilityId, Errno, HwNode};
use tairix_caps::CapabilitySet;
use tairix_kernel_core::{ConsoleRead, CooperativeYield, SecretFeedback};
use tairix_kernel_sec::captable::TaskId;
use tairix_log::{log, Event, EventId, Level, Sink};
use tairix_sync::SpinLock;

use crate::root_storage::RootBlockBinding;

/// The audit message the unlock kthread logs once it has brought the root
/// block device up, mounted the encrypted root, and installed the users
/// database into [`crate::root_mount::LATE_USERS_DB`] (the `UNLOCK_SERVICE`
/// event, logged from the `crate::aarch64::root_unlock` kthread body).
///
/// Exposed as a stable `pub const` so the `-M virt` admission vertical can
/// key its PASS on the production message — the witness that the in-kernel
/// kthread (not a directly-driven policy) reached a mounted, installed root
/// — without re-declaring the literal.
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
    /// no-op and `login` finds no accounts).
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
/// boot CPU runs on, `plans/PI.md` P6c-2), read once at the init seam.
/// Single producer, single consumer, so the lock never contends.
static UNLOCK_BOOT: SpinLock<UnlockBoot> = SpinLock::new(UnlockBoot::EMPTY);

/// Record the resolved root binding and the firmware DTB pointer for the
/// init seam, and seed the authoritative hardware-inventory store
/// ([`crate::hwtree_store::HW_TREE`]) with the discovered `tree`.
///
/// `tree` is the full discovered hardware tree the kthread matches against
/// the signed driver store during autoload — it
/// already carries the bus root nodes `FdtDiscovery` emits (the
/// `brcm,bcm2711-pcie` root complex, the `VideoCore` mailbox), against which
/// `devmgr` autoloads the user-space bus chain. It is copied into the store
/// (the single source of truth), so the boot path no longer
/// needs to leak it to `'static`; a user-space bus driver appends its
/// enumerated children at runtime through `hw_emit_node`
/// ([`crate::hwtree_store::HwTreeStore::publish_child`]), and the autoload
/// load gate resolves a matched node's grants from the live store directly.
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
/// owns the console no later code can re-gate it (fail
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
/// kernel-core's `BlockingConsoleRead` turns into a scheduler park — so the console-0 `login` waits rather than draining
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
            // `BlockingConsoleRead` until the gate opens.
            return Ok(0);
        }
        self.inner.read(buf)
    }
}

/// Audit event: the in-kernel root-unlock service lifecycle (started /
/// skipped / device bring-up result), logged at the PID 1 spawn seam and
/// from the kthread. Sits beside the root-mount audit
/// ids (`4135`–`4138`, [`crate::root_mount`] / [`crate::root_storage`]).
///
/// Architecture-neutral: the one lifecycle event id
/// every port's live bring-up (`crate::aarch64::root_unlock` and its
/// future x86_64 / riscv64 siblings) logs through [`note`], never a
/// per-arch copy.
pub const UNLOCK_SERVICE: EventId = EventId(4139);

/// Synthetic owner task id for the unlock kthread's capability context and
/// IRQ binding. Distinct from the keyboard service's so an audit observer
/// can tell the two in-kernel services apart. The single definition every
/// port shares.
pub const UNLOCK_TASK: TaskId = TaskId(0x5b4);

/// The capabilities the unlock kthread holds: [`CapabilityId::MMIO_MAP`]
/// (the virtio register window), [`CapabilityId::MEM_DMA`] (the request
/// DMA), and [`CapabilityId::DRV_LOAD`] (the signed driver-load gate). No
/// more — every map/alloc/load is re-checked against this set.
///
/// Architecture-neutral: every port's unlock kthread runs under the same
/// minimal capability set.
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
/// each driver's manifest request is intersected with.
///
/// This is deliberately broader than [`service_caps`] (the unlock kthread's
/// *own* minimal bring-up authority): the kthread, standing in for
/// `devmgr`, must be able to hand an autoloaded driver the resource
/// capabilities its class needs, but never holds them ambiently itself. It is
/// [`service_caps`] plus the resource capabilities an autoloaded driver's
/// class may legitimately request: [`CapabilityId::INPUT_INJECT`] and
/// [`CapabilityId::IRQ_BIND`] for an input driver (e.g. the virtio-input
/// keyboard) — the keyboard-injection authority `key_inject` requires and the
/// `irq_bind`/`irq_wait` authority its interrupt-driven event loop parks on;
/// [`CapabilityId::IPC_BIND_PRIVILEGED`] for a bus *service* driver (e.g. the
/// `VideoCore` `vcmailbox` mailbox) to bind the restricted-sender endpoint its
/// consumers call it through; [`CapabilityId::HW_EMIT`] and
/// [`CapabilityId::MAILBOX`] for the user-space USB bus drivers (`pcie_brcm`,
/// `vl805`) that publish enumerated devices and reload controller firmware;
/// [`CapabilityId::SHM`] / [`CapabilityId::IPC_ENDPOINT`] for the
/// USB host-controller driver and its HID class driver to stand up and submit
/// on the per-interface URB transport; [`CapabilityId::SCHED_REALTIME`] for the
/// USB host-controller driver to enter the real-time scheduling class so its
/// IRQ-woken report pump cannot be starved by CPU-bound work; and
/// [`CapabilityId::FS_MOUNT`] for the
/// `volmgr` storage-policy driver to request the audited kernel attach of
/// each recognised volume. A driver whose
/// manifest does not request them receives nothing extra (the per-driver
/// intersection still binds — no ambient authority).
/// The driver never receives `CAP_DRV_LOAD`: it is the *caller's* key to the
/// gate, not a capability any driver's manifest requests.
///
/// Architecture-neutral: every port's unlock kthread
/// autoloads under the same delegatable set.
#[must_use]
pub fn autoload_caps() -> CapabilitySet {
    let mut caps = service_caps();
    caps.insert(CapabilityId::INPUT_INJECT);
    // An interrupt-driven user-space input driver parks on its device line
    // through `irq_bind`/`irq_wait`, so the delegatable set carries
    // `CAP_IRQ_BIND` too; the per-driver manifest intersection still binds, so
    // a driver that does not request it receives nothing extra (no ambient authority).
    caps.insert(CapabilityId::IRQ_BIND);
    // A user-space *service* driver (the VideoCore `vcmailbox` mailbox, whose
    // consumers reach it through a restricted-sender call endpoint) must hold
    // `CAP_IPC_BIND_PRIVILEGED` to bind that endpoint (`kernel/ipc` requires it
    // for any non-empty required-sender set). The delegatable set carries it so
    // such a signed driver can be granted it; the per-driver manifest
    // intersection still binds, so a driver that does not request it receives
    // nothing extra (no ambient authority).
    caps.insert(CapabilityId::IPC_BIND_PRIVILEGED);
    // A user-space *bus* driver (the `pcie_brcm` root complex, the `vl805` USB
    // host) publishes the devices it enumerates into the live hardware tree
    // with `hw_emit_node`, so the device manager autoloads each in turn — the
    // recursive, data-driven discovery chain. That
    // requires `CAP_HW_EMIT`, so the delegatable set carries it; a driver whose
    // manifest does not request it (a storage or input leaf) receives nothing
    // extra (the per-driver intersection still binds).
    caps.insert(CapabilityId::HW_EMIT);
    // The `vl805` USB-host bus driver reloads its controller's firmware over
    // the VideoCore property mailbox before bring-up, reaching the `vcmailbox`
    // service's restricted-sender call endpoint, which requires the caller to
    // hold `CAP_MAILBOX` (`crate::driver_store_server` / `lib/abi`
    // `MAILBOX_ENDPOINT`). The delegatable set carries it so such a signed
    // driver can be granted it; a driver that does not request it receives
    // nothing extra (no ambient authority).
    caps.insert(CapabilityId::MAILBOX);
    // An autoloaded user-space driver emits its structured diagnostics
    // (e.g. the USB boot-keyboard driver's one-shot bring-up failure record)
    // through `log_emit`, which the kernel gates on `CAP_LOG_EMIT`. The delegatable set carries it so such a
    // signed driver can be granted it; the per-driver manifest intersection
    // still binds, so a driver that does not request it receives nothing
    // extra (no ambient authority).
    caps.insert(CapabilityId::LOG_EMIT);
    // The USB host-controller driver (`drivers/bus/usb/xhci`) stands up the
    // per-interface URB transport seam: it creates the shared URB data buffer
    // (`CAP_SHM`) and binds the restricted-sender URB endpoint, minting the
    // grant it forwards onto the interface node so the class driver can submit
    // URBs. The bound class driver (the HID keyboard) maps that forwarded
    // shared buffer (`CAP_SHM`) and submits URBs on the endpoint
    // (`CAP_IPC_ENDPOINT`). The delegatable set carries both so such signed
    // drivers can be granted them; a driver that does not request them
    // receives nothing extra (the per-driver manifest intersection still
    // binds, so no ambient authority).
    caps.insert(CapabilityId::SHM);
    caps.insert(CapabilityId::IPC_ENDPOINT);
    // The USB host-controller driver (`drivers/bus/usb/xhci`) elevates itself
    // to the real-time scheduling class so its IRQ-woken report pump preempts
    // CPU-bound work and interrupt-IN capture cannot be starved (`plans/USB.md`).
    // That elevation is gated on `CAP_SCHED_REALTIME`, so the delegatable set
    // carries it; the per-driver manifest intersection still binds, so a driver
    // that does not request it receives nothing extra (no ambient authority).
    caps.insert(CapabilityId::SCHED_REALTIME);
    // The `volmgr` storage-policy driver probes its bound block-service node
    // read-only and asks the kernel to attach each recognised volume through
    // `volume_attach`, which the kernel gates on `CAP_FS_MOUNT` (and
    // re-validates grants/extent/name before any mount). The delegatable set
    // carries it so such a signed driver can be granted it; a driver that
    // does not request it receives nothing extra (the per-driver manifest
    // intersection still binds, so no ambient authority).
    caps.insert(CapabilityId::FS_MOUNT);
    caps
}

/// The capability set the signed driver-load gate is presented with:
/// `CAP_DRV_LOAD` + `CAP_DRV_KERNEL` (the bootstrap block-device manifest
/// is `kind = InKernel`). Each driver receives only the intersection with
/// its manifest request.
///
/// Architecture-neutral: every port admits its bootstrap in-kernel block
/// driver through the same gate caps.
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
/// (the device manager's authority to read the store), and
/// binding such a *restricted-sender* endpoint is by definition privileged:
/// [`tairix_kernel_ipc::CallEndpoint::create`] requires the binder to hold
/// [`CapabilityId::IPC_BIND_PRIVILEGED`]. That bind authority is **not** part
/// of [`service_caps`] — the kthread's minimal device bring-up set, which
/// holds no IPC authority (no ambient authority) — so the one-shot
/// binder context is derived from this distinct, deliberately narrow set:
/// `IPC_BIND_PRIVILEGED` and nothing else. The kthread never posts to or
/// reads the store endpoint as a *caller* (it is the bound *server*), so it
/// needs no `CAP_DRV_LOAD` here.
///
/// Architecture-neutral: every port's driver-store
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
/// The driver-store server parks on [`tairix_kernel_core::SERVE_WAITQ`]
/// between requests (a real park, never a busy-yield);
/// the `ipc_call` handler's [`tairix_kernel_core::serve_wake`] unparks
/// **by id**, so the kthread's scheduler id must be reachable from the
/// serve loop. The init seam learns the id only when
/// [`tairix_kernel_core::InitSpawnCtx::spawn_kernel_service`] returns
/// (after the body that runs the serve loop was already built), so it is
/// stashed here and read by the loop on its first park. Single producer
/// (the admission seam) writes it once before the body ever runs; the loop
/// only reads — the lock never contends.
static STORE_SERVICE_TASK: SpinLock<Option<tairix_kernel_sched_api::TaskId>> = SpinLock::new(None);

/// Publish the disk-owning driver-store service kthread's scheduler task
/// id, so its serve loop can register on [`tairix_kernel_core::SERVE_WAITQ`]
/// to be unparked when a request is posted (see `STORE_SERVICE_TASK`).
pub fn set_store_service_task(id: tairix_kernel_sched_api::TaskId) {
    *STORE_SERVICE_TASK.lock() = Some(id);
}

/// The disk-owning driver-store service kthread's scheduler task id, or
/// [`None`] before admission published it (see `STORE_SERVICE_TASK`).
#[must_use]
pub fn store_service_task() -> Option<tairix_kernel_sched_api::TaskId> {
    *STORE_SERVICE_TASK.lock()
}

/// The scheduler task id of the interactive root-unlock kthread, published
/// once at admission so its passphrase reader can register on
/// [`tairix_kernel_core::CONSOLE_WAITQ`] and be unparked when a keystroke
/// arrives.
///
/// The unlock kthread reads the passphrase by genuinely **parking** off the
/// run queue (never a busy-yield), so the
/// console RX interrupt's [`tairix_kernel_core::console_wake`] must be able
/// to unpark it **by id**. The init seam learns the id only when
/// [`tairix_kernel_core::InitSpawnCtx::spawn_kernel_service`] returns (after
/// the body that runs the read loop was already built), so it is stashed
/// here and read once when that body constructs its reader. Single producer
/// (the admission seam) writes it once before the body ever runs; the body
/// only reads — the lock never contends. Mirrors `STORE_SERVICE_TASK`
/// (one published-kthread-id discipline).
static UNLOCK_CONSOLE_TASK: SpinLock<Option<tairix_kernel_sched_api::TaskId>> = SpinLock::new(None);

/// Publish the interactive root-unlock kthread's scheduler task id, so its
/// passphrase reader can register on [`tairix_kernel_core::CONSOLE_WAITQ`]
/// to be unparked by the console RX interrupt (see `UNLOCK_CONSOLE_TASK`).
pub fn set_unlock_console_task(id: tairix_kernel_sched_api::TaskId) {
    *UNLOCK_CONSOLE_TASK.lock() = Some(id);
}

/// The interactive root-unlock kthread's scheduler task id, or [`None`]
/// before admission published it (see `UNLOCK_CONSOLE_TASK`).
#[must_use]
pub fn unlock_console_task() -> Option<tairix_kernel_sched_api::TaskId> {
    *UNLOCK_CONSOLE_TASK.lock()
}

/// The minimum pause, in nanoseconds, the passphrase policy waits after a
/// wrong root passphrase before telling the operator and re-prompting.
///
/// Three seconds: long enough to rate-limit a scripted brute-force to a
/// crawl, short enough not to punish an operator who fat-fingered one
/// character. The pause is a genuine timed park (see [`park_for_ns`]), so it
/// costs no CPU. Defined here, arch-neutral, so every port's unlock kthread
/// shares one value rather than each baking its own.
pub const WRONG_PASSPHRASE_RETRY_DELAY_NS: u64 = 3_000_000_000;

/// Park the calling unlock kthread for at least `min_ns` nanoseconds without
/// busy-waiting — the anti-brute-force pause the passphrase policy runs after
/// a wrong attempt (`plans/PI.md` P11).
///
/// It registers a timed wakeup on [`tairix_kernel_core::CONSOLE_WAITQ`] at the
/// deadline, arms the tickless one-shot, and genuinely **parks** off the run
/// queue so the CPU idles for the whole wait. A wake before the deadline (a
/// keystroke arriving on the shared console queue) simply re-checks the clock
/// and re-parks, so the minimum duration always elapses — never a spin, never
/// a `yield_now` loop.
///
/// `task` is the kthread's published scheduler id; without it (a degenerate
/// build that never went through admission) or without an installed wait
/// clock, no timed park is possible, so this returns at once. The delay is a
/// rate limit, not a correctness invariant, and such a build has no monotonic
/// clock to measure three seconds against.
pub fn park_for_ns(
    yielder: &CooperativeYield<'_>,
    task: Option<tairix_kernel_sched_api::TaskId>,
    min_ns: u64,
) {
    let (Some(task), Some(start)) = (task, tairix_kernel_core::wait_now_ns()) else {
        return;
    };
    let deadline = start.saturating_add(min_ns);
    loop {
        let now = tairix_kernel_core::wait_now_ns().unwrap_or(deadline);
        if now >= deadline {
            return;
        }
        tairix_kernel_core::CONSOLE_WAITQ.register(task, deadline);
        tairix_kernel_core::rearm_timed_wakeup();
        yielder.park();
        tairix_kernel_core::console_deregister(task, deadline);
    }
}

/// Log an unlock-service lifecycle decision onto the service's audit sink
/// under the shared [`UNLOCK_SERVICE`] event id.
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
/// from the driver (`tairix_drv_storage_emmc2::BringUpStage::as_str` and
/// the caller's `DriverError` name), so no name is re-spelled here. The `error` distinguishes a controller/command
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
                tairix_log::Field {
                    key: "stage",
                    value: tairix_log::FieldValue::Str(stage),
                },
                tairix_log::Field {
                    key: "error",
                    value: tairix_log::FieldValue::Str(error),
                },
            ],
        },
    );
}

/// An interrupt-driven blocking console reader for the unlock kthread.
///
/// The kthread analogue of kernel-core's `BlockingConsoleRead` (which parks
/// only a *user* kthread, via `reschedule_current`): an empty device poll
/// **parks** the kthread off the run queue through its shared
/// [`CooperativeYield`] cell, and the console RX interrupt's
/// [`tairix_kernel_core::console_wake`] unparks it the instant a byte lands.
/// It is a genuine park, never a busy-yield: while
/// the kthread waits for a keystroke the dispatch loop reaches idle, so the
/// buffered-serial transmit drain (`pump_tx`) and the tickless idle run and the CPU sleeps — the cooperative busy-poll this replaces
/// kept a task perpetually runnable, so the loop never idled and console
/// output stalled until the next keystroke incidentally flushed it.
///
/// `task` is the kthread's own scheduler id (published by
/// [`set_unlock_console_task`] at admission); the reader registers it on
/// [`tairix_kernel_core::CONSOLE_WAITQ`] **before** each poll so a
/// `console_wake` arriving in the window between an empty poll and the park
/// is not lost — the scheduler's wake-pending token converts a concurrent
/// park into a re-ready, exactly the lost-wakeup interlock
/// `BlockingConsoleRead` and `serve_system_store` rely on. The device backing must be the interrupt-fed console queue
/// (the UART's `UART_INPUT`-backed read half, or the video keyboard queue),
/// not a raw hardware-FIFO poll, so the wake source exists.
///
/// Architecture-neutral: the one blocking console-read
/// shape every port's unlock kthread reads the passphrase through — the
/// device backing differs, the park-and-wake discipline does not.
pub struct KthreadConsoleRead<'a> {
    inner: &'static (dyn ConsoleRead + Sync + 'static),
    yielder: &'a CooperativeYield<'a>,
    task: Option<tairix_kernel_sched_api::TaskId>,
    /// The passphrase prompt's secret-entry feedback: fed the consumed
    /// bytes while armed, and ticked from the park loop when its animation
    /// deadline passes — the kthread mirror of the same behaviour
    /// `BlockingConsoleRead` gives a user-space password read.
    secret: Option<&'a SecretFeedback>,
}

impl<'a> KthreadConsoleRead<'a> {
    /// Wrap the interrupt-fed console-input device `inner`, parking the
    /// kthread through `yielder` between empty polls and registering `task`
    /// on [`tairix_kernel_core::CONSOLE_WAITQ`] so the RX interrupt unparks
    /// it. `task` is the kthread's scheduler id from
    /// [`unlock_console_task`]; [`None`] is re-resolved against that cell
    /// on every poll (on a multi-core boot the kthread's first instructions
    /// can race the spawner's publish, so the reader must not latch a
    /// pre-publish `None` for its lifetime) and, while still unpublished,
    /// degrades to a cooperative yield rather than parking a task no wake
    /// could reach. `secret` is the passphrase prompt's activity feedback,
    /// fed and animated while armed.
    #[must_use]
    pub fn new(
        inner: &'static (dyn ConsoleRead + Sync + 'static),
        yielder: &'a CooperativeYield<'a>,
        task: Option<tairix_kernel_sched_api::TaskId>,
        secret: Option<&'a SecretFeedback>,
    ) -> Self {
        Self {
            inner,
            yielder,
            task,
            secret,
        }
    }
}

impl ConsoleRead for KthreadConsoleRead<'_> {
    fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            // Resolve the kthread's id afresh each poll: on a multi-core
            // boot this body can start on another CPU before the spawner's
            // `set_unlock_console_task` publish lands, and a `None` latched
            // at construction would leave the whole passphrase entry on the
            // cooperative-yield fallback — a busy loop — for its lifetime.
            let task = self.task.or_else(unlock_console_task);
            // The secret feedback's one-shot animation deadline, when the
            // passphrase marker is animating; `NO_DEADLINE` the rest of the
            // time, so an ordinary wait takes no timer wake-ups (tickless).
            let deadline = self
                .secret
                .and_then(SecretFeedback::deadline_ns)
                .unwrap_or(tairix_kernel_core::NO_DEADLINE);
            // Register before polling so a `console_wake` arriving between an
            // empty poll and the park is not lost (the register-before-poll
            // interlock).
            if let Some(task) = task {
                tairix_kernel_core::CONSOLE_WAITQ.register(task, deadline);
            }
            let read = match self.inner.read(buf) {
                Ok(read) => read,
                Err(e) => {
                    // Leave the wait set first so no stale registration
                    // lingers, then propagate fail-closed.
                    if let Some(task) = task {
                        tairix_kernel_core::console_deregister(task, deadline);
                    }
                    return Err(e);
                }
            };
            if read > 0 {
                if let Some(task) = task {
                    tairix_kernel_core::console_deregister(task, deadline);
                }
                // Feed the consumed bytes to the armed feedback so the
                // operator sees the passphrase marker; a no-op while
                // disarmed.
                if let Some(secret) = self.secret {
                    let now = tairix_kernel_core::wait_now_ns().unwrap_or(0);
                    secret.consumed(&buf[..read.min(buf.len())], now);
                }
                return Ok(read);
            }
            match task {
                // Genuine park: suspend off the run queue until the RX
                // interrupt's `console_wake` unparks this id — or, while the
                // passphrase marker is animating, until the timed sweep
                // releases the registered deadline — then re-poll.
                // The CPU idles meanwhile.
                Some(task) => {
                    if deadline != tairix_kernel_core::NO_DEADLINE {
                        tairix_kernel_core::rearm_timed_wakeup();
                    }
                    self.yielder.park();
                    tairix_kernel_core::console_deregister(task, deadline);
                    // A timed wake: advance (or pause) the marker, then
                    // loop back to re-poll and re-park.
                    if let Some(secret) = self.secret {
                        if let Some(tick) = secret.deadline_ns() {
                            let now = tairix_kernel_core::wait_now_ns().unwrap_or(0);
                            if now >= tick {
                                secret.tick(now);
                            }
                        }
                    }
                }
                // The kthread's scheduler id is not published yet (the
                // spawner's publish racing this body's first instructions on
                // another CPU, or a degenerate build that skipped admission).
                // A parked task with no registration could never be woken, so
                // cooperatively yield and re-poll on the next dispatch — the
                // per-iteration re-resolution above picks the id up the
                // moment it lands, so this fallback is transient, never the
                // read's steady state.
                None => self.yielder.yield_now(),
            }
        }
    }

    fn read_timeout(&self, buf: &mut [u8], timeout_ns: u64) -> Result<usize, Errno> {
        if buf.is_empty() {
            return Ok(0);
        }
        // The absolute monotonic instant the wait may run until: a genuine
        // bounded park (never a busy poll), so the pre-boot Supervisor's
        // 2-second "press ESC" window can wait for a keystroke and then fall
        // through on time. `park` is woken by the console RX interrupt's
        // `console_wake` the instant a byte lands, or by the timed sweep at
        // the deadline — the CPU idles between, exactly as an untimed
        // [`ConsoleRead::read`] does.
        let start = tairix_kernel_core::wait_now_ns().unwrap_or(0);
        let caller_deadline = start.saturating_add(timeout_ns);
        loop {
            let task = self.task.or_else(unlock_console_task);
            // Wake no later than the caller's deadline; when the secret marker
            // is animating, wake at whichever of its tick and the deadline is
            // sooner so neither is missed.
            let deadline = match self.secret.and_then(SecretFeedback::deadline_ns) {
                Some(tick) => tick.min(caller_deadline),
                None => caller_deadline,
            };
            if let Some(task) = task {
                tairix_kernel_core::CONSOLE_WAITQ.register(task, deadline);
            }
            let read = match self.inner.read(buf) {
                Ok(read) => read,
                Err(e) => {
                    if let Some(task) = task {
                        tairix_kernel_core::console_deregister(task, deadline);
                    }
                    return Err(e);
                }
            };
            if read > 0 {
                if let Some(task) = task {
                    tairix_kernel_core::console_deregister(task, deadline);
                }
                if let Some(secret) = self.secret {
                    let now = tairix_kernel_core::wait_now_ns().unwrap_or(0);
                    secret.consumed(&buf[..read.min(buf.len())], now);
                }
                return Ok(read);
            }
            // Nothing pending: has the caller's window elapsed?
            let now = tairix_kernel_core::wait_now_ns().unwrap_or(caller_deadline);
            if now >= caller_deadline {
                if let Some(task) = task {
                    tairix_kernel_core::console_deregister(task, deadline);
                }
                return Err(Errno::TimedOut);
            }
            match task {
                Some(task) => {
                    tairix_kernel_core::rearm_timed_wakeup();
                    self.yielder.park();
                    tairix_kernel_core::console_deregister(task, deadline);
                    if let Some(secret) = self.secret {
                        if let Some(tick) = secret.deadline_ns() {
                            let now = tairix_kernel_core::wait_now_ns().unwrap_or(0);
                            if now >= tick {
                                secret.tick(now);
                            }
                        }
                    }
                }
                None => self.yielder.yield_now(),
            }
        }
    }

    fn set_secret(&self, secret: bool) {
        // Arm the passphrase marker only across a secret read and disarm it
        // otherwise, so the pre-boot Supervisor's echoed command prompt — read
        // through this same reader — never paints the `[input active…]` marker
        // over its own echo. A reader with no feedback attached is inert.
        if let Some(feedback) = self.secret {
            if secret {
                feedback.arm();
            } else {
                feedback.disarm();
            }
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
    fn the_boot_stash_and_inventory_round_trip_through_seed_and_snapshot() {
        use tairix_abi::hwtree::{HwDeviceClass, HwMatchKey, HwNode, HW_NODE_ROOT};

        // `record_boot` stashes the binding + DTB and seeds the authoritative
        // inventory with the discovered tree `FdtDiscovery` built — here a
        // root plus a discovered bus (the `brcm,bcm2711-pcie` root complex
        // stands in), against which `devmgr` autoloads the user-space bus
        // chain. (The single test touching the `HW_TREE` / `UNLOCK_BOOT`
        // globals, so it never races a sibling.)
        let seed = [
            HwNode::new(1, HW_NODE_ROOT, HwDeviceClass::Root),
            HwNode::new(2, 1, HwDeviceClass::Bus),
        ];
        record_boot(None, 0xDEAD_0000, &seed);
        let boot = take_boot();
        assert!(boot.binding.is_none());
        assert_eq!(boot.dtb, 0xDEAD_0000);

        // The live inventory snapshot reflects exactly the seeded tree; a
        // user-space bus driver's enumerated children are added at runtime
        // through `hw_emit_node` (`publish_child`) and observed through the
        // reactive `hw_tree_wait` generation.
        let snap = crate::hwtree_store::HW_TREE.snapshot();
        assert_eq!(snap.len(), 2, "the seeded discovered tree, nothing dropped");
        assert_eq!(snap[0], seed[0], "existing nodes keep their order");
        assert_eq!(snap[1], seed[1]);

        // A child published at runtime (the user-space bus driver's
        // `hw_emit_node`) lands in the live store under the bus, keyed by its
        // match key, and is seen by a fresh snapshot.
        let mut child = HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Input);
        child
            .push_match_key(HwMatchKey::usb(0x1234, 0x5678, 0x03_01_01))
            .expect("match key fits");
        let id = crate::hwtree_store::HW_TREE.publish_child(2, child);
        let snap = crate::hwtree_store::HW_TREE.snapshot();
        assert_eq!(snap.len(), 3, "the runtime child is added, nothing dropped");
        assert_eq!(snap[2].id(), id, "the store assigned the published id");
        assert_eq!(snap[2].parent(), 2, "parented under the emitter's node");
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
        // — no ambient authority: the kthread maps MMIO, allocs DMA,
        // and drives the signed load gate, and nothing more.
        let caps = service_caps();
        assert!(caps.contains(CapabilityId::MMIO_MAP));
        assert!(caps.contains(CapabilityId::MEM_DMA));
        assert!(caps.contains(CapabilityId::DRV_LOAD));
        assert!(!caps.contains(CapabilityId::DRV_KERNEL));
    }

    #[test]
    fn autoload_caps_extends_service_caps_with_the_delegatable_resource_caps() {
        // The autoload gate's delegatable superset is the kthread's own
        // minimal caps plus the resource caps an autoloaded driver's
        // manifest∩caller intersection may legitimately grant: `INPUT_INJECT`
        // and `IRQ_BIND` for an input driver, and `IPC_BIND_PRIVILEGED` for a
        // bus service driver that binds a restricted-sender endpoint (the
        // VideoCore `vcmailbox`) — none of which the kthread's own
        // `service_caps` hold (no ambient authority for the bring-up
        // context; the per-driver intersection still binds).
        let service = service_caps();
        let autoload = autoload_caps();
        for cap in [
            CapabilityId::INPUT_INJECT,
            CapabilityId::IRQ_BIND,
            CapabilityId::IPC_BIND_PRIVILEGED,
            // A user-space bus driver (`pcie_brcm`, `vl805`) publishes
            // enumerated devices with `hw_emit_node` (`CAP_HW_EMIT`), and
            // `vl805` reloads its controller firmware over the `vcmailbox`
            // restricted-sender endpoint (`CAP_MAILBOX`) — both delegatable to
            // a signed manifest that requests them, neither held by the
            // kthread itself (no ambient authority).
            CapabilityId::HW_EMIT,
            CapabilityId::MAILBOX,
            // The USB host-controller driver creates the shared URB data
            // buffer (`CAP_SHM`); the HID class driver maps that forwarded
            // buffer (`CAP_SHM`) and submits URBs on its interface's transport
            // endpoint (`CAP_IPC_ENDPOINT`). Both are delegatable to a signed
            // manifest that requests them and neither is held by the kthread
            // itself (no ambient authority).
            CapabilityId::SHM,
            CapabilityId::IPC_ENDPOINT,
            // The USB host-controller driver elevates itself to the real-time
            // scheduling class (`CAP_SCHED_REALTIME`) so its IRQ-woken report
            // pump cannot be starved by CPU-bound work — delegatable to a
            // signed manifest that requests it, never held by the kthread
            // itself (no ambient authority).
            CapabilityId::SCHED_REALTIME,
            // An autoloaded user-space driver emits its structured
            // diagnostics through `log_emit` (`CAP_LOG_EMIT`) — delegatable
            // to a signed manifest that requests it, never held by the
            // kthread itself (no ambient authority).
            CapabilityId::LOG_EMIT,
            // The `volmgr` storage-policy driver requests the audited kernel
            // attach of each volume it recognises on its bound block-service
            // node (`volume_attach`, gated on `CAP_FS_MOUNT`) — delegatable
            // to a signed manifest that requests it, never held by the
            // kthread itself (no ambient authority).
            CapabilityId::FS_MOUNT,
        ] {
            assert!(!service.contains(cap));
            assert!(autoload.contains(cap));
        }
        // Every bring-up capability is still present.
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
        // in-kernel services apart; fixed and shared by every port.
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

    /// A [`YieldHandle`](tairix_kernel_core::YieldHandle) that counts genuine
    /// **parks**; it must never busy-yield. With its scheduler id published,
    /// the kthread console reader parks off the run queue between empty
    /// polls (the RX interrupt's `console_wake` unparks it in production),
    /// never spinning the CPU.
    struct ParkCountingYielder {
        parks: u32,
    }

    impl tairix_kernel_core::YieldHandle for ParkCountingYielder {
        fn yield_now(&mut self) {
            panic!("the kthread console reader parks, never busy-yields, once its id is published");
        }

        fn park(&mut self) {
            self.parks += 1;
        }
    }

    /// The fixed scheduler id the reader-under-test registers on
    /// `CONSOLE_WAITQ` (any value works — registration is pure data here).
    fn reader_task() -> tairix_kernel_sched_api::TaskId {
        0x5b4
    }

    #[test]
    fn the_kthread_reader_parks_across_empty_polls_then_returns_the_byte() {
        static INNER: DelayedByte = DelayedByte {
            remaining_empty: core::sync::atomic::AtomicUsize::new(3),
        };
        let mut yielder = ParkCountingYielder { parks: 0 };
        {
            let coop = CooperativeYield::new(&mut yielder);
            let reader = KthreadConsoleRead::new(&INNER, &coop, Some(reader_task()), None);
            let mut buf = [0u8; 4];
            // Parks across the three empty polls (registered on
            // `CONSOLE_WAITQ`, unparked by the RX interrupt in production)
            // and returns the byte on the fourth — never a fabricated EOF,
            // never a busy-yield.
            assert_eq!(reader.read(&mut buf), Ok(1));
            assert_eq!(buf[0], b'k');
        }
        assert_eq!(yielder.parks, 3);
        // The reader deregistered itself once it had the byte; clearing any
        // residual registration keeps this shared global clean for siblings.
        tairix_kernel_core::CONSOLE_WAITQ.deregister(reader_task());
    }

    #[test]
    fn the_kthread_reader_reports_zero_for_an_empty_buffer_without_parking() {
        static INNER: DelayedByte = DelayedByte {
            remaining_empty: core::sync::atomic::AtomicUsize::new(0),
        };
        let mut yielder = ParkCountingYielder { parks: 0 };
        {
            let coop = CooperativeYield::new(&mut yielder);
            let reader = KthreadConsoleRead::new(&INNER, &coop, Some(reader_task()), None);
            let mut empty: [u8; 0] = [];
            assert_eq!(reader.read(&mut empty), Ok(0));
        }
        assert_eq!(yielder.parks, 0);
    }
}
