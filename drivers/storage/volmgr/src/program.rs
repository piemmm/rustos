//! The freestanding body of the volume-manager `Run` binary (`main.rs`):
//! grant resolution, the blkio probe, and the attach loop
//! (`plans/DEVICES.md` D3c).

use tairix_abi::blkio::{BlkDeviceClass, BLK_DATA_LEN};
use tairix_abi::hwtree::HwResourceKind;
use tairix_abi::volume::{VolumeAttachRequest, VOLUME_ATTACH_MAX_LEN};
use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
use tairix_abi::{CapabilityId, Errno};
use tairix_caps::CapabilitySet;
use tairix_drv_storage_volmgr::blk::{BlkCall, RemoteBlock};
use tairix_drv_storage_volmgr::name::{candidate, CANDIDATE_ATTEMPTS};
use tairix_drv_storage_volmgr::plan::{plan_volumes, VolumePlan};
use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};
use tairix_log::{log, Event, EventId, Field, FieldValue, Level};
use tairix_rt::LogSink;
use tairix_util::fmt::format_hex_u64;

/// Exit code when the rt-backed driver host could not be built from the
/// kernel-delivered grants. A reserved, fail-closed value.
const EXIT_NO_HOST: i32 = 90;

/// Exit code when the matched storage node did not carry the blkio
/// endpoint and shared-window grants this driver needs.
const EXIT_NO_TRANSPORT: i32 = 91;

/// Exit code when the served device could not be probed (a transport or
/// device fault — never merely an unrecognised layout).
const EXIT_DEVICE_FAILED: i32 = 92;

/// Diagnostic event id: the device was probed; carries the planned and
/// unrecognised counts.
const VOLMGR_PROBED: EventId = EventId(4180);

/// Diagnostic event id: one volume attached and published.
const VOLMGR_ATTACHED: EventId = EventId(4181);

/// Diagnostic event id: one volume's attach was refused (the kernel's
/// audited attach events carry the cause; this records the errno seen
/// from this side).
const VOLMGR_ATTACH_FAILED: EventId = EventId(4182);

/// Diagnostic event id: the device carried nothing attachable (no
/// partition table, no recognised filesystem) — a normal outcome, logged
/// so an unformatted stick is diagnosable.
const VOLMGR_NOTHING_ATTACHABLE: EventId = EventId(4183);

/// Diagnostic event id: the device could not be probed (transport or
/// device fault) and this instance is exiting for supervision.
const VOLMGR_DEVICE_FAILED: EventId = EventId(4184);

/// Diagnostic event id: an extent (the whole device, or a partition) was
/// recognised as a RAID array member and deliberately not attached — it
/// belongs to a RAID array awaiting assembly, and mounting one bare copy
/// would diverge a mirror or serve stale data (`plans/FIX-IO.md` IO6,
/// `AGENTS.md` §26.5). A normal outcome, logged so a member disk is
/// diagnosable rather than looking blank.
const VOLMGR_RAID_MEMBER: EventId = EventId(4185);

/// The capability set the driver host re-checks up front; the kernel is
/// the authority and re-checks every trap. It is the least-privilege set
/// this policy driver needs — no MMIO, DMA, IRQ, or node emission.
fn driver_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::SHM);
    caps.insert(CapabilityId::IPC_ENDPOINT);
    caps.insert(CapabilityId::FS_MOUNT);
    caps.insert(CapabilityId::LOG_EMIT);
    caps
}

/// Recover an [`Errno`] from a raw negative kernel result (`-errno`).
fn errno_from(neg: i64) -> Errno {
    Errno::from_i32(i32::try_from(-neg).unwrap_or(0)).unwrap_or(Errno::NotFound)
}

/// The production blkio transport: the bounded, capability-checked async
/// submit/reap seam on the granted block-service endpoint (`plans/FIX-IO.md`
/// IO1). Each request is `call_post`ed with a per-request deadline, the reply
/// awaited on a `CallReply` wait-set, and reaped with `call_reap`, so a wedged
/// device fails this transfer closed at its deadline instead of parking the
/// probe forever. The serving driver fills the shared window during the call,
/// so the window parameter is untouched here.
struct RtBlkCall {
    endpoint: u64,
    /// The wait-set multiplexing this device's reply completions, created
    /// lazily on first use and `0` until then. One device needs only one
    /// member, but using the wait-set seam even here keeps the single
    /// transport shape the multi-device consumer (IO2) also uses.
    waitset: u64,
    /// Per-request deadline (ns). The probe cannot yet discover the device's
    /// class, so it uses the most generous class budget
    /// ([`BlkDeviceClass::Rotational`] — a spinning disk's spin-up/reset
    /// envelope), bounding a genuinely dead device without prematurely failing
    /// a slow but healthy one.
    deadline_ns: u64,
}

