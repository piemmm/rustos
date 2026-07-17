//! Architecture-neutral root-unlock / driver-store orchestration
//! (`plans/PI.md` design B).
//!
//! Every Tier-1 port that brings a bootstrap-floor root block device up
//! feeds it into the *same* two-task tail: the one brought-up disk is shared
//! for the life of the system by two independent preemptive tasks — a spawned
//! interactive encrypted-root unlock, and the persistent driver-store serve
//! loop the user-space `devmgr` reads signed `/System` drivers through. That
//! tail is device- and architecture-independent, so it lives here rather than
//! being copied into each `kernel/arch/<target>/` sibling: a port supplies
//! only its bring-up, its console-0 seam ([`UnlockConsole`]), and its
//! process-spawn producer, and calls [`finish_unlock`].
//!
//! The two hardware-specific seams a port injects:
//!
//! * [`UnlockConsole`] — the primary-console (index 0) write + raw-read pair
//!   the passphrase prompt uses, plus the release-to-`login` handoff the
//!   moment the unlock resolves. A framebuffer-vs-UART decision, an
//!   interrupt-driven receive arm, and the console-0 ownership gate are all
//!   the port's business; the orchestration only asks for a console and to
//!   release it.
//! * [`rustos_kernel_core::ProcessSpawn`] — the architecture's process
//!   producer, bridged through the arch-neutral
//!   [`crate::driver_spawn_loader::InitCtxDriverProcessSpawn`] so a matched
//!   user-space driver is spawned into its own hardware-isolated process
//!   granted exactly its matched node's resources (no ambient authority).

use core::convert::Infallible;

use rustos_abi::driver::block::Block;
use rustos_crypto::Ed25519PublicKey;
use rustos_drv_fs_arxfs::{VolumeKey, ARXFS};
use rustos_kernel_core::{
    ConsoleRead, ConsoleWrite, CooperativeYield, InitSpawnCtx, ProcessSpawn, SecretFeedback,
    SleepLock, YieldHandle,
};
use rustos_kernel_mem::MemoryPressure;
use rustos_kernel_sec::captable::TaskCapabilities;
use rustos_kernel_sec::identity::UserId;
use rustos_log::{Level, Sink};
use rustos_partition::{parse_partition_table, PartitionBlock, PartitionType};

use crate::block_cache::BlockCache;
use crate::driver_catalog::KERNEL_DRIVER_SIGNER_PUBKEY;
use crate::driver_spawn_loader::InitCtxDriverProcessSpawn;
use crate::root_mount::{
    unlock_root_disk_interactively, with_system_volume, AdminInstall, UnlockInstall, UnlockOutcome,
    WritableRootSink, LATE_IDENTITY, LATE_USERS_ADMIN, LATE_USERS_DB,
};
use crate::shared_block::{DriverStoreService, SharedBlock};
use crate::system_mount::{
    install_system_mount, register_writable_state, KernelFs, ROOT_VOLUME_HANDLE,
};
use crate::transform_cache::TransformClusterCache;
use crate::unlock_service::{
    autoload_caps, note, store_endpoint_binder_caps, KthreadConsoleRead, UNLOCK_TASK,
    USERS_DB_INSTALLED_MESSAGE,
};

/// The architecture console-0 seam the root-unlock orchestration reaches the
/// hardware console through.
///
/// A port decides *which* device is the primary console (a framebuffer text
/// console, the discovered UART, an SBI console) and *how* to arm its
/// interrupt-driven receive and its console-0 ownership gate; the
/// orchestration only asks for the write + raw-read pair for the passphrase
/// prompt and to release the console to `login` once the unlock resolves.
///
/// `Sync` so a `'static` seam reference can be captured by the `Send` unlock
/// kthread body ([`rustos_kernel_core::ProcessSpawn`]-spawned service).
pub trait UnlockConsole: Sync {
    /// Acquire the primary-console (index 0) write half and raw read half for
    /// the passphrase prompt, arming any interrupt-driven receive the port
    /// needs so a keystroke wakes the parked reader rather than being missed.
    ///
    /// Both returned halves are `'static` (the console devices are boot
    /// statics), so the unlock kthread body can hold them for the life of the
    /// prompt.
    fn acquire_console0(
        &self,
    ) -> (
        &'static dyn ConsoleWrite,
        &'static (dyn ConsoleRead + Sync + 'static),
    );

    /// Release console 0 to `login` the instant the unlock resolves — open
    /// the console-0 ownership gate, arm the receive interrupt for a `login`
    /// reader where the port needs it, and resolve the late users-database
    /// pending wait. Called exactly once on every unlock return path, so a
    /// successful unlock can never leave the console latched shut.
    fn release_console0_to_login(&self);
}

