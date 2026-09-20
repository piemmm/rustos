//! The freestanding driver-process serve loop of the `audiochan-v1` device
//! channel (`plans/SOUND.md` SND4).
//!
//! This is the I/O half of the contract's driver side: everything an audio
//! driver process must do *around* an opened [`Audio`] device to serve the
//! mixer, written once for every such driver rather than copied per device.
//! It claims a reserved device-channel endpoint bound restricted-sender on
//! `CAP_AUDIO_DEVICE`, publishes the [`AUDIOCHAN_NODE_COMPATIBLE`] node so
//! `devmgr` hands the endpoint to `audiod`, and then parks — never
//! busy-polls — on a wait set over two sources:
//!
//! * a **call** wake decodes one request and drives the pure
//!   [`AudioChannelServer`]; `Attach` maps the granted PCM region, `Service`
//!   drives one device doorbell over it, `Detach` unmaps it;
//! * an **interrupt** wake reads the device's causes, moves a period for
//!   every endpoint whose boundary passed, and wakes the mixer with one
//!   notify carrying the clock pair.
//!
//! # Why the interrupt path services rather than merely notifying
//!
//! The period interrupt *means* "the device has consumed a period; refill
//! it". The region is already mapped here, so making the mixer ask for the
//! refill with a blocking call would cost two extra process switches per
//! period on the one path in the system whose whole job is not to have any
//! jitter — and would put the refill deadline behind the mixer's scheduling
//! latency instead of the driver's. So the driver moves the period itself
//! and then reports the clock pair; the mixer keeps its linear fit current
//! and writes ahead in its own time. The ring's atomic counters are what
//! make that safe.
//!
//! Compiled only for the bare-metal targets a driver binary is built for.

use tairix_abi::driver::audio::{Audio, AudioInterrupt, MAX_DEVICE_ENDPOINTS};
use tairix_abi::driver::audio_channel::{
    is_audio_channel_endpoint, AttachParams, AudioChannelNotify, AudioChannelRequest,
    AUDIOCHAN_NODE_COMPATIBLE, AUDIO_CHANNEL_ENDPOINT_BASE, AUDIO_CHANNEL_MAX_REPLY,
    AUDIO_CHANNEL_MAX_REQUEST,
};
use tairix_abi::hwtree::HW_NODE_ROOT;
use tairix_abi::reply::encode_status_reply;
use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
use tairix_abi::{CapabilityId, DriverError, Errno, HwDeviceClass, HwMatchKey, HwNode, HwResource};
use tairix_caps::CapabilitySet;
use tairix_log::{log, Event, EventId, Field, FieldValue, Level};
use tairix_rt::LogSink;

use crate::exit;
use crate::AudioChannelServer;

/// Diagnostic event id: the one-shot "device channel published, serving"
/// beacon an audio driver emits once its device is live and its endpoint is
/// bound.
const AUDIOCHAN_READY: EventId = EventId(4210);

/// Diagnostic event id: an audio driver process giving up, carrying the
/// reason it could not serve its device.
const AUDIOCHAN_FAILED: EventId = EventId(4211);

/// Diagnostic event id: an attach the driver refused because the region the
/// mixer granted is smaller than the geometry the two agreed.
const AUDIOCHAN_SHORT_REGION: EventId = EventId(4212);

/// Record why this driver process is ending abnormally, then return `code`
/// for the runtime to exit with.
///
/// An autoloaded driver is detached, so nothing reads its `stderr`; without
/// this an operator sees only the supervisor's exit code, which names the
/// stage that gave up but never what refused it. `detail` carries the typed
/// refusal where the failure had one.
///
/// One definition for every audio driver process, beside the codes it
/// reports, so two drivers cannot describe the same failure differently.
#[must_use]
pub fn fail(code: i32, reason: &'static str, detail: Option<DriverError>) -> i32 {
    let field = detail.map(|err| Field {
        key: "error",
        value: FieldValue::SignedInt(i64::from(err as i32)),
    });
    log(
        &LogSink,
        &Event {
            level: Level::Error,
            id: AUDIOCHAN_FAILED,
            message: reason,
            fields: field.as_slice(),
        },
    );
    code
}