impl RtBlkCall {
    /// A fresh transport for `endpoint` with its wait-set unset (created on
    /// the first [`BlkCall::call`]).
    fn new(endpoint: u64) -> Self {
        Self {
            endpoint,
            waitset: 0,
            deadline_ns: BlkDeviceClass::Rotational.budget().deadline_ns,
        }
    }

    /// Mint the reply wait-set and register this endpoint's `CallReply`
    /// member, once. The wait-set is reclaimed by the kernel when this
    /// run-to-completion program exits, so it needs no explicit teardown.
    fn ensure_waitset(&mut self) -> Result<u64, Errno> {
        if self.waitset == 0 {
            let set = tairix_rt::waitset_create();
            if set < 0 {
                return Err(errno_from(set));
            }
            let set = set as u64;
            let ctl = tairix_rt::waitset_ctl(
                set,
                WaitSetOp::Add,
                WaitSourceKind::CallReply,
                self.endpoint,
                0,
            );
            if ctl < 0 {
                return Err(errno_from(ctl));
            }
            self.waitset = set;
        }
        Ok(self.waitset)
    }
}

impl BlkCall for RtBlkCall {
    fn call(
        &mut self,
        request: &[u8],
        reply: &mut [u8],
        _window: &mut [u8],
    ) -> Result<usize, Errno> {
        let set = self.ensure_waitset()?;
        let ticket =
            tairix_rt::call_post(self.endpoint, request, self.deadline_ns).map_err(errno_from)?;
        loop {
            match tairix_rt::call_reap(self.endpoint, ticket, reply) {
                Ok(len) => return Ok(len),
                Err(neg) => {
                    let err = errno_from(neg);
                    // Not ready yet: park on the reply wait-set until the
                    // reply lands or the per-request deadline elapses (which
                    // makes the member ready and the next reap `TimedOut`),
                    // never a busy poll. Every other outcome — a timeout, a
                    // vanished endpoint — fails closed.
                    if err == Errno::WouldBlock {
                        let mut token = 0u64;
                        let _ = tairix_rt::waitset_wait(set, self.deadline_ns, &mut token);
                        continue;
                    }
                    return Err(err);
                }
            }
        }
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

/// Ask the kernel to attach one planned volume, walking the deterministic
/// candidate-name sequence on a name collision. Any other refusal is
/// final: the kernel audited its cause, and retrying an identical request
/// cannot change the answer.
fn attach_plan(endpoint: u64, window: u64, plan: &VolumePlan) -> Result<(), Errno> {
    let mut last = Errno::AlreadyExists;
    for attempt in 0..CANDIDATE_ATTEMPTS {
        let Some(name) = candidate(&plan.base, &plan.identity, attempt) else {
            break;
        };
        let request = VolumeAttachRequest {
            endpoint,
            window,
            first_lba: plan.first_lba,
            blocks: plan.blocks,
            fstype: plan.fstype,
            name: name.as_bytes(),
        };
        let mut frame = [0u8; VOLUME_ATTACH_MAX_LEN];
        let len = request.encode(&mut frame)?;
        let ret = tairix_rt::volume_attach(&frame[..len]);
        if ret == 0 {
            return Ok(());
        }
        let errno =
            Errno::from_i32(i32::try_from(-ret).unwrap_or(0)).unwrap_or(Errno::NotImplemented);
        if errno != Errno::AlreadyExists {
            return Err(errno);
        }
        last = errno;
    }
    Err(last)
}

/// Program entry point. `tairix-rt`'s `_start` calls it once the runtime
/// is set up and routes its return value through the `exit` syscall.
///
/// Run-to-completion: probe, attach, report, exit `0`. The kernel owns
/// every published mount from attach onward, so nothing here needs to
/// outlive the job; a re-plug re-discovers the node and reloads this
/// driver afresh.
fn main() -> i32 {
    // No MMIO/DMA grants to map, so no coherency shim is needed.
    let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
        return EXIT_NO_HOST;
    };
    // The matched storage node carried two transport grants: the blkio
    // call endpoint (its id) and the shared data window (mapped here; its
    // region id rides in every attach request the kernel re-checks
    // against this task's grants).
    let Some(endpoint) = host.endpoint_grant() else {
        return EXIT_NO_TRANSPORT;
    };
    let Some(window_id) = host
        .resources()
        .find(|resource| resource.kind() == Some(HwResourceKind::Shared))
        .map(tairix_abi::hwtree::HwResource::base)
    else {
        return EXIT_NO_TRANSPORT;
    };
    // The kernel reports the mapped region's true length; a window too
    // small for the blkio data protocol is a mis-provisioned node refused
    // here, before any slice is built over it.
    let Ok((window_base, window_len)) = host.map_shared() else {
        return EXIT_NO_TRANSPORT;
    };
    if window_len < BLK_DATA_LEN {
        return EXIT_NO_TRANSPORT;
    }
    // SAFETY: `map_shared` mapped the serving driver's shared data window
    // into this process at `window_base`, and the kernel-reported length
    // was verified above to hold at least `BLK_DATA_LEN` bytes (the one
    // length both sides build from). The mapping
    // lives for the rest of this process and nothing else in this address
    // space aliases it, so a single exclusive `&mut [u8]` over the buffer
    // is sound. The serving driver writes it only while serving this
    // process's own blocking blkio calls.
    let window =
        unsafe { core::slice::from_raw_parts_mut(window_base as usize as *mut u8, BLK_DATA_LEN) };

    let mut client = match RemoteBlock::connect(RtBlkCall::new(endpoint), window) {
        Ok(client) => client,
        Err(err) => {
            log_hex_event(
                VOLMGR_DEVICE_FAILED,
                Level::Error,
                "volmgr: block service unusable, exiting",
                "errno_hex",
                err as u64,
            );
            return EXIT_DEVICE_FAILED;
        }
    };

    // Probe and attach. The sink runs per recognised volume; attach
    // failures are logged per volume, never fatal to the sibling volumes
    // on the same device (fail only the affected volume).
    let mut attached = 0u64;
    let summary = plan_volumes(&mut client, |plan| {
        match attach_plan(endpoint, window_id, plan) {
            Ok(()) => {
                attached += 1;
                log_hex_event(
                    VOLMGR_ATTACHED,
                    Level::Info,
                    "volmgr: volume attached and published",
                    "first_lba_hex",
                    plan.first_lba,
                );
            }
            Err(err) => {
                log_hex_event(
                    VOLMGR_ATTACH_FAILED,
                    Level::Warn,
                    "volmgr: volume attach refused",
                    "errno_hex",
                    err as u64,
                );
            }
        }
    });
    let summary = match summary {
        Ok(summary) => summary,
        Err(err) => {
            log_hex_event(
                VOLMGR_DEVICE_FAILED,
                Level::Error,
                "volmgr: device probe failed, exiting",
                "errno_hex",
                err as u64,
            );
            return EXIT_DEVICE_FAILED;
        }
    };

    if summary.raid_members > 0 {
        log_hex_event(
            VOLMGR_RAID_MEMBER,
            Level::Info,
            "volmgr: RAID array member(s) present; not attaching (awaiting assembly)",
            "raid_members_hex",
            u64::from(summary.raid_members),
        );
    }
    if summary.planned == 0 {
        // Only genuinely blank when there is also no RAID member: a member is
        // reported above, never as "nothing attachable".
        if summary.raid_members == 0 {
            log_hex_event(
                VOLMGR_NOTHING_ATTACHABLE,
                Level::Info,
                "volmgr: no attachable volume on this device",
                "unrecognised_hex",
                u64::from(summary.unrecognised),
            );
        }
        return 0;
    }
    // A refused sibling volume was logged above; the exit stays `0` so a
    // partially attachable device still serves what it can.
    log_hex_event(
        VOLMGR_PROBED,
        Level::Info,
        "volmgr: device probed and volumes published",
        "attached_hex",
        attached,
    );
    0
}

tairix_rt::entry!(main);
