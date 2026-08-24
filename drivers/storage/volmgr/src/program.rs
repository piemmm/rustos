//! The freestanding body of the volume-manager `Run` binary (`main.rs`):
//! grant resolution, the blkio probe, and the attach loop
//! (`plans/DEVICES.md` D3c).
//!
//! # Probe first, then hand the device over
//!
//! The probe and the kernel's mounts drive the *same* served device over the
//! *same* single shared data window, which the blkio protocol stages each
//! transfer's bytes in. Two users of that window at once would overwrite each
//! other's staged bytes and read back another extent's data — silent
//! corruption, not a fault. So the two phases are strictly ordered here: the
//! whole device is probed by [`probe_device`], which owns the transport for
//! its whole life, and only then is the first volume attached. The window
//! borrow therefore ends with that call, so the compiler, not a convention,
//! is what stops a later probe read from racing the kernel
//! (`plans/FIX-IO.md`).

extern crate alloc;

use alloc::vec::Vec;

use tairix_abi::blkio::BLK_DATA_LEN;
use tairix_abi::hwtree::{HwResource, HwResourceKind, HW_NODE_ROOT};
use tairix_abi::raid_ipc::{RAID_CANDIDATE_COMPATIBLE, RAID_MEMBER_COMPATIBLE};
use tairix_abi::volume::{VolumeAttachRequest, VOLUME_ATTACH_MAX_LEN};
use tairix_abi::{CapabilityId, Errno, HwDeviceClass, HwMatchKey, HwNode};
use tairix_blkclient::{RemoteBlock, RtBlkCall};
use tairix_caps::CapabilitySet;
use tairix_drv_storage_volmgr::name::{candidate, CANDIDATE_ATTEMPTS};
use tairix_drv_storage_volmgr::plan::{plan_volumes, PlanSummary, VolumePlan};
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

/// Exit code when the wait-set the block transport parks each reply on could
/// not be minted — without it every wait would have to become a poll.
const EXIT_NO_WAITSET: i32 = 93;

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
/// would diverge a mirror or serve stale data (`plans/FIX-IO.md` IO6). A
/// normal outcome, logged so a member disk is diagnosable rather than looking
/// blank.
const VOLMGR_RAID_MEMBER: EventId = EventId(4185);

/// Diagnostic event id: the RAID member node could not be published, so no
/// agent will offer this device to the array composer and any array it belongs
/// to stays short a member. Not fatal to this device's other volumes.
const VOLMGR_RAID_MEMBER_UNPUBLISHED: EventId = EventId(4190);

/// Diagnostic event id: the whole device carries nothing at all, so it was
/// published as a device a new RAID array could be created over. A normal
/// outcome for a fresh disk, logged so an administrator can see why a blank
/// disk became nameable to the array tools.
const VOLMGR_RAID_CANDIDATE: EventId = EventId(4191);

/// Diagnostic event id: the blank-device node could not be published, so this
/// disk cannot be named when creating an array. Not fatal to anything else.
const VOLMGR_RAID_CANDIDATE_UNPUBLISHED: EventId = EventId(4192);

/// The capability set the driver host re-checks up front; the kernel is
/// the authority and re-checks every trap. It is the least-privilege set
/// this policy driver needs — no MMIO, DMA, or IRQ. Node emission is held
/// only to publish the RAID member node below, and the kernel admits that
/// emission only for resources this task already holds.
fn driver_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::SHM);
    caps.insert(CapabilityId::IPC_ENDPOINT);
    caps.insert(CapabilityId::FS_MOUNT);
    caps.insert(CapabilityId::HW_EMIT);
    caps.insert(CapabilityId::LOG_EMIT);
    caps
}