/// Wait-set token for a device-channel call doorbell on the claimed endpoint.
const CALL_TOKEN: u64 = 1;

/// Wait-set token for "the device interrupt fired".
const IRQ_TOKEN: u64 = 2;

/// Outstanding-call capacity of the device-channel endpoint. The mixer issues
/// one control request at a time (it blocks on the reply); a small queue
/// absorbs a doorbell racing the previous reply — a fail-closed memory bound.
const ENDPOINT_CAPACITY: usize = 4;

/// Wait forever on the serve wait set (a doorbell or an interrupt arrives
/// whenever there is work).
const WAIT_FOREVER_NS: u64 = u64::MAX;

/// Endpoint slots the loop holds a mapping for — the ABI's own endpoint
/// ceiling, so a device cannot present one this loop could not serve.
const ENDPOINT_SLOTS: usize = MAX_DEVICE_ENDPOINTS as usize;

/// One mapping of a shared PCM region the mixer granted in `Attach`.
struct Region {
    /// Base virtual address of the [`shm_map`](tairix_rt::shm_map)ping.
    base: u64,
    /// Full mapped byte length — page-rounded by the kernel, so possibly
    /// larger than the ring geometry — released verbatim by the matching
    /// `shm_unmap`.
    len: usize,
    /// The exclusive ring view: the first `geometry.region_len()` bytes of
    /// the mapping (a subset of `len`), which a `Service` binds the PCM ring
    /// across.
    bytes: &'static mut [u8],
}

/// Serve the `audiochan-v1` device channel over the opened device `audio`
/// for the life of the driver process.
///
/// `irq_handle` is the bound device interrupt (from `irq_bind` on the line
/// the driver's matched node granted) the loop parks on alongside the call
/// endpoint. Never returns on the success path; every set-up refusal returns
/// a reserved [`exit`](crate::exit) code so the driver ends fail-closed with
/// a diagnosable reason rather than degrading into a busy re-poll.
pub fn serve<A: Audio>(audio: A, irq_handle: u64) -> i32 {
    let Some(endpoint) = claim_channel_endpoint() else {
        return fail(
            exit::NO_SERVICE,
            "audiochan: no reserved device-channel endpoint could be claimed and bound",
            None,
        );
    };
    if emit_audiochan_node(endpoint).is_none() {
        return fail(
            exit::NO_SERVICE,
            "audiochan: the device-channel node could not be published to the hardware tree",
            None,
        );
    }

    let set = tairix_rt::waitset_create();
    if set < 0 {
        return fail(
            exit::NO_SERVICE,
            "audiochan: the serve wait set could not be created",
            None,
        );
    }
    #[allow(clippy::cast_sign_loss)] // `set >= 0` is the wait-set handle.
    let set = set as u64;
    if tairix_rt::waitset_ctl(
        set,
        WaitSetOp::Add,
        WaitSourceKind::Endpoint,
        endpoint,
        CALL_TOKEN,
    ) != 0
        || tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Irq,
            irq_handle,
            IRQ_TOKEN,
        ) != 0
    {
        return fail(
            exit::NO_SERVICE,
            "audiochan: the call endpoint and device interrupt could not be joined into the serve wait set",
            None,
        );
    }

    log(
        &LogSink,
        &Event {
            level: Level::Info,
            id: AUDIOCHAN_READY,
            message: "audiochan: device channel published, serving",
            fields: &[],
        },
    );

    let mut server = AudioChannelServer::new(audio);
    // Nothing is attached yet, so a device left clocking by its bring-up has
    // nowhere to put frames; its event sources stay masked until an `Attach`
    // gives this driver a region.
    let _ = server.audio_mut().set_event_interrupts(false);
    serve_loop(server, set, endpoint)
}