/// The `'static` boot environment a root-unlock bring-up threads through: the
/// init-spawn context (the per-arch driver-spawn seam) and the audit sink.
/// The matched-node grants a driver load mints are resolved from the live
/// [`crate::hwtree_store::HW_TREE`] inventory directly, so no boot-tree
/// snapshot rides along here.
///
/// Grouped because both travel together from the kthread body through the
/// per-device bring-up into the shared [`finish_unlock`] tail; passing one
/// cohesive `Copy` value rather than re-listing two `'static` references in
/// every signature keeps the seams readable and below the argument-count bar.
#[derive(Clone, Copy)]
pub struct UnlockEnv {
    /// The boot-time init-spawn context owning the live kernel registries the
    /// driver spawn and the unlock kthread are admitted through.
    pub ctx: &'static (dyn InitSpawnCtx + Sync),
    /// The `'static` boot audit sink every security-relevant decision (mount,
    /// install, give-up, autoload) is logged through.
    pub audit: &'static (dyn Sink + Sync),
    /// The system memory-pressure gauge the mounted volumes' caches sample
    /// (`plans/SMARTRAM.md` SMART2), built over the kernel frame allocator
    /// before the unlock kthread spawns.
    pub pressure: &'static MemoryPressure,
}

/// The live [`WritableRootSink`]: on a successful unlock it opens a second,
/// independent `'static` read-write [`ARXFS`] window onto the `ARXFSRoot`
/// partition under the just-derived key and registers it as the **writable
/// root volume** backing — `/` itself and every writable sub-mount of it
/// (`/Users`, `/Apps`, `/Storage`, `/System/Logs`, `/System/Settings`), which
/// all resolve to this one volume
/// ([`crate::system_mount::register_writable_state`]).
///
/// This is the only path that can mount the writable state: the encrypted
/// root is the one writable partition, so its key — live only at the moment
/// of a successful unlock — is required, and until it lands every write to
/// `/` and its subtrees fails closed. The read window the unlock used for
/// `/System/Security` is already dropped, so this read-write view is the sole
/// writer of the volume. Fail-soft and audited: any partition/window/mount
/// refusal leaves the writable tree failing closed and never disturbs the
/// users/identity install.
struct WritableStateSink<B: Block + 'static> {
    store: &'static DriverStoreService<B>,
    audit: &'static (dyn Sink + Sync),
    /// The system memory-pressure gauge the writable root volume's cache
    /// samples, threaded from [`UnlockEnv`].
    pressure: &'static MemoryPressure,
}

