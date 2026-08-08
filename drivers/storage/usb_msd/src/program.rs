//! The freestanding body of the mass-storage `Run` binary (`main.rs`):
//! URB-backed transport, LUN bring-up, per-LUN block-service publication,
//! and the wait-set serve loop (`plans/DEVICES.md` D2).

use tairix_abi::blkio::{
    fault_domain_wait_timeout, recovery_wait_timeout, BlkDeviceClass, BlkHealth, BlkHealthState,
    BlkStatus, FaultDomain, FaultDomainState, RecoveryAction, RecoveryLadder, BLK_COMPLETION_LEN,
    BLK_DATA_LEN, BLK_REQUEST_LEN,
};
use tairix_abi::hwtree::{ancestor_imposed_status_from_snapshot, HW_NODE_ROOT};
use tairix_abi::sysinfo::BlkHealthTransition;
use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
use tairix_abi::{CapabilityId, Errno, HwDeviceClass, HwMatchKey, HwNode, HwResource};
use tairix_caps::CapabilitySet;
use tairix_drv_storage_usb_msd::bot::{Bot, MsdTransport};
use tairix_drv_storage_usb_msd::cbi::{Cbi, CbiStatus};
use tairix_drv_storage_usb_msd::desc::{
    configuration_total_length, find_storage_interface, StorageInterface, StorageProtocol,
    UasEndpoints, CONFIGURATION_HEADER_LEN,
};
use tairix_drv_storage_usb_msd::recover::{serve_lun_with_domain, LunRecovery, ServeBuffers};
use tairix_drv_storage_usb_msd::scsi::{
    CommandSet, LunBlock, LunState, ScsiDevice, ScsiTransport, DEVICE_TYPE_DIRECT_ACCESS, MAX_LUNS,
};
use tairix_drv_storage_usb_msd::serve::blk_block_for;
use tairix_drv_storage_usb_msd::uas::{Uas, UasPipes};
use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};
use tairix_log::{log, Event, EventId, Field, FieldValue, Level};
use tairix_rt::LogSink;
use tairix_usb::device::BULK_BUF_LEN;
use tairix_usb::transport::{UrbCall, UrbClient};
use tairix_util::fmt::format_hex_u64;

/// Exit code when the rt-backed driver host could not be built from the
/// kernel-delivered grants. A reserved, fail-closed value.
const EXIT_NO_HOST: i32 = 80;

/// Exit code when the matched interface node did not carry the URB
/// transport endpoint and shared-buffer grants this driver needs.
const EXIT_NO_TRANSPORT: i32 = 81;

/// Exit code when the device's descriptors or LUN bring-up refused every
/// unit (no disk to serve).
const EXIT_BRINGUP_FAILED: i32 = 82;

/// Exit code when a per-LUN block service (endpoint, window, node, or the
/// wait-set) could not be stood up.
const EXIT_NO_SERVICE: i32 = 83;

/// Diagnostic event id: the one-shot "LUNs published, serving" beacon.
const MSD_READY: EventId = EventId(4160);

/// Diagnostic event id: a bring-up step completed.
const MSD_SETUP: EventId = EventId(4161);

/// Diagnostic event id: a bring-up or serve step failed.
const MSD_ERROR: EventId = EventId(4162);

/// Diagnostic event id: one LUN was brought up and its node published.
const MSD_LUN_READY: EventId = EventId(4163);

/// Diagnostic event id: one LUN was skipped (non-disk type or never
/// became ready) — logged, never a crash.
const MSD_LUN_SKIPPED: EventId = EventId(4164);

/// Diagnostic event id: the interface disappeared; nodes retracted and
/// the driver exits for a clean reload on re-plug.
const MSD_DETACHED: EventId = EventId(4165);

/// Diagnostic event id: a LUN's recovery grace window elapsed with no
/// request and no recovery, so it failed closed. The disk stays present
/// and its endpoint keeps serving fail-closed answers — a later genuine
/// return still recovers it (sticky-but-recoverable, no reboot).
const MSD_GRACE_EXPIRED: EventId = EventId(4166);

/// Diagnostic event id: the recovery ladder escalated to a data-path reset
/// for a stalling LUN — the bounded step taken to try to bring a recovering
/// unit back before its grace window is left to fail it closed.
const MSD_RECOVERY_RESET: EventId = EventId(4167);

/// Diagnostic event id: a LUN reported itself unhealthy while still serving
/// valid data (a recovered-error threshold, a pending sector reallocation) —
/// the device-level [`BlkHealthTransition::Degraded`] edge.
const MSD_HEALTH_DEGRADED: EventId = EventId(4168);

/// Diagnostic event id: a LUN stalled or reset and entered its bounded
/// recovery grace window — the device-level [`BlkHealthTransition::Recovering`]
/// edge. Its I/O is ridden out reissuably while it is given a chance to return.
const MSD_HEALTH_RECOVERING: EventId = EventId(4169);

/// Diagnostic event id: a degraded, recovering, or failed-closed LUN returned
/// to healthy service — the device-level [`BlkHealthTransition::Recovered`]
/// edge. "The disk came back": logged as a recovery, not a fault.
const MSD_HEALTH_RECOVERED: EventId = EventId(4170);

/// Diagnostic event id: the device's shared transport reset, holding every
/// LUN reissuable under one recovery window — the fault-domain
/// [`BlkHealthTransition::Recovering`] edge for the shared BOT transport.
const MSD_DOMAIN_RECOVERING: EventId = EventId(4171);

/// Diagnostic event id: the device's shared transport demonstrably returned
/// (a unit completed a real transfer), recovering the whole device — the
/// fault-domain [`BlkHealthTransition::Recovered`] edge.
const MSD_DOMAIN_RECOVERED: EventId = EventId(4172);

/// Diagnostic event id: the shared-transport recovery window elapsed without a
/// return, failing the whole device closed. It stays sticky-but-recoverable —
/// a later genuine transfer recovers it with no reboot.
const MSD_DOMAIN_OFFLINE: EventId = EventId(4173);