/// Claim the first free id in the reserved device-channel endpoint block and
/// bind it **restricted-sender requiring `CAP_AUDIO_DEVICE`**: the kernel
/// admits a caller only if it holds that capability, so only the mixer can
/// post to this driver (defence in depth atop the `CAP_IPC_BIND_PRIVILEGED`
/// gate the reserved-id bind already demands). `recv_caps` is empty —
/// endpoint ownership already restricts receive to this task. Returns the
/// claimed id, or [`None`] if the whole block was already taken (every id
/// squatted on — fail closed).
fn claim_channel_endpoint() -> Option<u64> {
    let mut send_caps = CapabilitySet::empty();
    send_caps.insert(CapabilityId::AUDIO_DEVICE);
    let recv_caps = CapabilitySet::empty();
    let mut id = AUDIO_CHANNEL_ENDPOINT_BASE;
    while is_audio_channel_endpoint(id) {
        let bound = tairix_rt::call_create(
            id,
            &send_caps,
            &recv_caps,
            AUDIO_CHANNEL_MAX_REQUEST,
            AUDIO_CHANNEL_MAX_REPLY,
            ENDPOINT_CAPACITY,
        );
        if bound == 0 {
            return Some(id);
        }
        id += 1;
    }
    None
}

/// Publish the `audiochan` hardware-tree node carrying the claimed
/// device-channel endpoint as a grant request, so `devmgr` observes it (a
/// hardware-tree generation bump) and hands the endpoint to the mixer.
/// Returns the kernel-assigned node id, or [`None`] on any refusal.
///
/// The node names [`HW_NODE_ROOT`] as its parent; the kernel re-parents it
/// under the *discovered node this driver was loaded for*, which is what lets
/// `devmgr` recover the device's stable bus location from the published
/// channel.
fn emit_audiochan_node(endpoint: u64) -> Option<u32> {
    let mut node = HwNode::new(0, HW_NODE_ROOT, HwDeviceClass::Audio);
    let key = HwMatchKey::compatible(AUDIOCHAN_NODE_COMPATIBLE).ok()?;
    node.push_match_key(key).ok()?;
    node.push_resource(HwResource::endpoint(endpoint)).ok()?;
    let emit = tairix_rt::hw_emit_node(&node);
    if emit < 0 {
        return None;
    }
    // `emit >= 0` is the kernel-assigned node id.
    u32::try_from(emit).ok()
}

/// Park on the wait set and serve device-channel doorbells and device
/// interrupts for the life of the driver. Never returns on the success path;
/// a wait-set fault exits fail-closed.
fn serve_loop<A: Audio>(mut server: AudioChannelServer<A>, set: u64, endpoint: u64) -> i32 {
    let mut regions: [Option<Region>; ENDPOINT_SLOTS] = [const { None }; ENDPOINT_SLOTS];
    let mut request = [0u8; AUDIO_CHANNEL_MAX_REQUEST];
    loop {
        let mut token = 0u64;
        let woke = tairix_rt::waitset_wait(set, WAIT_FOREVER_NS, &mut token);
        if woke < 0 {
            return fail(
                exit::NO_SERVICE,
                "audiochan: the serve wait set faulted; the device is no longer being served",
                None,
            );
        }
        if woke != 0 {
            // A spurious/lapsed wake with no ready source; re-park.
            continue;
        }
        match token {
            IRQ_TOKEN => on_interrupt(&mut server, &mut regions),
            CALL_TOKEN => serve_call(&mut server, endpoint, &mut request, &mut regions),
            _ => {}
        }
    }
}