impl<B: Block + 'static> WritableRootSink for WritableStateSink<B> {
    fn publish(
        &self,
        volume_key: &VolumeKey,
    ) -> Option<alloc::sync::Arc<SleepLock<alloc::boxed::Box<dyn KernelFs>>>> {
        // Locate the ARXFSRoot extent on a throwaway probe window, then open
        // the durable owned `'static` window onto it.
        let extent = {
            let mut probe = self.store.window();
            let Ok(table) = parse_partition_table(&mut probe) else {
                note(
                    self.audit,
                    Level::Error,
                    "root-unlock: writable-state partition table invalid",
                );
                return None;
            };
            let Some(extent) = table.first_of_type(PartitionType::ARXFSRoot) else {
                note(
                    self.audit,
                    Level::Error,
                    "root-unlock: writable-state no root partition",
                );
                return None;
            };
            extent
        };
        let Ok(window) = PartitionBlock::from_partition(self.store.window(), &extent) else {
            note(
                self.audit,
                Level::Error,
                "root-unlock: writable-state window out of range",
            );
            return None;
        };
        // Re-open the same encrypted volume read-write under the just-derived
        // key. The driver retains the derived master key for the life of the
        // mount, exactly as the read mount does. Compressed clusters are
        // served through the SMART3 transform cache, charged to the root
        // volume's mount identity and governed by the shared pressure gauge.
        let Ok(fs) = ARXFS::open(window, volume_key) else {
            note(
                self.audit,
                Level::Error,
                "root-unlock: writable-state mount failed",
            );
            return None;
        };
        let fs = fs.with_cluster_cache(TransformClusterCache::for_volume(
            ROOT_VOLUME_HANDLE,
            self.pressure,
            self.audit,
        ));
        // The registered driver is the volume's single writer; the
        // `CAP_USER_ADMIN` account-administration engine shares this same
        // lock (`plans/CAPABILITY_USE.md` CU4) — `/System/Security` is
        // shadowed by the read-only `/System` mount, so the engine persists
        // through this driver directly. Fail-soft: a registration refusal
        // leaves the writable tree and `users_admin` failing closed.
        let volume_uuid = fs.volume_uuid();
        let driver: alloc::boxed::Box<dyn KernelFs> = alloc::boxed::Box::new(fs);
        register_writable_state(driver, volume_uuid, self.audit, self.pressure)
    }
}