/// Outstanding-request capacity of a per-LUN endpoint. The volume layer
/// submits one request at a time (it blocks on the reply); a small queue
/// absorbs a re-submit racing the previous reply.
const BLK_ENDPOINT_CAPACITY: usize = 4;

/// Bounded bring-up TEST UNIT READY attempts per LUN. Each failed
/// attempt reads (and thereby clears) the unit's sense state — the
/// standard start-of-day UNIT ATTENTION drain — so this is a fixed
/// number of real round trips, never a hot spin.
const READY_ATTEMPTS: usize = 8;

/// Wait forever on the serve wait-set (block requests arrive whenever a
/// consumer issues them).
const WAIT_FOREVER_NS: u64 = u64::MAX;

/// The capability set the driver host re-checks up front; the kernel is
/// the authority and re-checks every trap. It is the least-privilege set
/// this class driver needs — no MMIO, DMA, or IRQ.
fn driver_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::SHM);
    caps.insert(CapabilityId::IPC_ENDPOINT);
    caps.insert(CapabilityId::IPC_BIND_PRIVILEGED);
    caps.insert(CapabilityId::HW_EMIT);
    caps.insert(CapabilityId::LOG_EMIT);
    caps
}

/// The class-side URB transport: one synchronous, capability-checked
/// `ipc_call` to the host-controller driver's per-interface endpoint. It
/// records a vanished endpoint (`Errno::NotFound`) so the serve loop can
/// retract the LUN nodes and exit for a clean reload.
struct IpcUrbCall {
    endpoint: u64,
    disconnected: bool,
}

impl UrbCall for IpcUrbCall {
    fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
        match tairix_rt::ipc_call(self.endpoint, request, reply) {
            Ok(len) => Ok(len),
            Err(neg) => {
                let errno =
                    Errno::from_i32(i32::try_from(-neg).unwrap_or(0)).unwrap_or(Errno::NotFound);
                if errno == Errno::NotFound {
                    self.disconnected = true;
                }
                Err(errno)
            }
        }
    }
}

/// The driver's one URB link to its interface: the call client plus this
/// driver's mapping of the shared URB data buffer (the HCD maps the same
/// frames and moves each transfer's bytes through it). Both wire-transport
/// adapters — [`UrbTransport`] for BOT/CBI and [`UasUrbPipes`] for UAS —
/// move their bytes through these same primitives, so the chunking and
/// bounce-copy logic exists once. Bulk transfers are split into per-URB
/// chunks of at most one buffer ([`BULK_BUF_LEN`], the engine's per-TD
/// ceiling); a short chunk ends the transfer honestly.
struct UrbLink {
    client: UrbClient<IpcUrbCall>,
    shm: &'static mut [u8],
}

impl UrbLink {
    /// Whether the underlying transport endpoint has vanished (the HCD
    /// retracted the interface — the device was unplugged).
    fn disconnected(&self) -> bool {
        self.client.transport().disconnected
    }

    fn control_in(&mut self, setup: [u8; 8], data: &mut [u8]) -> Result<usize, Errno> {
        let len =
            u32::try_from(data.len().min(self.shm.len())).map_err(|_| Errno::LengthOutOfRange)?;
        let n = self.client.control_in(setup, 0, len)? as usize;
        let n = n.min(data.len()).min(self.shm.len());
        data[..n].copy_from_slice(&self.shm[..n]);
        Ok(n)
    }

    fn control_out(&mut self, setup: [u8; 8], data: &[u8]) -> Result<(), Errno> {
        if data.len() > self.shm.len() {
            return Err(Errno::LengthOutOfRange);
        }
        self.shm[..data.len()].copy_from_slice(data);
        let len = u32::try_from(data.len()).map_err(|_| Errno::LengthOutOfRange)?;
        self.client.control_out(setup, 0, len)
    }

    fn control_no_data(&mut self, setup: [u8; 8]) -> Result<(), Errno> {
        self.client.control_no_data(setup)
    }

    fn bulk_in(&mut self, endpoint: u8, data: &mut [u8]) -> Result<usize, Errno> {
        let mut off = 0usize;
        while off < data.len() {
            let chunk = (data.len() - off).min(BULK_BUF_LEN);
            let chunk_u32 = u32::try_from(chunk).map_err(|_| Errno::LengthOutOfRange)?;
            let n = self.client.bulk_in(endpoint, 0, chunk_u32)? as usize;
            let n = n.min(chunk);
            data[off..off + n].copy_from_slice(&self.shm[..n]);
            off += n;
            if n < chunk {
                break; // Short packet: the device ended the phase early.
            }
        }
        Ok(off)
    }

    fn bulk_out(&mut self, endpoint: u8, data: &[u8]) -> Result<usize, Errno> {
        let mut off = 0usize;
        while off < data.len() {
            let chunk = (data.len() - off).min(BULK_BUF_LEN);
            self.shm[..chunk].copy_from_slice(&data[off..off + chunk]);
            let chunk_u32 = u32::try_from(chunk).map_err(|_| Errno::LengthOutOfRange)?;
            let n = self.client.bulk_out(endpoint, 0, chunk_u32)? as usize;
            let n = n.min(chunk);
            off += n;
            if n < chunk {
                break;
            }
        }
        Ok(off)
    }

    fn interrupt_in(&mut self, endpoint: u8, data: &mut [u8]) -> Result<usize, Errno> {
        let len =
            u32::try_from(data.len().min(self.shm.len())).map_err(|_| Errno::LengthOutOfRange)?;
        let n = self.client.interrupt_in(endpoint, 0, len)? as usize;
        let n = n.min(data.len()).min(self.shm.len());
        data[..n].copy_from_slice(&self.shm[..n]);
        Ok(n)
    }

    fn scrub(&mut self) {
        self.shm.fill(0);
    }
}