/// Serve one device interrupt: read the causes, move a period for every
/// endpoint whose boundary passed, and wake the mixer once per reported
/// event.
fn on_interrupt<A: Audio>(
    server: &mut AudioChannelServer<A>,
    regions: &mut [Option<Region>; ENDPOINT_SLOTS],
) {
    let Ok(causes) = server.audio_mut().take_interrupt() else {
        // A device whose cause register cannot be read would re-raise the
        // same line forever, so its sources are held down. The mixer's next
        // `Service` carries the fault and is what can release them.
        let _ = server.audio_mut().set_event_interrupts(false);
        return;
    };
    if causes.is_empty() {
        return;
    }
    if !server.any_attached() {
        // Detached: nowhere to put frames and no mixer to wake. The sources
        // stay masked until an `Attach` re-enables them, so a device left
        // running cannot storm a driver that can only drop what it produces.
        let _ = server.audio_mut().set_event_interrupts(false);
        return;
    }
    for endpoint in 0..MAX_DEVICE_ENDPOINTS {
        report_endpoint(server, regions, causes, endpoint);
    }
}

/// Move a period for `endpoint` if its boundary passed, and send whichever
/// notifications its causes name.
fn report_endpoint<A: Audio>(
    server: &mut AudioChannelServer<A>,
    regions: &mut [Option<Region>; ENDPOINT_SLOTS],
    causes: AudioInterrupt,
    endpoint: u16,
) {
    let slot = usize::from(endpoint);
    if AudioInterrupt::names(causes.period_elapsed, endpoint) {
        // The period notify carries the clock pair the mixer's linear fit is
        // built from, so it is sent from the service report rather than from
        // a second read of the device.
        if let Some(region) = regions[slot].as_mut() {
            match server.service(endpoint, region.bytes) {
                Ok(serviced) => {
                    notify(
                        server,
                        endpoint,
                        AudioChannelNotify::PeriodElapsed {
                            endpoint,
                            position: serviced.report.position,
                            sampled_at: serviced.report.sampled_at,
                        },
                    );
                    if serviced.lost_frames != 0 {
                        notify(
                            server,
                            endpoint,
                            AudioChannelNotify::Xrun {
                                endpoint,
                                position: serviced.report.position,
                                lost_frames: serviced.lost_frames,
                            },
                        );
                    }
                    // A drain the device has played out: the mixer handed
                    // these frames over long before they were heard, so only
                    // this side can say when the last one was.
                    if !serviced.report.running {
                        notify(
                            server,
                            endpoint,
                            AudioChannelNotify::Drained {
                                endpoint,
                                position: serviced.report.position,
                            },
                        );
                    }
                }
                Err(_) => {
                    // The mixer cannot be told a typed fault on this path, and
                    // re-arming into a device that just faulted would storm.
                    // Its next `Service` carries the reason and is what can
                    // release the sources.
                    let _ = server.audio_mut().set_event_interrupts(false);
                }
            }
        }
    } else if AudioInterrupt::names(causes.xrun, endpoint) {
        // A loss with no period boundary: nothing moved, so the position is
        // read from a zero-frame service rather than invented.
        if let Some(region) = regions[slot].as_mut() {
            if let Ok(serviced) = server.service(endpoint, region.bytes) {
                if serviced.lost_frames != 0 {
                    notify(
                        server,
                        endpoint,
                        AudioChannelNotify::Xrun {
                            endpoint,
                            position: serviced.report.position,
                            lost_frames: serviced.lost_frames,
                        },
                    );
                }
            }
        }
    }
    if AudioInterrupt::names(causes.jack_changed, endpoint) {
        if let Ok(facts) = server.audio().endpoint_facts(endpoint) {
            notify(
                server,
                endpoint,
                AudioChannelNotify::JackChanged {
                    endpoint,
                    jack: facts.jack,
                },
            );
        }
    }
}

/// Send one notification to the endpoint's attached notify port, if it has
/// one. A send that fails costs at worst a late mixer wake, which its next
/// period recovers.
fn notify<A: Audio>(server: &AudioChannelServer<A>, endpoint: u16, what: AudioChannelNotify) {
    if let Some(port) = server.notify_endpoint(endpoint) {
        let _ = tairix_rt::ipc_send(port, &what.encode());
    }
}

