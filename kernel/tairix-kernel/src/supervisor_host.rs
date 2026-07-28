//! The binding kernel's [`SupervisorHost`] implementation — the consumer that
//! wires the arch-neutral pre-boot Supervisor engine (`lib/supervisor`) to the
//! real bootstrap-floor state (`plans/NEW-SUPERVISOR.md`).
//!
//! The engine presents and controls; this host is where each command reaches
//! the one existing source of truth. It reuses, never re-derives (the charter
//! forbids duplicating the data):
//!
//! * `version` / `memory` / `memory_map` / `cpu` / `uptime` / `date` /
//!   `memtest` / `reboot` / `poweroff` delegate to the live
//!   [`SupervisorSystem`](tairix_kernel_core::SupervisorSystem) the kernel
//!   core published at boot ([`tairix_kernel_core::supervisor_system()`]) —
//!   the arch handle, the frame allocator, the boot memory map, the wall
//!   clock.
//! * `log_tail` reads the retained boot audit-log ring the boot path
//!   composed ([`tairix_kernel_core::boot_log_tail`]).
//! * `hardware` reads the live discovered inventory
//!   ([`crate::hwtree_store::HW_TREE`]).
//! * `disks` / `partitions` / `arxfs_status` / `list` / `scan_disk` read the
//!   one brought-up boot disk through an independent serialised window onto
//!   the shared block device, using the same `lib/partition` parser and
//!   `/System` mount path the rest of the boot uses.
//! * `mount` runs the **real** [`mount_root_disk_and_load_users`] under the
//!   typed passphrase and, on success, publishes the users database and
//!   writable root exactly as the normal unlock does (through the crate's
//!   `finish_install`) — no oracle, no fail-open.
//!
//! Every state-changing decision is recorded on the hash-chained audit log
//! through [`SupervisorHost::audit`] with a stable event id in the `41xx`
//! root-unlock range. No command reveals key material, and no handler panics
//! on any input (the charter forbids it): a bad argument, an unreadable disk,
//! or a missing record is a rendered message, never a panic.

use tairix_abi::driver::filesystem::NodeKind;
use tairix_abi::driver::{block::Block, DriverError};
use tairix_abi::hwtree::HwDeviceClass;
use tairix_kernel_core::{boot_log_tail, supervisor_system};
use tairix_log::{log, Event, EventId, Level, Sink};
use tairix_partition::{parse_partition_table, PartitionType};
use tairix_supervisor::{MountOutcome, Report, SupervisorEvent, SupervisorHost, TestOutcome};

use crate::block_cache::BlockCache;
use crate::root_mount::{
    finish_install, mount_root_disk_and_load_users, with_system_volume, RootMountError,
    UnlockInstall, UnlockOutcome,
};
use crate::shared_block::DriverStoreService;

/// Audit event: the operator entered the Supervisor console from the boot
/// screen. Recorded loudly because it is a full-authority pre-auth action at
/// the physical console (the physical-attacker class, already out of scope) —
/// a reason to audit, never to weaken a defence.
const SUPERVISOR_ENTERED: EventId = EventId(4150);

/// Audit event: the operator asked to resume the normal boot (`continue`).
const SUPERVISOR_CONTINUE: EventId = EventId(4151);

/// Audit event: the operator requested a machine reset.
const SUPERVISOR_REBOOT: EventId = EventId(4152);

/// Audit event: the operator requested a power-off / halt.
const SUPERVISOR_POWEROFF: EventId = EventId(4153);

/// Audit event: an in-Supervisor root `mount` attempt began. No passphrase
/// byte is ever logged.
const SUPERVISOR_MOUNT_ATTEMPT: EventId = EventId(4154);

/// Audit event: an in-Supervisor `mount` unlocked and mounted the root.
const SUPERVISOR_MOUNT_OK: EventId = EventId(4155);

/// Audit event: an in-Supervisor `mount` failed (wrong passphrase or a
/// structural fault). Never an oracle: a wrong passphrase logs exactly this.
const SUPERVISOR_MOUNT_FAILED: EventId = EventId(4156);