/// The shared two-task tail both floor block bring-ups feed, turning the one
/// brought-up disk into a disk shared for life by two independent preemptive
/// tasks (Design D D2b-2c).
///
/// `blk` is the brought-up whole-disk [`Block`] device (virtio-blk or EMMC2),
/// already boot-leaked to `'static` by its bring-up. It is wrapped in a leaked
/// `&'static` [`DriverStoreService`] (over the [`SharedBlock`] layer), so two
/// tasks reach it through independent serialised windows:
///
/// * A **separate, spawned** preemptive task runs the interactive
///   encrypted-root unlock against the *user-data* volume and, when it
///   resolves (installed or fail-closed), releases the console to `login`
///   through [`UnlockConsole::release_console0_to_login`].
/// * **This** task becomes the persistent driver-store serve loop: it binds
///   and serves the capability-gated store IPC endpoint the user-space
///   `devmgr` loads signed `/System` drivers through, real-parking on
///   `SERVE_WAITQ` between requests, and never returns on success.
///
/// Crucially the store endpoint binds **independently of** the user-data
/// passphrase (the signed driver store lives on the always-readable `/System`
/// volume, `plans/PI.md` design B), so the keyboard driver loads in user space
/// *before* the unlock prompt — no chicken-and-egg, and no cooperative
/// interleaving of the two on one kthread.
///
/// `console` is the port's console-0 seam and `producer` its process-spawn
/// producer — the two hardware-specific inputs the otherwise arch-neutral tail
/// needs.
///
/// On success this never returns. Every fallible *setup* step fails closed
/// with a stable stage string the caller logs.
pub fn finish_unlock<B: Block + 'static>(
    blk: B,
    coop: &CooperativeYield<'_>,
    env: UnlockEnv,
    console: &'static dyn UnlockConsole,
    producer: &'static dyn ProcessSpawn,
) -> Result<Infallible, &'static str> {
    let UnlockEnv {
        ctx,
        audit,
        pressure,
    } = env;

    // The one brought-up disk, boot-leaked to `'static` behind the
    // block-sharing layer so two independent preemptive tasks drive it through
    // their own serialised windows: *this* task is the driver-store serve loop
    // (below), and a *separate* spawned task runs the encrypted-root unlock. A
    // geometry fault refuses the device fail-closed. The device sits behind
    // the SMART11 block cache (`plans/SMARTRAM.md`), on the device side of the
    // sharing lock, so every window reads through one coherent,
    // pressure-governed cache of recently used blocks.
    let blk = BlockCache::for_boot_disk(blk, pressure, audit)
        .map_err(|_| "root-unlock: block device geometry")?;
    rustos_kernel_core::memstats::MEM_STATS.register_ledger(blk.accounting_shared());
    let store: &'static DriverStoreService<BlockCache<B>> =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(DriverStoreService::new(
            SharedBlock::new(blk).map_err(|_| "root-unlock: block device geometry")?,
        )));

    // Spawn the encrypted-root unlock as its own preemptive task. The
    // user-data volume's passphrase is independent of the always-readable
    // `/System` driver store (`plans/PI.md` design B), so the store endpoint
    // (bound + served by *this* task below) answers `devmgr` immediately — the
    // keyboard driver loads in user space *before* the prompt, with no
    // cooperative interleaving on one kthread (two independent tasks sharing
    // the disk). The unlock task drives its own console reader over its own
    // scheduler yield handle.
    let unlock_body = move |yielder: &mut dyn YieldHandle| {
        let coop = CooperativeYield::new(yielder);
        // The primary console (index 0): the port's console-0 seam decides
        // the device (framebuffer text console, discovered UART, …) and arms
        // its interrupt-driven receive so a keystroke wakes the parked reader
        // (nothing cooperative). The read half is the raw device behind the
        // console-0 gate `login` reads through (the gate stays closed until
        // this unlock resolves), so the two never contend.
        let (console_write, raw_read) = console.acquire_console0();
        // The passphrase prompt's secret-entry feedback: the same
        // `[input active...]` marker a user-space password read shows, drawn
        // to this console's own output. Armed for the whole unlock window —
        // the marker only ever appears while a passphrase line is partially
        // typed (it hides on Enter and on a full erase), so the silent blank
        // probe and the mount attempts render nothing.
        let secret = SecretFeedback::new(console_write);
        secret.arm();
        // The kthread's own scheduler id (published at admission), so the
        // reader registers on `CONSOLE_WAITQ` and the RX interrupt unparks it
        // by id.
        let reader = KthreadConsoleRead::new(
            raw_read,
            &coop,
            crate::unlock_service::unlock_console_task(),
            Some(&secret),
        );
        // Publish the writable root volume backing (`/` and its writable
        // subtrees — `/Users`, `/Apps`, `/Storage`, `/System/Logs`,
        // `/System/Settings`) on a successful unlock, from a second `'static`
        // read-write window onto the same `'static`-leaked disk (park-safe via
        // the device `SleepLock`), under the just-derived key.
        let writable = WritableStateSink {
            store,
            audit,
            pressure,
        };
        // The unlock owns console 0 for the passphrase prompt (its
        // `GatedConsoleRead` keeps `login` parked). The moment it resolves — a
        // database installed *or* given up — console 0 must be released to
        // `login`: the port's seam opens the gate, arms the receive interrupt
        // where a `login` reader needs it, and resolves the `LateUsersDb`
        // pending wait. `unlock_root_disk_interactively` calls this
        // `on_resolved` callback exactly once on every internal return path,
        // so a successful unlock can no longer leave the console latched shut.
        let release = || console.release_console0_to_login();
        // The anti-brute-force pause after a wrong passphrase: a genuine timed
        // park (never a busy-wait) for at least three seconds, so a scripted
        // brute-force gains no faster oracle than the honest operator, before
        // the "Incorrect passphrase" notice and re-prompt.
        let retry_delay = || {
            crate::unlock_service::park_for_ns(
                &coop,
                crate::unlock_service::unlock_console_task(),
                crate::unlock_service::WRONG_PASSPHRASE_RETRY_DELAY_NS,
            );
        };
        match unlock_root_disk_interactively(
            store.window(),
            console_write,
            &reader,
            &UnlockInstall {
                users: &LATE_USERS_DB,
                identity: &LATE_IDENTITY,
                writable: &writable,
                admin: Some(AdminInstall {
                    cell: &LATE_USERS_ADMIN,
                    users: &LATE_USERS_DB,
                    identity: &LATE_IDENTITY,
                    audit,
                }),
                storage_gid: &crate::volume_policy::LATE_STORAGE_GID,
            },
            audit,
            &retry_delay,
            &release,
        ) {
            UnlockOutcome::Installed => note(audit, Level::Info, USERS_DB_INSTALLED_MESSAGE),
            UnlockOutcome::GaveUp => note(
                audit,
                Level::Error,
                "root-unlock: gave up fail-closed; login refused until reboot",
            ),
        }
        // The unlock task then ends (the disk stays alive — it is
        // `'static`-leaked — and this task's window borrow ends with it).
    };
    if let Some(unlock_task) = ctx.spawn_kernel_service(alloc::boxed::Box::new(unlock_body)) {
        // Publish the interactive unlock kthread's scheduler id so its
        // passphrase reader can register on `CONSOLE_WAITQ` and the console RX
        // interrupt can unpark it by id. On a multi-core boot the spawned body
        // can start on another CPU before this store lands; the reader
        // tolerates that by re-resolving the id on every poll
        // (`KthreadConsoleRead::read`), so a pre-publish start degrades to at
        // most one transient cooperative yield, never a lost wake.
        crate::unlock_service::set_unlock_console_task(unlock_task);
    } else {
        // The unlock task could not be admitted: nothing will prompt for the
        // passphrase or open the console-0 gate, so open it here (login still
        // refuses, as no database is installed) and serve the store anyway so
        // `devmgr` can load drivers.
        note(
            audit,
            Level::Error,
            "root-unlock: unlock task not admitted; console gate opened, store still served",
        );
        console.release_console0_to_login();
    }

    // Publish the read-only `/System` volume as the userland `fs_*` mount
    // before entering the serve loop: a second, park-safe `'static` window
    // onto the same `'static`-leaked disk (`PREREQUISITES.md` P-A). The store
    // serve loop below keeps its own independent window, so the two never
    // conflict. Fail-soft and audited: a disk with no readable `/System`
    // volume simply leaves the `fs_*` syscalls failing closed.
    install_system_mount(store, audit, pressure);

    // Become the persistent capability-gated driver-store serve loop the
    // user-space `devmgr` autoloads signed `/System` drivers through — bound
    // over the always-readable `/System` volume, independent of the user-data
    // passphrase (`plans/PI.md` design B). Never returns on success.
    serve_driver_store(store, coop, env, producer)
}