/// Serve one device-channel doorbell on the claimed endpoint: receive the
/// request, drive the pure [`AudioChannelServer`], and reply. A transient
/// recv error simply drops the doorbell (the mixer retries); a decode failure
/// is answered with the typed error so the mixer sees the exact refusal.
fn serve_call<A: Audio>(
    server: &mut AudioChannelServer<A>,
    endpoint: u64,
    request: &mut [u8; AUDIO_CHANNEL_MAX_REQUEST],
    regions: &mut [Option<Region>; ENDPOINT_SLOTS],
) {
    let mut ticket = 0u64;
    let Ok(request_len) = tairix_rt::call_recv(endpoint, request, &mut ticket) else {
        return;
    };
    match AudioChannelRequest::decode(&request[..request_len]) {
        Ok(AudioChannelRequest::Facts) => {
            let reply = server.facts_reply();
            let _ = tairix_rt::call_reply(endpoint, ticket, &reply);
        }
        Ok(AudioChannelRequest::EndpointFacts { endpoint: index }) => {
            let reply = server.endpoint_facts_reply(index);
            let _ = tairix_rt::call_reply(endpoint, ticket, &reply);
        }
        Ok(AudioChannelRequest::Configure(params)) => {
            let reply = server.configure_reply(&params);
            // A re-grant leaves any existing mapping the wrong shape, and the
            // server has already dropped its attach state, so the mapping
            // goes with it rather than being serviced against a stale
            // geometry.
            if !server.is_attached(params.endpoint) {
                release_region(regions, params.endpoint);
            }
            let _ = tairix_rt::call_reply(endpoint, ticket, &reply);
        }
        Ok(AudioChannelRequest::Attach(params)) => {
            let status = attach(server, &params, regions);
            let _ = tairix_rt::call_reply(endpoint, ticket, &status);
        }
        Ok(AudioChannelRequest::Start {
            endpoint: index,
            at,
        }) => {
            let reply = server.start(index, at);
            let _ = tairix_rt::call_reply(endpoint, ticket, &reply);
        }
        Ok(AudioChannelRequest::Stop {
            endpoint: index,
            at,
        }) => {
            let reply = server.stop(index, at);
            let _ = tairix_rt::call_reply(endpoint, ticket, &reply);
        }
        Ok(AudioChannelRequest::Drain { endpoint: index }) => {
            let reply = server.drain(index);
            let _ = tairix_rt::call_reply(endpoint, ticket, &reply);
        }
        Ok(AudioChannelRequest::Service { endpoint: index }) => {
            let reply = match regions.get_mut(usize::from(index)).and_then(Option::as_mut) {
                Some(region) => server.service_reply(index, region.bytes),
                // Detached (or an index the device does not present): the
                // server answers before it ever touches a slice.
                None => server.service_reply(index, &mut []),
            };
            // The mixer has just made room (playback) or taken frames
            // (capture), so this is where sources masked for back-pressure
            // come back up.
            if server.any_attached() {
                let _ = server.audio_mut().set_event_interrupts(true);
            }
            let _ = tairix_rt::call_reply(endpoint, ticket, &reply);
        }
        Ok(AudioChannelRequest::Gain {
            endpoint: index,
            millibel,
            mute,
        }) => {
            let reply = server.set_gain(index, millibel, mute);
            let _ = tairix_rt::call_reply(endpoint, ticket, &reply);
        }
        Ok(AudioChannelRequest::Detach { endpoint: index }) => {
            let reply = server.detach(index);
            release_region(regions, index);
            // With nothing left attached the device has nowhere to put
            // frames, so its sources go down rather than storming this
            // driver.
            if !server.any_attached() {
                let _ = server.audio_mut().set_event_interrupts(false);
            }
            let _ = tairix_rt::call_reply(endpoint, ticket, &reply);
        }
        Err(err) => {
            let _ = tairix_rt::call_reply(endpoint, ticket, &encode_status_reply(Err(err)));
        }
    }
}

/// Drop `endpoint`'s mapping, if it has one.
fn release_region(regions: &mut [Option<Region>; ENDPOINT_SLOTS], endpoint: u16) {
    if let Some(region) = regions
        .get_mut(usize::from(endpoint))
        .and_then(Option::take)
    {
        let _ = tairix_rt::shm_unmap(region.base, region.len);
    }
}