/// [`MsdTransport`] (the BOT/CBI seam) over the URB link, addressing the
/// endpoints the device's own configuration descriptor named.
struct UrbTransport {
    link: UrbLink,
    bulk_in_endpoint: u8,
    bulk_out_endpoint: u8,
    /// The CBI command-completion interrupt endpoint; `0` for a BOT
    /// interface, whose transport never reads one (a use is refused).
    interrupt_endpoint: u8,
}

impl MsdTransport for UrbTransport {
    fn control_in(&mut self, setup: [u8; 8], data: &mut [u8]) -> Result<usize, Errno> {
        self.link.control_in(setup, data)
    }

    fn control_out(&mut self, setup: [u8; 8], data: &[u8]) -> Result<(), Errno> {
        self.link.control_out(setup, data)
    }

    fn control_no_data(&mut self, setup: [u8; 8]) -> Result<(), Errno> {
        self.link.control_no_data(setup)
    }

    fn bulk_in(&mut self, data: &mut [u8]) -> Result<usize, Errno> {
        self.link.bulk_in(self.bulk_in_endpoint, data)
    }

    fn bulk_out(&mut self, data: &[u8]) -> Result<usize, Errno> {
        self.link.bulk_out(self.bulk_out_endpoint, data)
    }

    fn interrupt_in(&mut self, data: &mut [u8]) -> Result<usize, Errno> {
        if self.interrupt_endpoint == 0 {
            return Err(Errno::NotImplemented);
        }
        self.link.interrupt_in(self.interrupt_endpoint, data)
    }

    fn scrub(&mut self) {
        self.link.scrub();
    }
}

/// [`UasPipes`] over the URB link, addressing the four pipes the Pipe
/// Usage descriptors named.
struct UasUrbPipes {
    link: UrbLink,
    endpoints: UasEndpoints,
}

impl UasPipes for UasUrbPipes {
    fn command_out(&mut self, iu: &[u8]) -> Result<(), Errno> {
        self.link.bulk_out(self.endpoints.command, iu).map(|_| ())
    }

    fn status_in(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        self.link.bulk_in(self.endpoints.status, buf)
    }

    fn data_in(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        self.link.bulk_in(self.endpoints.data_in, buf)
    }

    fn data_out(&mut self, buf: &[u8]) -> Result<usize, Errno> {
        self.link.bulk_out(self.endpoints.data_out, buf)
    }

    fn scrub(&mut self) {
        self.link.scrub();
    }
}

/// Emit one structured diagnostic event with a single hex field.
fn log_hex_event(id: EventId, level: Level, message: &'static str, key: &'static str, value: u64) {
    let mut value_buf = [0u8; 16];
    log(
        &LogSink,
        &Event {
            level,
            id,
            message,
            fields: &[Field {
                key,
                value: FieldValue::Str(format_hex_u64(value, &mut value_buf)),
            }],
        },
    );
}

/// The 8-byte SETUP of a standard `GET_DESCRIPTOR(CONFIGURATION, 0)` for
/// `length` bytes (USB 2.0 §9.4.3).
fn get_configuration_setup(length: u16) -> [u8; 8] {
    let len = length.to_le_bytes();
    [0x80, 0x06, 0x00, 0x02, 0x00, 0x00, len[0], len[1]]
}

/// One published logical unit: its serve endpoint, shared data window,
/// emitted node, brought-up state, and health/recovery state machine.
struct LunServe {
    endpoint: u64,
    node_id: u32,
    state: LunState,
    window: &'static mut [u8],
    /// The per-LUN health state machine and recovery grace window. A USB
    /// mass-storage unit is a removable device (bus resets, surprise
    /// removal), so it is served with that class's budget.
    health: BlkHealth,
    /// The per-LUN recovery-escalation ladder: what this driver *does* to the
    /// hardware between reissued attempts while the unit is recovering (a
    /// gentle retry first, then an escalating data-path reset), bounded by the
    /// same removable-class budget the health window uses.
    ladder: RecoveryLadder,
}

/// Create the block-service endpoint `id`. Binding it grant-restricted
/// (`send_caps` carries `CAP_IPC_ENDPOINT`) makes the kernel mint this
/// driver the matching per-endpoint grant, which it forwards onto the LUN
/// node so a consumer inherits exactly the right to drive this one unit.
/// `false` if the kernel refused the id.
fn create_blk_endpoint(id: u64) -> bool {
    let mut send_caps = CapabilitySet::empty();
    send_caps.insert(CapabilityId::IPC_ENDPOINT);
    let recv_caps = CapabilitySet::empty();
    tairix_rt::call_create(
        id,
        &send_caps,
        &recv_caps,
        BLK_REQUEST_LEN,
        BLK_COMPLETION_LEN,
        BLK_ENDPOINT_CAPACITY,
    ) == 0
}

/// Bind LUN `lun`'s block-service endpoint inside this driver's derived
/// block `block_base`: the id is `block_base + lun`. Every create must
/// succeed first try — a refusal means the reserved range was squatted
/// on, and the bring-up fails closed.
fn bind_blk_endpoint(block_base: u64, lun: u8) -> Option<u64> {
    let id = block_base + u64::from(lun);
    if create_blk_endpoint(id) {
        Some(id)
    } else {
        None
    }
}

/// Create a LUN's shared data window, returning this driver's mapping and
/// the shm id forwarded as the node's grant.
fn create_window() -> Option<(&'static mut [u8], u64)> {
    let mut shm_id = 0u64;
    // A negative return is the errno and a base this pointer width cannot hold
    // is equally unusable: either way there is no window.
    let base = usize::try_from(tairix_rt::shm_create(BLK_DATA_LEN, &mut shm_id)).ok()?;
    // SAFETY: `shm_create` mapped `BLK_DATA_LEN` bytes of zeroed,
    // cacheable, RW (non-executable) memory into this process at `base`
    // and returned that base. The region is owned by this process for the
    // rest of its life (never unmapped here), and no other reference in
    // this address space aliases it, so a single exclusive `&mut [u8]`
    // over exactly the requested length is sound. The consumer maps the
    // same frames through its own inherited grant.
    let window = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, BLK_DATA_LEN) };
    Some((window, shm_id))
}