/// Become the persistent capability-gated driver-store serve loop the
/// user-space `devmgr` autoloads signed `/System` drivers through.
///
/// Binds and serves the store IPC endpoint over its own `/System` window onto
/// the `'static`-leaked shared disk, real-parking on `SERVE_WAITQ` between
/// requests and never returning on success. Every fallible setup step (a
/// missing trust anchor, an unbindable endpoint, no readable `/System`
/// volume) fails closed — logged, then the kthread parks for life still
/// owning the disk so an `ipc_call` to the unbound endpoint fails closed with
/// `NotFound` rather than blocking.
///
/// `producer` is the port's process-spawn producer, bridged through the
/// arch-neutral [`InitCtxDriverProcessSpawn`] so a matched user-space driver
/// is spawned into its own hardware-isolated process granted exactly its
/// matched node's resources (no ambient authority).
fn serve_driver_store<B: Block + 'static>(
    store: &'static DriverStoreService<BlockCache<B>>,
    coop: &CooperativeYield<'_>,
    env: UnlockEnv,
    producer: &'static dyn ProcessSpawn,
) -> Result<Infallible, &'static str> {
    let audit = env.audit;
    // The driver-signing trust anchor the autoload load gate verifies each
    // winning bundle against — the kernel's own embedded key, the single
    // source `KernelDriverLoader` also trusts. A corrupt key is a broken
    // build, not an admissible state: fail closed rather than autoload against
    // no anchor.
    let trust_anchor = Ed25519PublicKey::from_bytes(&KERNEL_DRIVER_SIGNER_PUBKEY)
        .map_err(|_| "root-unlock: driver trust anchor")?;
    let trusted = [trust_anchor];
    // The scheduler-agnostic driver-spawn seam over the captured boot context
    // + the port's process producer — the one per-arch input the otherwise
    // arch-neutral driver-store load op needs to spawn a verified driver into
    // its own process.
    let driver_spawn = InitCtxDriverProcessSpawn::new(env.ctx, producer);
    // The kernel-side load mechanism the persistent driver-store service keeps
    // in its trusted base (Design D D2b-2c): the driver-signing trust anchor,
    // the delegatable `autoload_caps` gate superset (`CAP_DRV_LOAD` to pass
    // the gate plus the resource caps an autoloaded driver's class may request
    // — `CAP_INPUT_INJECT`/`CAP_IRQ_BIND` for an input driver and
    // `CAP_IPC_BIND_PRIVILEGED` for a bus service driver such as the VideoCore
    // `vcmailbox`, intersected per driver with its signed manifest request),
    // the port process-spawn seam, and the **live** hardware inventory
    // (`crate::hwtree_store::HW_TREE`) a matched `node_id` is resolved against
    // to mint exactly that node's grants (no ambient authority). Resolving
    // against the live store (not a frozen boot snapshot) is what lets a node
    // a user-space bus driver publishes at runtime through `hw_emit_node` be
    // loaded the moment it appears — the recursive bus chain (pcie → vl805 →
    // usb_kbd) depends on it. The user-space `devmgr` owns the matching
    // *policy*; this kthread serves the *mechanism* over the capability-gated
    // store endpoint below.
    let serve_ctx = crate::driver_store_server::StoreServeContext {
        trusted: &trusted,
        caps: autoload_caps(),
        spawn: &driver_spawn,
        nodes: &crate::hwtree_store::HW_TREE,
    };

    // This task is now the persistent driver-store service: it binds and
    // serves the capability-gated store IPC endpoint the user-space `devmgr`
    // reads the signed `/System` driver store through, real-parking on
    // `SERVE_WAITQ` between requests. It serves over its own `/System` window
    // onto the `'static`-leaked shared disk, independently of the
    // encrypted-root unlock task spawned above (`plans/PI.md` design B).
    // `login`, PID 1, `devmgr`, the unlock task, and every other task run on
    // their own tasks.
    //
    // The binder context holds only `IPC_BIND_PRIVILEGED` (the privileged
    // authority to bind the restricted-sender store endpoint), distinct from
    // the kthread's own minimal `service_caps` (no ambient authority).
    let binder = TaskCapabilities::derive(
        UNLOCK_TASK,
        UserId(0),
        store_endpoint_binder_caps(),
        store_endpoint_binder_caps(),
        audit,
    );
    // The persistent `/System` window is taken in an inner scope so that, on a
    // fail-closed fallback, the window borrow of `store` ends before the
    // `store.hold` park. On the success path `serve_system_store` never
    // returns, so the window stays borrowed for the life of the system.
    let outcome = {
        let mut window = store.window();
        with_system_volume(&mut window, audit, |volume| {
            crate::driver_store_server::serve_system_store(volume, &serve_ctx, &binder, coop, audit)
        })
    };
    match outcome {
        // The serve loop never returns on success (`Infallible`).
        Some(Ok(never)) => match never {},
        // The endpoint could not be bound (e.g. its well-known id is already
        // registered, or the mount became unreadable). Fail closed: log the
        // stage and park the kthread for life still owning the disk, so an
        // `ipc_call` to the unbound store endpoint fails closed with
        // `NotFound` rather than blocking.
        Some(Err(stage)) => {
            note(audit, Level::Error, stage);
            store.hold(coop)
        }
        // No read-only `/System` volume on this disk (already audited
        // `SYSTEM_VOLUME_UNAVAILABLE`): nothing to serve. Park for life owning
        // the disk; `devmgr`'s store reads fail closed with `NotFound`.
        None => {
            note(
                audit,
                Level::Error,
                "driver-store: no /System volume to serve; driver-store endpoint not bound",
            );
            store.hold(coop)
        }
    }
}
