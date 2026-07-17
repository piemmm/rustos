//! The freestanding body of the mass-storage `Run` binary (`main.rs`):
//! URB-backed transport, LUN bring-up, per-LUN block-service publication,
//! and the wait-set serve loop (`plans/DEVICES.md` D2).

use tairix_abi::blkio::{BLK_COMPLETION_LEN, BLK_DATA_LEN, BLK_REQUEST_LEN};
use tairix_abi::hwtree::HW_NODE_ROOT;
use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
use tairix_abi::{CapabilityId, Errno, HwDeviceClass, HwMatchKey, HwNode, HwResource};
use tairix_caps::CapabilitySet;
use tairix_drv_storage_usb_msd::bot::{Bot, MsdTransport};
use tairix_drv_storage_usb_msd::cbi::{Cbi, CbiStatus};
use tairix_drv_storage_usb_msd::desc::{
    configuration_total_length, find_storage_interface, StorageProtocol, UasEndpoints,
    CONFIGURATION_HEADER_LEN,
};
use tairix_drv_storage_usb_msd::scsi::{
    CommandSet, LunBlock, LunState, ScsiDevice, ScsiTransport, DEVICE_TYPE_DIRECT_ACCESS, MAX_LUNS,
};
use tairix_drv_storage_usb_msd::serve::{blk_block_for, serve_request};
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
/// emitted node, and brought-up state.
struct LunServe {
    endpoint: u64,
    node_id: u32,
    state: LunState,
    window: &'static mut [u8],
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
    let base = tairix_rt::shm_create(BLK_DATA_LEN, &mut shm_id);
    if base < 0 {
        return None;
    }
    // SAFETY: `shm_create` mapped `BLK_DATA_LEN` bytes of zeroed,
    // cacheable, RW (non-executable) memory into this process at `base`
    // and returned that base. The region is owned by this process for the
    // rest of its life (never unmapped here), and no other reference in
    // this address space aliases it, so a single exclusive `&mut [u8]`
    // over exactly the requested length is sound. The consumer maps the
    // same frames through its own inherited grant.
    let window = unsafe { core::slice::from_raw_parts_mut(base as usize as *mut u8, BLK_DATA_LEN) };
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
    let emit = tairix_rt::hw_emit_node(&node);
    if emit < 0 {
        return None;
    }
    #[allow(clippy::cast_sign_loss)] // `emit >= 0` is the assigned node id.
    Some(emit as u32)
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
        let _ = tairix_rt::hw_remove_node(lun.node_id);
    }
}

/// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
/// is set up and routes its return value through the `exit` syscall.
///
/// On success this never returns: the block-service loop runs for the
/// life of the device, and a detach exits `0` so `devmgr` reloads the
/// driver cleanly on re-plug.
fn main() -> i32 {
    // No MMIO/DMA grants to map, so no coherency shim is needed.
    let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
        return EXIT_NO_HOST;
    };
    // The matched interface node carried two transport grants: the URB
    // call endpoint (its id) and the shared data buffer (mapped here).
    let Some(endpoint) = host.endpoint_grant() else {
        return EXIT_NO_TRANSPORT;
    };
    // The kernel reports the mapped region's true length; a buffer too
    // small for one bulk chunk is a mis-provisioned node refused here,
    // before any slice is built over it.
    let Ok((shm_base, shm_len)) = host.map_shared() else {
        return EXIT_NO_TRANSPORT;
    };
    if shm_len < BULK_BUF_LEN {
        return EXIT_NO_TRANSPORT;
    }
    // SAFETY: `map_shared` mapped the HCD-created shared URB data buffer
    // into this process at `shm_base`, and the kernel-reported length was
    // verified above to hold at least `BULK_BUF_LEN` bytes (one bulk
    // chunk — the one length both sides build from).
    // The mapping lives for the rest of this process and nothing else in
    // this address space aliases it, so a single exclusive `&mut [u8]`
    // over the buffer is sound. The HCD writes it only while serving this
    // driver's own blocking URB calls.
    let shm =
        unsafe { core::slice::from_raw_parts_mut(shm_base as usize as *mut u8, BULK_BUF_LEN) };

    // Learn the interface number and bulk endpoint pair from the device's
    // own configuration descriptor (never assumed): header first for the
    // total length, then the full stream, parsed in place from the shared
    // buffer the control-IN landed it in.
    let mut client = UrbClient::new(IpcUrbCall {
        endpoint,
        disconnected: false,
    });
    let header_len = CONFIGURATION_HEADER_LEN as u32;
    let Ok(n) = client.control_in(get_configuration_setup(header_len as u16), 0, header_len) else {
        return EXIT_BRINGUP_FAILED;
    };
    let Ok(total) = configuration_total_length(&shm[..(n as usize).min(shm.len())]) else {
        return EXIT_BRINGUP_FAILED;
    };
    // A configuration stream larger than the shared buffer cannot be
    // fetched over this transport; refuse the device rather than parse a
    // truncated stream.
    let Ok(total_u16) = u16::try_from(total) else {
        return EXIT_BRINGUP_FAILED;
    };
    if total > shm.len() {
        return EXIT_BRINGUP_FAILED;
    }
    let Ok(n) = client.control_in(get_configuration_setup(total_u16), 0, total_u16.into()) else {
        return EXIT_BRINGUP_FAILED;
    };
    if (n as usize) < total {
        return EXIT_BRINGUP_FAILED;
    }
    let Ok(interface) = find_storage_interface(&shm[..total]) else {
        return EXIT_BRINGUP_FAILED;
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
    // Bring every unit up; publish one storage node per ready LUN.
    let Ok(lun_count) = scsi.lun_count() else {
        return EXIT_BRINGUP_FAILED;
    };
    let Some(blk_block) = blk_block_for(urb_endpoint) else {
        return EXIT_NO_SERVICE;
    };
    let mut luns: [Option<LunServe>; MAX_LUNS] = core::array::from_fn(|_| None);
    let mut published = 0usize;
    for lun in 0..lun_count {
        let state = match bring_up_lun(&mut scsi, lun) {
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
            return EXIT_NO_SERVICE;
        };
        let Some((window, shm_id)) = create_window() else {
            return EXIT_NO_SERVICE;
        };
        let Some(node_id) = emit_lun_node(blk_endpoint, shm_id) else {
            return EXIT_NO_SERVICE;
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
        return EXIT_BRINGUP_FAILED;
    }

    // The serve wait-set: one member per published LUN endpoint, token =
    // LUN number.
    let set = tairix_rt::waitset_create();
    if set < 0 {
        retract_all(&luns);
        return EXIT_NO_SERVICE;
    }
    #[allow(clippy::cast_sign_loss)] // `set >= 0` is the wait-set handle.
    let set = set as u64;
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
            retract_all(&luns);
            return EXIT_NO_SERVICE;
        }
    }

    log(
        &LogSink,
        &Event {
            level: Level::Info,
            id: MSD_READY,
            message: "usb-msd: logical units published, serving block requests",
            fields: &[],
        },
    );

    // Event-driven service loop: park on the wait-set until a consumer's
    // request arrives, serve it (the data moves via blocking URB calls
    // that park in the kernel), reply, and check for detach. Never a
    // busy-poll.
    loop {
        let mut token = 0u64;
        let ret = tairix_rt::waitset_wait(set, WAIT_FOREVER_NS, &mut token);
        if ret < 0 {
            retract_all(&luns);
            return EXIT_NO_SERVICE;
        }
        let index = usize::try_from(token).unwrap_or(MAX_LUNS);
        let Some(serve) = luns.get_mut(index).and_then(Option::as_mut) else {
            continue;
        };
        let mut request = [0u8; BLK_REQUEST_LEN];
        let mut ticket = 0u64;
        // Non-blocking: this wait-set serves every LUN's endpoint, and the
        // queued call the wake reported may have been cancelled by its
        // poster's exit — parking here would starve the other LUNs.
        let Ok(n) = tairix_rt::call_recv_nonblock(serve.endpoint, &mut request, &mut ticket) else {
            continue;
        };
        let lun = index as u8;
        let read_only = serve.state.write_protected;
        let mut reply = [0u8; BLK_COMPLETION_LEN];
        let len = {
            let mut block = LunBlock::new(&mut scsi, lun, serve.state);
            serve_request(
                &mut block,
                read_only,
                &request[..n],
                serve.window,
                &mut reply,
            )
        };
        let _ = tairix_rt::call_reply(serve.endpoint, ticket, &reply[..len]);
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

tairix_rt::entry!(main);