/// The number of logical blocks a single [`scan_disk`](KernelSupervisorHost::scan_disk)
/// read covers. A surface scan reads the device sequentially in chunks so it
/// can poll the abort seam and report progress between reads without holding
/// the block device for one unbounded transfer; it is a read-granularity
/// bound on an operator-initiated scan, not a scalable capacity.
const SCAN_CHUNK_BLOCKS: u64 = 256;

/// One binary mebibyte, the unit disk/scan figures are shown in.
const MIB: u64 = 1024 * 1024;

/// The production [`SupervisorHost`] over the one brought-up boot disk and the
/// live kernel state the boot path already published.
///
/// Built in the root-unlock kthread body (`unlock_orchestrate.rs`), where the
/// shared driver-store service (its window onto the boot disk), the unlock
/// install cells, and the boot audit sink are all in scope. It is **not**
/// `'static`: it borrows the in-scope [`UnlockInstall`] (whose writable-state
/// sink is a stack local of that body) and lives only for the ESC-window /
/// REPL call, so a Supervisor `mount` publishes through the very same install
/// the normal unlock would.
///
/// The rendering, control, memtest, and log-tail commands reach the live
/// kernel state through the set-once globals the boot path published, so this
/// host holds no copy of it — the one source of truth stays where it lives.
pub struct KernelSupervisorHost<'a, B: Block + 'static> {
    /// The shared driver-store service over the `'static`-leaked boot disk;
    /// each disk-reading command opens its own independent serialised window.
    store: &'static DriverStoreService<BlockCache<B>>,
    /// The set-once publish destinations a successful `mount` fills — the same
    /// cells the normal interactive unlock installs into.
    install: &'a UnlockInstall<'a>,
    /// The boot audit sink every state-changing decision is logged through. No
    /// passphrase, key, or volume byte is ever logged.
    audit: &'a dyn Sink,
}

impl<'a, B: Block + 'static> KernelSupervisorHost<'a, B> {
    /// Build the host over the shared boot disk, the unlock install cells, and
    /// the boot audit sink.
    #[must_use]
    pub fn new(
        store: &'static DriverStoreService<BlockCache<B>>,
        install: &'a UnlockInstall<'a>,
        audit: &'a dyn Sink,
    ) -> Self {
        Self {
            store,
            install,
            audit,
        }
    }

    /// Record a Supervisor decision on the hash-chained audit log.
    fn record(&self, id: EventId, level: Level, message: &'static str) {
        log(
            self.audit,
            &Event {
                level,
                id,
                message,
                fields: &[],
            },
        );
    }

    /// The human-facing name of a hardware-tree device class.
    fn class_name(class: Option<HwDeviceClass>) -> &'static str {
        match class {
            Some(HwDeviceClass::Root) => "root",
            Some(HwDeviceClass::Bus) => "bus",
            Some(HwDeviceClass::Cpu) => "cpu",
            Some(HwDeviceClass::Memory) => "memory",
            Some(HwDeviceClass::Timer) => "timer",
            Some(HwDeviceClass::InterruptController) => "irqchip",
            Some(HwDeviceClass::Display) => "display",
            Some(HwDeviceClass::Input) => "input",
            Some(HwDeviceClass::Network) => "network",
            Some(HwDeviceClass::Storage) => "storage",
            Some(HwDeviceClass::Serial) => "serial",
            Some(HwDeviceClass::Other) | None => "other",
        }
    }

    /// The human-facing name of a partition's TAIRiX role.
    fn partition_role(ty: PartitionType) -> &'static str {
        match ty {
            PartitionType::FatBoot => "fat-boot",
            PartitionType::ARXFSSystem => "arxfs-system",
            PartitionType::ARXFSRoot => "arxfs-root (encrypted)",
            PartitionType::Other => "other",
        }
    }
}

impl<B: Block + 'static> SupervisorHost for KernelSupervisorHost<'_, B> {
    fn version(&mut self, out: &mut dyn Report) {
        if let Some(sys) = supervisor_system() {
            sys.version(out);
        } else {
            out.line("version: system state provider not installed");
        }
    }