/// Publish one LUN's storage node: class `Storage`, a
/// `tairix,usb-msd-lun` compatible key the volume layer selects on, and
/// the two transport grants (the block-service endpoint and the shared
/// data window). Returns the kernel-assigned node id.
fn emit_lun_node(endpoint: u64, shm_id: u64) -> Option<u32> {
    let mut node = HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Storage);
    let key = HwMatchKey::compatible(b"tairix,usb-msd-lun").ok()?;
    node.push_match_key(key).ok()?;
    node.push_resource(HwResource::endpoint(endpoint)).ok()?;
    node.push_resource(HwResource::shared(shm_id)).ok()?;
    // A negative return is the errno; anything else is the assigned node id.
    u32::try_from(tairix_rt::hw_emit_node(&node)).ok()
}

/// Bring one LUN up: identity, the bounded ready drain, geometry, and
/// write policy. `Ok(None)` skips the unit (not a disk / never ready);
/// `Err` is a transport-level failure.
fn bring_up_lun<T: ScsiTransport>(
    scsi: &mut ScsiDevice<T>,
    lun: u8,
) -> Result<Option<LunState>, Errno> {
    let inquiry = scsi.inquiry(lun)?;
    if inquiry.device_type != DEVICE_TYPE_DIRECT_ACCESS {
        log_hex_event(
            MSD_LUN_SKIPPED,
            Level::Info,
            "usb-msd: LUN skipped (not a direct-access unit)",
            "type_hex",
            u64::from(inquiry.device_type),
        );
        return Ok(None);
    }
    // Drain the start-of-day UNIT ATTENTION / not-ready states — bounded;
    // a unit that never becomes ready is skipped, not spun on.
    if !scsi.ready_after_drain(lun, READY_ATTEMPTS)? {
        log_hex_event(
            MSD_LUN_SKIPPED,
            Level::Warn,
            "usb-msd: LUN skipped (never became ready)",
            "lun_hex",
            u64::from(lun),
        );
        return Ok(None);
    }
    let geometry = scsi.read_capacity(lun)?;
    let write_protected = scsi.write_protected(lun)?;
    Ok(Some(LunState {
        geometry,
        write_protected,
    }))
}

/// Retract every published LUN node (device unplugged / fatal exit).
fn retract_all(luns: &[Option<LunServe>]) {
    for lun in luns.iter().flatten() {
        // A device unplug or fatal exit is a surprise removal, never refused
        // for being in use, so the flag set is empty.
        let _ = tairix_rt::hw_remove_node(lun.node_id, tairix_abi::HwRemoveFlags::empty());
    }
}

/// Advance every published LUN's recovery grace window on a pure time tick
/// at monotonic `now_ns`, failing closed any unit that has stayed
/// `Recovering` past its window without a further request to fold through
/// `observe` (`BlkHealth::poll`). Called on every serve-loop wake — the
/// grace-timer one-shot firing *and* a request wake (another LUN's window
/// may have come due while parked on this one) — so a quiet stalled disk
/// still fails closed on time, driven by the timer rather than a busy-poll.
///
/// A newly failed-closed LUN is logged once, keeps its node and endpoint so
/// its consumer receives typed fail-closed answers, and stays
/// sticky-but-recoverable: a later genuine return recovers it via `observe`
/// with no reboot and no node retraction (retraction is surprise-removal,
/// a distinct event).
fn expire_idle_grace_windows(luns: &mut [Option<LunServe>], now_ns: u64) {
    for serve in luns.iter_mut().flatten() {
        let before = serve.health.state();
        let after = serve.health.poll(now_ns);
        note_health_edge(before, after, serve.node_id);
    }
}

/// Record a LUN's device-level health edge from `before` to `after` as one
/// audit event, naming the unit's fault-domain node, so a returning disk, a
/// degrade, a grace-window entry, and a fail-closed each land on the health
/// trail (`plans/FIX-IO.md` IO5).
///
/// The Degraded / Recovering / Recovered edges use the **shared**
/// [`BlkHealthTransition`] vocabulary — the same the kernel block client emits
/// for a volume — so a driver process and the consumer cannot classify a
/// recovery or a degrade differently. A fail-closed edge (the grace window
/// elapsing, or a hard fault) is not part of that vocabulary and is logged as
/// the distinct [`MSD_GRACE_EXPIRED`] event; the disk stays present and
/// sticky-but-recoverable, so a later genuine return is a `Recovered` edge, not
/// a surprise-removal. Surprise removal itself is the hotplug path's event and
/// is never logged here. Edge-triggered: an unchanged state logs nothing, so a
/// run of identical outcomes is one event, not one per request.
fn note_health_edge(before: BlkHealthState, after: BlkHealthState, node_id: u32) {
    if before == after {
        return;
    }
    if let Some(transition) = BlkHealthTransition::for_device(before, after) {
        let (id, level, message) = match transition {
            BlkHealthTransition::Degraded => (
                MSD_HEALTH_DEGRADED,
                Level::Warn,
                "usb-msd: LUN reports itself degraded but still serving",
            ),
            BlkHealthTransition::Recovering => (
                MSD_HEALTH_RECOVERING,
                Level::Warn,
                "usb-msd: LUN stalled/reset, entered its recovery grace window",
            ),
            BlkHealthTransition::Recovered => (
                MSD_HEALTH_RECOVERED,
                Level::Info,
                "usb-msd: LUN recovered, serving normally again",
            ),
        };
        log_hex_event(id, level, message, "node_hex", u64::from(node_id));
    } else if matches!(
        after,
        BlkHealthState::Faulted | BlkHealthState::Offline | BlkHealthState::Failed
    ) {
        log_hex_event(
            MSD_GRACE_EXPIRED,
            Level::Warn,
            "usb-msd: LUN recovery grace window elapsed, failing closed",
            "node_hex",
            u64::from(node_id),
        );
    }
}