/// Publish the member node the array composer's agent binds, re-declaring
/// this device's transport under it (`plans/FIX-IO.md` `IO6c`).
///
/// The node says only "this device's first block, or one of its partitions,
/// carries array metadata" — it is a pointer for the composer to look, never a
/// datum for it to believe. Which array, which slot, and which generation are
/// read back off the device itself, so a mistaken or malicious emitter cannot
/// place a disk into an array it has nothing to do with. That is also why one
/// node covers a whole device however many of its partitions are members: the
/// composer re-probes the device through the same shared definition this probe
/// used.
///
/// The kernel assigns the node's identity and parents it to this driver's own
/// matched node, and admits each declared resource only if this task already
/// holds a grant covering it — so the emission can republish this device's
/// transport and nothing else. Best-effort: a refusal leaves the member
/// unassembled and logged, exactly as a device with no driver is left unbound,
/// and never fails the volumes this device did attach.
fn publish_raid_member(endpoint: u64, window_id: u64) {
    publish_raid_node(
        RAID_MEMBER_COMPATIBLE,
        endpoint,
        window_id,
        VOLMGR_RAID_MEMBER_UNPUBLISHED,
        "volmgr: could not publish the RAID member node; array cannot assemble",
    );
}

/// Publish the node that offers this whole device to the array composer as
/// one a new array could be created over.
///
/// Emitted only for a device the probe found *entirely* empty: no partition
/// table, no whole-device filesystem, and no array metadata. A disk with
/// anything recognisable on it is never offered, so creating an array cannot
/// silently consume data — and the composer re-reads the device before it
/// writes anything, so this node is a pointer to look rather than a claim to
/// believe. It republishes only the transport this driver already holds.
fn publish_raid_candidate(endpoint: u64, window_id: u64) {
    publish_raid_node(
        RAID_CANDIDATE_COMPATIBLE,
        endpoint,
        window_id,
        VOLMGR_RAID_CANDIDATE_UNPUBLISHED,
        "volmgr: could not publish the blank-device node; no array can be created here",
    );
}