    fn memory(&mut self, out: &mut dyn Report) {
        if let Some(sys) = supervisor_system() {
            sys.memory(out);
        } else {
            out.line("mem: system state provider not installed");
        }
    }

    fn memory_map(&mut self, out: &mut dyn Report) {
        if let Some(sys) = supervisor_system() {
            sys.memory_map(out);
        } else {
            out.line("mem map: system state provider not installed");
        }
    }

    fn cpu(&mut self, out: &mut dyn Report) {
        if let Some(sys) = supervisor_system() {
            sys.cpu(out);
        } else {
            out.line("cpu: system state provider not installed");
        }
    }

    fn hardware(&mut self, out: &mut dyn Report) {
        let nodes = crate::hwtree_store::HW_TREE.snapshot();
        if nodes.is_empty() {
            out.line("hardware tree: empty (no nodes discovered)");
            return;
        }
        out.line("hardware tree:");
        for node in &nodes {
            out.write_str("  node ");
            out.write_u64(u64::from(node.id()));
            out.write_str(" parent ");
            out.write_u64(u64::from(node.parent()));
            out.write_str("  ");
            out.write_str(Self::class_name(node.class()));
            out.write_str("  keys ");
            out.write_u64(node.match_keys().len() as u64);
            out.newline();
        }
    }

    fn disks(&mut self, out: &mut dyn Report) {
        let window = self.store.window();
        match window.geometry() {
            Ok(geometry) => {
                let total = geometry
                    .block_count
                    .saturating_mul(u64::from(geometry.block_size));
                out.write_str("disk0: ");
                out.write_u64(geometry.block_count);
                out.write_str(" blocks x ");
                out.write_u64(u64::from(geometry.block_size));
                out.write_str(" B = ");
                out.write_u64(total / MIB);
                out.line(" MiB");
            }
            Err(_) => out.line("disk0: geometry unavailable"),
        }
    }

    fn partitions(&mut self, _device: &str, out: &mut dyn Report) {
        let mut window = self.store.window();
        match parse_partition_table(&mut window) {
            Ok(table) => {
                let parts = table.partitions();
                if parts.is_empty() {
                    out.line("partitions: table is empty");
                    return;
                }
                out.line("partitions:");
                for (index, part) in parts.iter().enumerate() {
                    out.write_str("  ");
                    out.write_u64(index as u64);
                    out.write_str("  ");
                    out.write_str(Self::partition_role(part.ty));
                    out.write_str("  lba ");
                    out.write_u64(part.start_lba);
                    out.write_str(" + ");
                    out.write_u64(part.block_count);
                    out.line(" blocks");
                }
            }
            Err(_) => out.line("partitions: no valid MBR/GPT table"),
        }
    }

    fn arxfs_status(&mut self, out: &mut dyn Report) {
        let mut window = self.store.window();
        let Ok(table) = parse_partition_table(&mut window) else {
            out.line("arxfs: no partition table (root status unknown)");
            return;
        };
        let system = table.first_of_type(PartitionType::ARXFSSystem).is_some();
        let root = table.first_of_type(PartitionType::ARXFSRoot).is_some();
        out.write_str("arxfs /System partition: ");
        out.line(if system { "present" } else { "absent" });
        out.write_str("arxfs root partition:    ");
        out.line(if root {
            "present (encrypted)"
        } else {
            "absent"
        });
        // Pre-mount status is reported without unlocking: the encrypted root
        // is never opened to answer a status query (that would need the
        // secret and is not this command's job).
        out.line("arxfs root state:        locked (not mounted from the Supervisor)");
    }