/// Record a shared-transport fault-domain edge from `before` to `after` as
/// one audit event, naming the device's transport owner id.
///
/// It uses the **shared** [`BlkHealthTransition`] vocabulary — the same the
/// per-device LUN edges and the kernel block client use — so a device-wide
/// transport recovery and a per-LUN recovery cannot be classified differently.
/// A quiesce (`Healthy → Recovering`, or a re-entry from `Offline`) is
/// [`BlkHealthTransition::Recovering`]; a demonstrated return
/// (`Recovering | Offline → Healthy`) is [`BlkHealthTransition::Recovered`].
/// The fail-closed edge (`→ Offline`, the shared window elapsing) is not part
/// of that vocabulary and is logged as the distinct [`MSD_DOMAIN_OFFLINE`]
/// event; an interior transport has no degraded-but-serving state of its own,
/// so [`BlkHealthTransition::Degraded`] cannot occur. Edge-triggered: an
/// unchanged state logs nothing.
fn note_domain_edge(before: FaultDomainState, after: FaultDomainState, owner: u32) {
    if before == after {
        return;
    }
    if let Some(transition) = BlkHealthTransition::for_fault_domain(before, after) {
        let (id, level, message) = match transition {
            BlkHealthTransition::Recovering => (
                MSD_DOMAIN_RECOVERING,
                Level::Warn,
                "usb-msd: shared transport reset, whole device held recovering under one window",
            ),
            BlkHealthTransition::Recovered => (
                MSD_DOMAIN_RECOVERED,
                Level::Info,
                "usb-msd: shared transport returned, device recovered",
            ),
            // An interior transport never reports itself degraded-but-serving.
            BlkHealthTransition::Degraded => return,
        };
        log_hex_event(id, level, message, "owner_hex", u64::from(owner));
    } else if matches!(after, FaultDomainState::Offline) {
        log_hex_event(
            MSD_DOMAIN_OFFLINE,
            Level::Warn,
            "usb-msd: shared transport recovery window elapsed, device failed closed",
            "owner_hex",
            u64::from(owner),
        );
    }
}

/// Bounded read buffer for the best-effort ancestor-health snapshot read.
///
/// This is **not** a capacity limit on anything: it sizes the stack buffer the
/// recovery-path tree read uses. A discovered tree that does not fit simply
/// leaves ancestor attribution unavailable for that read — the leaf then
/// answers on its own device health, which is always correct — so the read
/// never grows an unbounded buffer on a driver's recovery path and never
/// fails a request. Ample for the trees TAIRiX's Tier-1 targets discover.
const TREE_SNAPSHOT_BUF: usize = 8192;

/// The [`BlkStatus`] this driver's interior fault-domain ancestors currently
/// impose on a LUN request, read from the live hardware-tree snapshot.
///
/// Called by [`serve_lun_with_domain`] **only on the recovery path** (a stall
/// the device did not answer definitively), so a healthy transfer never reads
/// the tree. It reads the current snapshot into a bounded stack buffer and
/// folds the published health of this driver's ancestor chain
/// ([`ancestor_imposed_status_from_snapshot`]), so a resetting controller/hub
/// is attributed to the fault domain rather than to the disk.
///
/// It fails **safe**: no matched node ([`self_node`] is `None`), a refused
/// read, or a snapshot larger than the buffer all yield [`BlkStatus::Ok`] (no
/// ancestor imposes anything), so the leaf simply answers on its own health.
fn ancestor_status(self_node: Option<u32>) -> BlkStatus {
    let Some(node) = self_node else {
        return BlkStatus::Ok;
    };
    let mut buf = [0u8; TREE_SNAPSHOT_BUF];
    match tairix_rt::hw_tree_read(&mut buf) {
        Ok(len) => ancestor_imposed_status_from_snapshot(&buf[..len], node),
        Err(_) => BlkStatus::Ok,
    }
}

/// Map the two transport grants the matched interface node carried: the URB
/// call endpoint's id and the shared bulk data buffer.
///
/// `Err` is the exit code the entry point returns. A buffer too small for one
/// bulk chunk, or a base this pointer width cannot hold, is a mis-provisioned
/// node refused here, before any slice is built over it.
fn map_urb_transport() -> Result<(u64, &'static mut [u8]), i32> {
    // No MMIO/DMA grants to map, so no coherency shim is needed.
    let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
        return Err(EXIT_NO_HOST);
    };
    let Some(endpoint) = host.endpoint_grant() else {
        return Err(EXIT_NO_TRANSPORT);
    };
    // The kernel reports the mapped region's true length.
    let Ok((shm_base, shm_len)) = host.map_shared() else {
        return Err(EXIT_NO_TRANSPORT);
    };
    let Ok(base) = usize::try_from(shm_base) else {
        return Err(EXIT_NO_TRANSPORT);
    };
    if shm_len < BULK_BUF_LEN {
        return Err(EXIT_NO_TRANSPORT);
    }
    // SAFETY: `map_shared` mapped the HCD-created shared URB data buffer
    // into this process at `shm_base`, and the kernel-reported length was
    // verified above to hold at least `BULK_BUF_LEN` bytes (one bulk
    // chunk — the one length both sides build from).
    // The mapping lives for the rest of this process and nothing else in
    // this address space aliases it, so a single exclusive `&mut [u8]`
    // over the buffer is sound. The HCD writes it only while serving this
    // driver's own blocking URB calls.
    let shm = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, BULK_BUF_LEN) };
    Ok((endpoint, shm))
}