/// Map the PCM region the mixer granted, validate its length against the
/// agreed geometry, and attach the pure server. On any refusal the region is
/// unmapped and no attach state is kept (fail closed — a rejected attach
/// never half-binds).
fn attach<A: Audio>(
    server: &mut AudioChannelServer<A>,
    params: &AttachParams,
    regions: &mut [Option<Region>; ENDPOINT_SLOTS],
) -> [u8; tairix_abi::reply::STATUS_REPLY_LEN] {
    let Some(slot) = regions.get_mut(usize::from(params.endpoint)) else {
        return encode_status_reply(Err(Errno::NotFound));
    };
    // A re-attach without a prior detach releases the old mapping first.
    if let Some(previous) = slot.take() {
        let _ = tairix_rt::shm_unmap(previous.base, previous.len);
    }
    let mut len_out = 0u64;
    let mapped = tairix_rt::shm_map(params.region_grant, &mut len_out);
    if mapped < 0 {
        return encode_status_reply(Err(Errno::from_syscall(mapped)));
    }
    let (Ok(base), Ok(addr), Ok(map_len)) = (
        u64::try_from(mapped),
        usize::try_from(mapped),
        usize::try_from(len_out),
    ) else {
        return encode_status_reply(Err(Errno::DeviceFault));
    };
    let status = server.attach(params);
    let Some(geometry) = server.geometry(params.endpoint) else {
        // The server refused (a ring its own grant does not admit); drop the
        // mapping it will never use.
        let _ = tairix_rt::shm_unmap(base, map_len);
        return status;
    };
    let expected = geometry.region_len();
    if map_len < expected {
        // The grant is smaller than the geometry both sides agreed, so the
        // attach state the server just took is withdrawn before anything can
        // service it.
        let _ = server.detach(params.endpoint);
        let _ = tairix_rt::shm_unmap(base, map_len);
        // Both sizes, because "too small" without them names no defect: the
        // mixer sizes the region and the driver checks it, so which of the
        // two is wrong is only visible from the pair.
        log(
            &LogSink,
            &Event {
                level: Level::Error,
                id: AUDIOCHAN_SHORT_REGION,
                message: "audiochan: the granted region is smaller than the agreed ring",
                fields: &[
                    Field {
                        key: "mapped",
                        value: FieldValue::UnsignedInt(map_len as u64),
                    },
                    Field {
                        key: "expected",
                        value: FieldValue::UnsignedInt(expected as u64),
                    },
                    Field {
                        key: "ring_frames",
                        value: FieldValue::UnsignedInt(u64::from(params.ring_frames)),
                    },
                ],
            },
        );
        return encode_status_reply(Err(Errno::BufferTooSmall));
    }
    // SAFETY: `shm_map` mapped `map_len` bytes (>= `expected`, verified
    // above) of zeroed, cacheable, RW (non-executable) memory into this
    // process at `addr`. The ring view binds only the first `expected` bytes
    // — exactly the agreed geometry — so the exclusive `&mut [u8]` over
    // `expected` bytes is a sound subset of the mapping (any page-rounding
    // tail beyond it is left untouched). The region is owned by this process
    // until the matching `shm_unmap` (on detach, a re-attach, a
    // reconfiguration, or a rejected attach above) releases the full
    // `map_len`, and nothing else in this address space aliases it: the slot
    // it is stored in was emptied first, and every other slot holds a
    // different grant's mapping. The mixer maps the same frames through its
    // own grant; the ring's atomic positions are what order the two sides'
    // accesses to the samples.
    let bytes = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, expected) };
    regions[usize::from(params.endpoint)] = Some(Region {
        base,
        len: map_len,
        bytes,
    });
    // There is somewhere to put frames again, so the event sources — masked
    // whenever nothing is attached — come back up.
    let _ = server.audio_mut().set_event_interrupts(true);
    status
}