    fn list(&mut self, _path: Option<&str>, out: &mut dyn Report) {
        // Pre-mount the only readable volume is the always-readable `/System`
        // (the signed driver store lives there). Say so, then list its root.
        let mut window = self.store.window();
        let listed = with_system_volume(&mut window, self.audit, |volume| {
            out.line("/System (read-only, pre-mount):");
            let root = volume.root();
            let mut cursor = 0u64;
            let mut name = [0u8; 256];
            loop {
                match volume.read_dir(root, cursor, &mut name) {
                    Ok(Some(entry)) => {
                        out.write_str("  ");
                        let len = entry.name_len.min(name.len());
                        out.write_bytes(&name[..len]);
                        if entry.info.kind == NodeKind::Directory {
                            out.write_str("/");
                        }
                        out.newline();
                        cursor = entry.next_cursor;
                    }
                    // End of the listing.
                    Ok(None) => break,
                    // A name that did not fit is skipped rather than aborting
                    // the whole listing; a device fault ends it fail-soft.
                    Err(DriverError::BufferTooSmall) => {
                        cursor = cursor.saturating_add(1);
                    }
                    Err(_) => {
                        out.line("  (listing ended on a read error)");
                        break;
                    }
                }
            }
        });
        if listed.is_none() {
            out.line("ls: no readable /System volume before the root is mounted");
        }
    }

    fn log_tail(&mut self, count: Option<usize>, out: &mut dyn Report) {
        let Some(tail) = boot_log_tail() else {
            out.line("log: boot audit-log ring not installed");
            return;
        };
        let Some((oldest, newest)) = tail.seq_range() else {
            out.line("log: no boot audit records retained");
            return;
        };
        // "last k of N": start at the k-th newest retained record when a count
        // was given, otherwise the whole retained window.
        let start = match count {
            Some(k) => newest
                .saturating_sub(k.saturating_sub(1) as u64)
                .max(oldest),
            None => oldest,
        };
        out.write_str("boot audit log (");
        out.write_u64(tail.total());
        out.line(" records total):");
        let mut seq = start;
        while seq <= newest {
            if let Some(record) = tail.record(seq) {
                out.write_str("  #");
                out.write_u64(record.seq);
                out.write_str(" id ");
                out.write_u64(u64::from(record.id.0));
                out.write_str("  ");
                out.write_str(record.message());
                out.newline();
            }
            seq = seq.saturating_add(1);
        }
    }

    fn panic_log(&mut self, out: &mut dyn Report) {
        // A persisted cross-boot panic/lockup record store does not yet exist
        // (`plans/FIX-PANICS.md` / `plans/WATCHDOG.md` record to the live
        // serial + audit sink, not to a persistent slot). Report honestly that
        // there is none rather than fabricate a record.
        out.line("panic-log: no persisted previous-boot diagnostic record");
    }

    fn uptime(&mut self, out: &mut dyn Report) {
        if let Some(sys) = supervisor_system() {
            sys.uptime(out);
        } else {
            out.line("uptime: system state provider not installed");
        }
    }

    fn date(&mut self, out: &mut dyn Report) {
        if let Some(sys) = supervisor_system() {
            sys.date(out);
        } else {
            out.line("date: system state provider not installed");
        }
    }

    fn memtest(
        &mut self,
        passes: u32,
        out: &mut dyn Report,
        abort: &mut dyn FnMut() -> bool,
    ) -> TestOutcome {
        let Some(sys) = supervisor_system() else {
            out.line("memtest: system state provider not installed");
            return TestOutcome::Aborted;
        };
        sys.memtest(passes, out, abort)
    }