/// Learn the interface number and transport endpoints from the device's own
/// configuration descriptor (never assumed): header first for the total
/// length, then the full stream, parsed in place from the shared buffer the
/// control-IN landed it in.
///
/// `Err` is the exit code the entry point returns.
fn discover_storage_interface(
    client: &mut UrbClient<IpcUrbCall>,
    shm: &[u8],
) -> Result<StorageInterface, i32> {
    // `wLength` is 16-bit on the wire; refuse rather than truncate.
    let Ok(header_len) = u16::try_from(CONFIGURATION_HEADER_LEN) else {
        return Err(EXIT_BRINGUP_FAILED);
    };
    let Ok(n) = client.control_in(get_configuration_setup(header_len), 0, header_len.into()) else {
        return Err(EXIT_BRINGUP_FAILED);
    };
    let Ok(total) = configuration_total_length(&shm[..(n as usize).min(shm.len())]) else {
        return Err(EXIT_BRINGUP_FAILED);
    };
    // A configuration stream larger than the shared buffer cannot be
    // fetched over this transport; refuse the device rather than parse a
    // truncated stream.
    let Ok(total_u16) = u16::try_from(total) else {
        return Err(EXIT_BRINGUP_FAILED);
    };
    if total > shm.len() {
        return Err(EXIT_BRINGUP_FAILED);
    }
    let Ok(n) = client.control_in(get_configuration_setup(total_u16), 0, total_u16.into()) else {
        return Err(EXIT_BRINGUP_FAILED);
    };
    if (n as usize) < total {
        return Err(EXIT_BRINGUP_FAILED);
    }
    find_storage_interface(&shm[..total]).map_err(|_| EXIT_BRINGUP_FAILED)
}

/// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
/// is set up and routes its return value through the `exit` syscall.
///
/// On success this never returns: the block-service loop runs for the
/// life of the device, and a detach exits `0` so `devmgr` reloads the
/// driver cleanly on re-plug.
fn main() -> i32 {
    let (endpoint, shm) = match map_urb_transport() {
        Ok(transport) => transport,
        Err(code) => return code,
    };
    let mut client = UrbClient::new(IpcUrbCall {
        endpoint,
        disconnected: false,
    });
    let interface = match discover_storage_interface(&mut client, shm) {
        Ok(interface) => interface,
        Err(code) => return code,
    };

    // Build the wire transport the interface's protocol byte named and run
    // the one shared bring-up + serve body over it.
    let link = UrbLink { client, shm };
    match interface.protocol {
        StorageProtocol::Bot { bulk_in, bulk_out } => {
            log_hex_event(
                MSD_SETUP,
                Level::Info,
                "usb-msd: BOT interface + bulk endpoint pair derived from descriptors",
                "in_out_hex",
                (u64::from(bulk_in) << 8) | u64::from(bulk_out),
            );
            let transport = UrbTransport {
                link,
                bulk_in_endpoint: bulk_in,
                bulk_out_endpoint: bulk_out,
                interrupt_endpoint: 0,
            };
            let scsi = ScsiDevice::new(
                Bot::new(transport, interface.interface_number),
                interface.command_set,
            );
            run_device(scsi, endpoint, |scsi| {
                scsi.transport().transport().link.disconnected()
            })
        }
        StorageProtocol::Cbi {
            bulk_in,
            bulk_out,
            interrupt_in,
        } => {
            log_hex_event(
                MSD_SETUP,
                Level::Info,
                "usb-msd: CBI interface endpoints derived from descriptors",
                "in_out_int_hex",
                (u64::from(bulk_in) << 16) | (u64::from(bulk_out) << 8) | u64::from(interrupt_in),
            );
            let transport = UrbTransport {
                link,
                bulk_in_endpoint: bulk_in,
                bulk_out_endpoint: bulk_out,
                interrupt_endpoint: interrupt_in,
            };
            // The accepted CBI command set is UFI (the floppy set), whose
            // completion block carries ASC/ASCQ.
            let status = match interface.command_set {
                CommandSet::Ufi => CbiStatus::UfiSense,
                CommandSet::Transparent => CbiStatus::CommandStatus,
            };
            let scsi = ScsiDevice::new(
                Cbi::new(transport, interface.interface_number, status),
                interface.command_set,
            );
            run_device(scsi, endpoint, |scsi| {
                scsi.transport().transport().link.disconnected()
            })
        }
        StorageProtocol::Uas(endpoints) => {
            log_hex_event(
                MSD_SETUP,
                Level::Info,
                "usb-msd: UAS pipes derived from descriptors",
                "cmd_sts_din_dout_hex",
                (u64::from(endpoints.command) << 24)
                    | (u64::from(endpoints.status) << 16)
                    | (u64::from(endpoints.data_in) << 8)
                    | u64::from(endpoints.data_out),
            );
            let pipes = UasUrbPipes { link, endpoints };
            let scsi = ScsiDevice::new(Uas::new(pipes), interface.command_set);
            run_device(scsi, endpoint, |scsi| {
                scsi.transport().pipes().link.disconnected()
            })
        }
    }
}