/// Emit one hardware-tree node bearing `compatible` and re-declaring this
/// device's transport, so the array subsystem can reach it.
///
/// The kernel assigns the node's identity, parents it to this driver's own
/// matched node, and admits each declared resource only if this task already
/// holds a grant covering it — so an emission can republish this device's
/// transport and nothing else. Best-effort: a refusal is logged and never
/// fails the volumes this device did attach.
fn publish_raid_node(
    compatible: &[u8],
    endpoint: u64,
    window_id: u64,
    refused: EventId,
    refusal: &'static str,
) {
    let mut node = HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Storage);
    let published = HwMatchKey::compatible(compatible)
        .and_then(|key| node.push_match_key(key))
        .and_then(|()| node.push_resource(HwResource::endpoint(endpoint)))
        .and_then(|()| node.push_resource(HwResource::shared(window_id)))
        .map_or_else(
            |err| -i64::from(err.as_i32()),
            |()| tairix_rt::hw_emit_node(&node),
        );
    if published < 0 {
        log_hex_event(
            refused,
            Level::Warn,
            refusal,
            "errno_hex",
            tairix_rt::errno_from_raw(published) as u64,
        );
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

/// Probe the whole served device, returning its plans and the pass summary,
/// or the exit code to fail closed with.
///
/// The blkio client — and with it the exclusive borrow of the shared data
/// window — is created and consumed entirely inside this call, so the
/// compiler, not a convention, is what stops a probe read from following the
/// caller's first attach (see the module docs).
fn probe_device<S: tairix_drvrt::GrantSyscalls>(
    host: &RtDriverHost<S>,
    endpoint: u64,
) -> Result<(Vec<VolumePlan>, PlanSummary), i32> {
    // The kernel reports the mapped window's true base and length; a window
    // too small for the blkio data protocol, or a base this target cannot
    // address, is a mis-provisioned node refused before any slice is built
    // over it.
    let Ok((window_base, window_len)) = host.map_shared() else {
        return Err(EXIT_NO_TRANSPORT);
    };
    let Ok(base) = usize::try_from(window_base) else {
        return Err(EXIT_NO_TRANSPORT);
    };
    if window_len < BLK_DATA_LEN {
        return Err(EXIT_NO_TRANSPORT);
    }
    // SAFETY: `map_shared` mapped the serving driver's shared data window
    // into this process at `window_base`, and the kernel-reported length was
    // verified above to hold at least `BLK_DATA_LEN` bytes (the one length
    // both sides build from). The mapping lives for the rest of this process
    // and nothing else in this address space aliases it — this is the
    // program's only window slice, built here once — so the exclusive
    // `&mut [u8]` is sound. The serving driver writes it only while serving
    // this process's own blocking blkio calls.
    let window = unsafe { core::slice::from_raw_parts_mut(base as *mut u8, BLK_DATA_LEN) };

    // The one wait-set this program's block transport parks its replies on.
    // The kernel reclaims it when this process exits.
    let Ok(waitset) = u64::try_from(tairix_rt::waitset_create()) else {
        return Err(EXIT_NO_WAITSET);
    };
    let mut client = match RemoteBlock::connect_read_only(RtBlkCall::new(endpoint, waitset), window)
    {
        Ok(client) => client,
        Err(err) => {
            log_hex_event(
                VOLMGR_DEVICE_FAILED,
                Level::Error,
                "volmgr: block service unusable, exiting",
                "errno_hex",
                err as u64,
            );
            return Err(EXIT_DEVICE_FAILED);
        }
    };

    // The sink only records; nothing is attached while this driver is still
    // reading the device through the shared window. The plan count is
    // bounded by the device's own validated partition table, and the list
    // grows to fit it rather than capping at a hand-picked constant.
    let mut plans: Vec<VolumePlan> = Vec::new();
    match plan_volumes(&mut client, |plan| plans.push(*plan)) {
        Ok(summary) => Ok((plans, summary)),
        Err(err) => {
            log_hex_event(
                VOLMGR_DEVICE_FAILED,
                Level::Error,
                "volmgr: device probe failed, exiting",
                "errno_hex",
                err as u64,
            );
            Err(EXIT_DEVICE_FAILED)
        }
    }
}

/// Attach every planned volume, returning how many the kernel published.
///
/// A refusal is logged per volume and is never fatal to the sibling volumes
/// on the same device: only the affected volume fails.
fn attach_all(endpoint: u64, window_id: u64, plans: &[VolumePlan]) -> u64 {
    let mut attached = 0u64;
    for plan in plans {
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
    }
    attached
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
    // call endpoint (its id) and the shared data window (mapped by the probe;
    // its region id rides in every attach request the kernel re-checks
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

    // Phase 1 — probe, then Phase 2 — attach. The order is the module's
    // corruption guarantee: the probe's window borrow is gone before the
    // kernel's mounts get the staging buffer to themselves.
    let (plans, summary) = match probe_device(&host, endpoint) {
        Ok(probed) => probed,
        Err(exit) => return exit,
    };
    let attached = attach_all(endpoint, window_id, &plans);

    if summary.raid_members > 0 {
        log_hex_event(
            VOLMGR_RAID_MEMBER,
            Level::Info,
            "volmgr: RAID array member(s) present; not attaching (awaiting assembly)",
            "raid_members_hex",
            u64::from(summary.raid_members),
        );
        publish_raid_member(endpoint, window_id);
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
            // A device with no scheme at all carries nothing an array would
            // destroy, so it is the one shape that may be offered for
            // composition. A device whose partition table held only extents
            // this build cannot read is *not* blank and is never offered.
            if summary.no_scheme {
                log_hex_event(
                    VOLMGR_RAID_CANDIDATE,
                    Level::Info,
                    "volmgr: device is empty; offering it for array composition",
                    "endpoint_hex",
                    endpoint,
                );
                publish_raid_candidate(endpoint, window_id);
            }
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