    fn scan_disk(
        &mut self,
        _device: &str,
        out: &mut dyn Report,
        abort: &mut dyn FnMut() -> bool,
    ) -> TestOutcome {
        let mut window = self.store.window();
        let Ok(geometry) = window.geometry() else {
            out.line("scan-disk: geometry unavailable");
            return TestOutcome::Failed;
        };
        let block_size = usize::try_from(geometry.block_size).unwrap_or(0);
        if block_size == 0 || geometry.block_count == 0 {
            out.line("scan-disk: device reports no readable blocks");
            return TestOutcome::Failed;
        }
        let chunk_blocks = SCAN_CHUNK_BLOCKS.min(geometry.block_count);
        let chunk_len = usize::try_from(chunk_blocks).unwrap_or(0);
        // A fixed on-stack read buffer sized for one chunk; never a heap
        // allocation per read. The scan is read-only — it never writes.
        let mut buf = alloc::vec![0u8; block_size * chunk_len];
        let mut lba = 0u64;
        let mut next_progress = 0u64;
        while lba < geometry.block_count {
            if abort() {
                out.write_str("\r  aborted after ");
                out.write_u64(lba.saturating_mul(u64::from(geometry.block_size)) / MIB);
                out.line(" MiB");
                return TestOutcome::Aborted;
            }
            let blocks = chunk_blocks.min(geometry.block_count - lba);
            let bytes = block_size * usize::try_from(blocks).unwrap_or(0);
            if window.read_blocks(lba, &mut buf[..bytes]).is_err() {
                out.write_str("\r  READ ERROR at block ");
                out.write_u64(lba);
                out.newline();
                return TestOutcome::Failed;
            }
            lba += blocks;
            let done_mib = lba.saturating_mul(u64::from(geometry.block_size)) / MIB;
            if done_mib >= next_progress {
                out.write_bytes(b"\r  scanned ");
                out.write_u64(done_mib);
                out.write_str(" MiB");
                next_progress = done_mib + 64;
            }
        }
        out.write_bytes(b"\r  ");
        out.write_u64(lba.saturating_mul(u64::from(geometry.block_size)) / MIB);
        out.line(" MiB scanned with no read error");
        TestOutcome::Passed
    }

    fn mount(&mut self, passphrase: &[u8], out: &mut dyn Report) -> MountOutcome {
        // The real unlock: derive the key from the typed passphrase and mount
        // the encrypted root. No oracle and no fail-open — a wrong passphrase
        // is exactly `Mount(PermissionDenied)`, indistinguishable from the
        // normal path. On success, publish the users database + writable root
        // through the same install cells the interactive unlock uses.
        match mount_root_disk_and_load_users(self.store.window(), passphrase, self.audit) {
            Ok(unlocked) => match finish_install(unlocked, self.install, self.audit) {
                UnlockOutcome::Installed => {
                    out.line("root unlocked and mounted");
                    MountOutcome::Mounted
                }
                UnlockOutcome::GaveUp => {
                    out.line("root unlocked but the users database could not be installed");
                    MountOutcome::Failed
                }
            },
            Err(RootMountError::Mount(DriverError::PermissionDenied)) => {
                out.line("incorrect passphrase");
                MountOutcome::WrongPassphrase
            }
            Err(error) => {
                out.write_str("root cannot be mounted: ");
                out.line(error.cause());
                MountOutcome::Failed
            }
        }
    }

    fn reboot(&mut self) {
        if let Some(sys) = supervisor_system() {
            sys.reboot();
        }
    }

    fn poweroff(&mut self) {
        if let Some(sys) = supervisor_system() {
            sys.poweroff();
        }
    }

    fn audit(&mut self, event: SupervisorEvent) {
        let (id, level, message) = match event {
            SupervisorEvent::Entered => (
                SUPERVISOR_ENTERED,
                Level::Warn,
                "supervisor: console entered at the physical boot screen",
            ),
            SupervisorEvent::ContinueBoot => (
                SUPERVISOR_CONTINUE,
                Level::Info,
                "supervisor: resume normal boot requested",
            ),
            SupervisorEvent::Reboot => (
                SUPERVISOR_REBOOT,
                Level::Warn,
                "supervisor: machine reboot requested",
            ),
            SupervisorEvent::Poweroff => (
                SUPERVISOR_POWEROFF,
                Level::Warn,
                "supervisor: machine power-off requested",
            ),
            SupervisorEvent::MountAttempt => (
                SUPERVISOR_MOUNT_ATTEMPT,
                Level::Info,
                "supervisor: in-console root mount attempt",
            ),
            SupervisorEvent::MountOk => (
                SUPERVISOR_MOUNT_OK,
                Level::Info,
                "supervisor: in-console root mount succeeded",
            ),
            SupervisorEvent::MountFailed => (
                SUPERVISOR_MOUNT_FAILED,
                Level::Warn,
                "supervisor: in-console root mount failed",
            ),
        };
        self.record(id, level, message);
    }
}