/// Bring every unit of the brought-up device online and publish one storage
/// node per ready LUN.
///
/// `Err` is the exit code the caller returns: no unit could be brought up, or
/// a service resource a ready unit needs was refused.
fn publish_luns<T: ScsiTransport>(
    scsi: &mut ScsiDevice<T>,
    urb_endpoint: u64,
) -> Result<[Option<LunServe>; MAX_LUNS], i32> {
    let Ok(lun_count) = scsi.lun_count() else {
        return Err(EXIT_BRINGUP_FAILED);
    };
    let Some(blk_block) = blk_block_for(urb_endpoint) else {
        return Err(EXIT_NO_SERVICE);
    };
    let mut luns: [Option<LunServe>; MAX_LUNS] = core::array::from_fn(|_| None);
    let mut published = 0usize;
    for lun in 0..lun_count {
        let state = match bring_up_lun(scsi, lun) {
            Ok(Some(state)) => state,
            Ok(None) => continue,
            Err(err) => {
                log_hex_event(
                    MSD_ERROR,
                    Level::Warn,
                    "usb-msd: LUN bring-up failed",
                    "errno_hex",
                    err as u64,
                );
                continue;
            }
        };
        let Some(blk_endpoint) = bind_blk_endpoint(blk_block, lun) else {
            return Err(EXIT_NO_SERVICE);
        };
        let Some((window, shm_id)) = create_window() else {
            return Err(EXIT_NO_SERVICE);
        };
        let Some(node_id) = emit_lun_node(blk_endpoint, shm_id) else {
            return Err(EXIT_NO_SERVICE);
        };
        log_hex_event(
            MSD_LUN_READY,
            Level::Info,
            "usb-msd: LUN published as a storage node",
            "blocks_hex",
            state.geometry.block_count,
        );
        luns[usize::from(lun)] = Some(LunServe {
            endpoint: blk_endpoint,
            node_id,
            state,
            window,
            health: BlkHealth::new(BlkDeviceClass::Removable),
            ladder: RecoveryLadder::new(BlkDeviceClass::Removable),
        });
        published += 1;
    }
    if published == 0 {
        log_hex_event(
            MSD_ERROR,
            Level::Error,
            "usb-msd: no logical unit could be brought up",
            "count_hex",
            u64::from(lun_count),
        );
        return Err(EXIT_BRINGUP_FAILED);
    }
    Ok(luns)
}

/// The serve wait-set: one member per published LUN endpoint, token = LUN
/// number. [`None`] means the device cannot be served.
fn join_serve_set(luns: &[Option<LunServe>]) -> Option<u64> {
    // A negative return is the errno; anything else is the wait-set handle.
    let set = u64::try_from(tairix_rt::waitset_create()).ok()?;
    for (lun, serve) in luns.iter().enumerate() {
        let Some(serve) = serve else { continue };
        let ret = tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            serve.endpoint,
            lun as u64,
        );
        if ret != 0 {
            return None;
        }
    }
    Some(set)
}

/// Bring every unit of the brought-up device online and serve block
/// requests for the life of the device: the one bring-up + serve body
/// every wire transport runs.
///
/// `disconnected` observes the transport's vanished-endpoint state (the
/// HCD retracted the interface — the device was unplugged), so the serve
/// loop can retract the LUN nodes and exit `0` for a clean reload on
/// re-plug.
fn run_device<T, F>(mut scsi: ScsiDevice<T>, urb_endpoint: u64, disconnected: F) -> i32
where
    T: ScsiTransport,
    F: Fn(&ScsiDevice<T>) -> bool,
{
    let luns = match publish_luns(&mut scsi, urb_endpoint) {
        Ok(luns) => luns,
        Err(code) => return code,
    };
    let Some(set) = join_serve_set(&luns) else {
        retract_all(&luns);
        return EXIT_NO_SERVICE;
    };
    log(
        &LogSink,
        &Event {
            level: Level::Info,
            id: MSD_READY,
            message: "usb-msd: logical units published, serving block requests",
            fields: &[],
        },
    );
    serve_requests(scsi, luns, set, urb_endpoint, disconnected)
}

/// Serve the published LUNs until the device detaches (exit `0`) or the
/// service path fails closed.
fn serve_requests<T, F>(
    mut scsi: ScsiDevice<T>,
    mut luns: [Option<LunServe>; MAX_LUNS],
    set: u64,
    urb_endpoint: u64,
    disconnected: F,
) -> i32
where
    T: ScsiTransport,
    F: Fn(&ScsiDevice<T>) -> bool,
{
    // The shared BOT transport is the fault domain of this device's LUNs: a
    // transport-wide reset (the recovery ladder's data-path scrub, a port
    // reset, a bus blip) hits every unit at once, so it is one recovery
    // episode across the whole device, not N independent LUN failures. The
    // owner id is this device's own URB transport grant (runtime-discovered,
    // never a board constant); the shared grace window is the removable
    // class's — the same the LUNs ride their own blips out under.
    let domain_owner = u32::try_from(urb_endpoint & 0xFFFF_FFFF).unwrap_or(u32::MAX);
    let mut domain = FaultDomain::new(domain_owner, BlkDeviceClass::Removable.budget().grace_ns);

    // This driver's own place in the discovered hardware tree, learned once so
    // it can attribute a stall to an interior ancestor (the USB controller, a
    // hub) that is resetting rather than to the disk (`plans/FIX-IO.md` IO4).
    // `None` (no matched node) simply disables ancestor attribution — the leaf
    // then answers on its own health, which is the pre-IO4 behaviour.
    let self_node = {
        let ret = tairix_rt::hw_self_node();
        if ret < 0 {
            None
        } else {
            u32::try_from(ret).ok()
        }
    };

    // Event-driven service loop: park on the wait-set until a consumer's
    // request arrives, serve it (the data moves via blocking URB calls
    // that park in the kernel), reply, and check for detach. Never a
    // busy-poll.
    loop {
        let arm_now_ns = tairix_rt::clock_get();
        // Park until the soonest of any per-LUN grace window and the shared
        // transport's fault-domain window comes due (each computed by one
        // shared rule), so a quiet recovering unit *and* a quiet recovering
        // transport both fail closed on time off a one-shot, never a spin;
        // with nothing recovering the loop parks with no timeout.
        let device_deadline =
            recovery_wait_timeout(luns.iter().flatten().map(|s| &s.health), arm_now_ns);
        let domain_deadline = fault_domain_wait_timeout(core::iter::once(&domain), arm_now_ns);
        let timeout = match (device_deadline, domain_deadline) {
            (Some(a), Some(b)) => a.min(b),
            (Some(t), None) | (None, Some(t)) => t,
            (None, None) => WAIT_FOREVER_NS,
        };
        let mut token = 0u64;
        let ret = tairix_rt::waitset_wait(set, timeout, &mut token);
        // One fresh monotonic reading taken after waking, reused to fold the
        // grace windows and (on a request) to time the served unit; the
        // pre-park reading above could be arbitrarily stale after the park.
        let now_ns = tairix_rt::clock_get();
        if ret < 0 {
            // A timed-out wait is the grace one-shot firing, not a failure:
            // fold the elapsed windows and re-arm. Any other error is fatal.
            let errno =
                Errno::from_i32(i32::try_from(-ret).unwrap_or(0)).unwrap_or(Errno::NotFound);
            if errno == Errno::TimedOut {
                expire_idle_grace_windows(&mut luns, now_ns);
                let domain_before = domain.state();
                note_domain_edge(domain_before, domain.poll(now_ns), domain_owner);
                continue;
            }
            retract_all(&luns);
            return EXIT_NO_SERVICE;
        }
        // A member is ready: fold any grace window that came due while parked
        // on this wake before serving, so a sibling LUN's window is honoured
        // on time even under a steady request stream to another unit.
        expire_idle_grace_windows(&mut luns, now_ns);
        // Advance the shared-transport window on this wake too, so a quiesced
        // transport still fails closed on time even under a steady request
        // stream to a healthy unit.
        let domain_poll_before = domain.state();
        note_domain_edge(domain_poll_before, domain.poll(now_ns), domain_owner);
        // The token a member was added under is its LUN number.
        let Ok(lun) = u8::try_from(token) else {
            continue;
        };
        let Some(serve) = luns.get_mut(usize::from(lun)).and_then(Option::as_mut) else {
            continue;
        };
        serve_ready_lun(
            &mut scsi,
            serve,
            lun,
            &mut domain,
            domain_owner,
            self_node,
            now_ns,
        );
        // A vanished URB endpoint means the HCD retracted the interface:
        // the device is gone. Retract the LUN nodes and exit cleanly so a
        // re-plug re-enumerates and reloads this driver.
        if disconnected(&scsi) {
            log(
                &LogSink,
                &Event {
                    level: Level::Info,
                    id: MSD_DETACHED,
                    message: "usb-msd: device detached, retracting LUN nodes and exiting",
                    fields: &[],
                },
            );
            retract_all(&luns);
            return 0;
        }
    }
}

/// Serve the one queued request a ready LUN woke the set for: receive it, run
/// the transfer under the unit's health and the device-wide fault domain,
/// reply, and drive the bounded recovery-escalation ladder from the outcome.
///
/// A wake whose queued call its poster already cancelled simply returns.
fn serve_ready_lun<T: ScsiTransport>(
    scsi: &mut ScsiDevice<T>,
    serve: &mut LunServe,
    lun: u8,
    domain: &mut FaultDomain,
    domain_owner: u32,
    self_node: Option<u32>,
    now_ns: u64,
) {
    let mut request = [0u8; BLK_REQUEST_LEN];
    let mut ticket = 0u64;
    // Non-blocking: this wait-set serves every LUN's endpoint, and the
    // queued call the wake reported may have been cancelled by its
    // poster's exit — parking here would starve the other LUNs.
    let Ok(n) = tairix_rt::call_recv_nonblock(serve.endpoint, &mut request, &mut ticket) else {
        return;
    };
    let read_only = serve.state.write_protected;
    let mut reply = [0u8; BLK_COMPLETION_LEN];
    // Snapshot the unit's health before serving so the outcome's effect on
    // it is one auditable edge (Degraded/Recovering/Recovered, or a
    // fail-closed) rather than silent.
    let before_health = serve.health.state();
    // Snapshot the shared-transport domain too: serving folds the unit's
    // own outcome with what the transport imposes and may recover the
    // whole device when a unit demonstrates the transport is back, so that
    // is one auditable device-wide edge as well.
    let domain_before = domain.state();
    let len = {
        let mut block = LunBlock::new(scsi, lun, serve.state);
        let mut recovery = LunRecovery {
            health: &mut serve.health,
            domain: &mut *domain,
        };
        serve_lun_with_domain(
            &mut block,
            read_only,
            ServeBuffers {
                request: &request[..n],
                window: serve.window,
                reply: &mut reply,
            },
            &mut recovery,
            now_ns,
            // Consulted only on the recovery path (a stall the device did
            // not answer definitively), so a healthy transfer never reads
            // the tree: report what this LUN's interior fault-domain
            // ancestors currently impose.
            || ancestor_status(self_node),
        )
    };
    let _ = tairix_rt::call_reply(serve.endpoint, ticket, &reply[..len]);
    note_health_edge(before_health, serve.health.state(), serve.node_id);
    note_domain_edge(domain_before, domain.state(), domain_owner);
    // Drive the bounded recovery-escalation ladder from the unit's health.
    // A unit that just answered normally re-arms the ladder; a `Recovering`
    // unit escalates — a first gentle retry, then a data-path reset (this
    // driver's one recovery mechanism: clear the bulk pipes) — to try to
    // bring it back before its grace window is left to fail it closed. The
    // ladder bounds how often the reset is taken, so a wedged unit is never
    // reset forever, and the reset is only ever issued for a unit already
    // being answered reissuably, so head-of-line freedom holds.
    let health_state = serve.health.state();
    if serve.ladder.next_action(health_state) == RecoveryAction::Reset {
        // The scrub clears the *shared* bulk pipes, so it is a
        // transport-wide event: open (or continue) the one shared recovery
        // window over the whole device before touching the transport, so a
        // sibling LUN's next request is held reissuable under it rather
        // than surfacing as an independent failure.
        let domain_before = domain.state();
        domain.quiesce(now_ns);
        note_domain_edge(domain_before, domain.state(), domain_owner);
        scsi.scrub_window();
        log_hex_event(
            MSD_RECOVERY_RESET,
            Level::Warn,
            "usb-msd: escalating a data-path reset to recover a stalling LUN",
            "lun_hex",
            u64::from(lun),
        );
    }
}

tairix_rt::entry!(main);
