//! The `Run` entry-point binary of the audio service, installed as the
//! signed `/System/Services/audiod.app` bundle (`plans/SOUND.md` SND4).
//!
//! It is the process half of `tairix_audiod`: it claims the reserved
//! `audio-v1` rendezvous, binds one notify port per device channel it
//! adopts, and parks on a wait set over {control endpoint, every device's
//! notify port} for the life of the machine. Nothing spins: the device's own
//! period interrupt reaches this loop as a driver notify, and there is no
//! audio tick anywhere in the system.
//!
//! The three I/O seams the engine is written over are backed here, and
//! nowhere else:
//!
//! * `RtRegions` — `shm_create` / `shm_map` / `shm_grant` / `shm_unmap`.
//!   Regions run both ways: a device ring is created here and granted *to*
//!   the driver, a client ring is created by the client and adopted *from*
//!   its grant.
//! * `RtChannel` — one `ipc_call` per `audiochan-v1` control operation.
//! * `RtNotifier` — one `ipc_send` per client wake.
//!
//! On the host it is an inert stub, so `cargo build --workspace`, clippy and
//! fmt still cover the file while the host test build never links the
//! userland runtime.

#![cfg_attr(freestanding, no_std)]
#![cfg_attr(freestanding, no_main)]
#![deny(missing_docs)]

// --- Pure-Rust program --------------------------------------------------
#[cfg(freestanding)]
mod program {
    extern crate alloc;

    use alloc::boxed::Box;
    use alloc::vec::Vec;

    use tairix_abi::audio::{
        AudioRequest, AUDIO_ENDPOINT, AUDIO_MAX_REPLY, AUDIO_MAX_REQUEST, AUDIO_NOTIFY_LEN,
    };
    use tairix_abi::driver::audio_channel::AUDIO_CHANNEL_NOTIFY_LEN;
    use tairix_abi::reply::encode_status_reply;
    use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
    use tairix_abi::{CapabilityId, Errno, Origin, ORIGIN_WIRE_LEN};
    use tairix_audiod::events;
    use tairix_audiod::{exit, AudioChannelTransport, AudioService, Caller, RegionHost, RegionId};
    use tairix_caps::CapabilitySet;
    use tairix_log::{log, Event, Level};
    use tairix_rt::{ClockDelay, LogSink};

    /// Outstanding-call capacity of the `audio-v1` rendezvous. Every client
    /// blocks on its reply, so a short queue only absorbs a request racing
    /// the previous reply — a fail-closed memory bound, not a capacity.
    const ENDPOINT_CAPACITY: usize = 8;

    /// Wait-set token of the control endpoint.
    const CONTROL_TOKEN: u64 = 0;

    /// One buffer wide enough for either notify frame the serve loop can
    /// receive. Only a device-channel frame arrives on these ports today;
    /// sizing to the wider of the two keeps the buffer honest if either
    /// grows.
    const NOTIFY_BUF: usize = if AUDIO_CHANNEL_NOTIFY_LEN > AUDIO_NOTIFY_LEN {
        AUDIO_CHANNEL_NOTIFY_LEN
    } else {
        AUDIO_NOTIFY_LEN
    };

    /// Wait-set token of the `n`th adopted device channel's notify port.
    const fn device_token(index: usize) -> u64 {
        // Token zero is the control endpoint, so devices start at one.
        index as u64 + 1
    }

    /// The notify port this service binds for the `index`th device channel.
    ///
    /// Derived from this process's own pid so two audio services could never
    /// collide, and unreserved so it needs no privileged bind; the mailbox is
    /// owner-only to receive, so a bystander cannot steal a driver's wakes.
    fn device_notify_endpoint(pid: u64, index: usize) -> u64 {
        tairix_abi::audio::notify_endpoint_for(pid, index as u64)
    }

    /// One shared PCM region this service holds, and where it came from.
    ///
    /// Only a region this service *created* carries a kernel id, because
    /// only its owner may delegate it onward; a region a client granted is
    /// mapped and read, never re-granted.
    enum Backing {
        Created(tairix_rt::shm::SharedRegion),
        Adopted(tairix_rt::shm::MappedGrant),
    }

    impl Backing {
        fn bytes_mut(&mut self) -> &mut [u8] {
            match self {
                Self::Created(region) => region.bytes_mut(),
                Self::Adopted(grant) => grant.bytes_mut(),
            }
        }
    }

    /// One mapped shared PCM region, unmapped when its backing drops.
    struct Mapping {
        id: RegionId,
        /// The agreed ring's own byte length. A mapping is page-rounded and
        /// so may be longer; the ring binds the exact geometry and nothing
        /// beyond it.
        used: usize,
        backing: Backing,
    }

    /// The live region host, over the unprivileged anonymous shared-memory
    /// syscalls.
    struct RtRegions {
        mappings: Vec<Mapping>,
        next: u32,
    }

    impl RtRegions {
        const fn new() -> Self {
            Self {
                mappings: Vec::new(),
                next: 1,
            }
        }

        /// Record `backing`, whose agreed ring is its first `used` bytes,
        /// under a fresh local identity.
        fn record(&mut self, backing: Backing, used: usize) -> RegionId {
            let id = RegionId(self.next);
            self.next = self.next.wrapping_add(1).max(1);
            self.mappings.push(Mapping { id, used, backing });
            id
        }

        fn slot(&mut self, region: RegionId) -> Option<usize> {
            self.mappings
                .iter()
                .position(|mapping| mapping.id == region)
        }
    }

    impl RegionHost for RtRegions {
        fn create(&mut self, len: usize) -> Result<RegionId, Errno> {
            let region = tairix_rt::shm::SharedRegion::create(len).ok_or(Errno::OutOfMemory)?;
            Ok(self.record(Backing::Created(region), len))
        }

        fn adopt(&mut self, grant: u64, len: usize) -> Result<RegionId, Errno> {
            let mapped = tairix_rt::shm::MappedGrant::map(grant, len)?;
            Ok(self.record(Backing::Adopted(mapped), len))
        }

        fn grant(&mut self, region: RegionId, endpoint: u64) -> Result<u64, Errno> {
            let slot = self.slot(region).ok_or(Errno::NotFound)?;
            // Delegation names the region by its kernel id. A region this
            // service only holds a grant for is not its to pass on.
            let Backing::Created(created) = &self.mappings[slot].backing else {
                return Err(Errno::PermissionDenied);
            };
            let granted = tairix_rt::shm_grant(created.id(), endpoint);
            if granted < 0 {
                return Err(Errno::from_syscall(granted));
            }
            #[allow(clippy::cast_sign_loss)] // `granted >= 0` is the handle.
            Ok(granted as u64)
        }

        fn bytes(&mut self, region: RegionId) -> Result<&mut [u8], Errno> {
            let slot = self.slot(region).ok_or(Errno::NotFound)?;
            let mapping = &mut self.mappings[slot];
            let used = mapping.used;
            mapping
                .backing
                .bytes_mut()
                .get_mut(..used)
                .ok_or(Errno::BufferTooSmall)
        }

        fn release(&mut self, region: RegionId) {
            let Some(slot) = self.slot(region) else {
                return;
            };
            self.mappings.remove(slot);
        }
    }

    /// One driver's device-channel call endpoint.
    struct RtChannel {
        endpoint: u64,
    }

    impl AudioChannelTransport for RtChannel {
        fn call(&mut self, request: &[u8], reply: &mut [u8]) -> Result<usize, Errno> {
            tairix_rt::ipc_call(self.endpoint, request, reply).map_err(Errno::from_syscall)
        }
    }

    /// The client wake-up transport.
    struct RtNotifier;

    impl tairix_audiod::Notifier for RtNotifier {
        fn notify(&mut self, endpoint: u64, frame: &[u8]) {
            let _ = tairix_rt::ipc_send(endpoint, frame);
        }
    }

    /// Program entry point. Never returns on the success path.
    fn main() -> i32 {
        let empty = CapabilitySet::empty();
        // The rendezvous is unrestricted-sender — every program plays sound
        // — but its id is reserved, so claiming it needs the manifest's
        // privileged bind: a squatter would otherwise receive every
        // program's samples and learn their shared-memory grants.
        if tairix_rt::call_create(
            AUDIO_ENDPOINT,
            &empty,
            &empty,
            AUDIO_MAX_REQUEST,
            AUDIO_MAX_REPLY,
            ENDPOINT_CAPACITY,
        ) != 0
        {
            return exit::NO_SERVICE;
        }
        let set = tairix_rt::waitset_create();
        if set < 0 {
            return exit::NO_RESOURCES;
        }
        #[allow(clippy::cast_sign_loss)] // `set >= 0` is the wait-set handle.
        let set = set as u64;
        if tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Endpoint,
            AUDIO_ENDPOINT,
            CONTROL_TOKEN,
        ) != 0
        {
            return exit::NO_RESOURCES;
        }
        // The mixing path must not be preempted by ordinary work, and an
        // audio buffer must never be paged out — releasing one under memory
        // pressure buys a few kibibytes and costs an audible glitch. Both are
        // refusable, and a refusal is reported rather than fatal: the service
        // still plays sound, it just no longer promises not to stutter.
        harden();
        let Ok(origin) = tairix_rt::self_origin() else {
            return exit::NO_SERVICE;
        };
        log(
            &LogSink,
            &Event {
                level: Level::Info,
                id: events::AUDIOD_READY,
                message: "audiod: audio service endpoint claimed, serving",
                fields: &[],
            },
        );
        serve(set, origin.pid())
    }

    /// Take the real-time priority and the memory pin the period path needs,
    /// reporting whichever the machine refused.
    fn harden() {
        for (taken, what) in [
            (
                tairix_rt::sched_set_realtime(true) == 0,
                "real-time priority",
            ),
            (tairix_rt::mem_pin() == 0, "pinned audio memory"),
        ] {
            if !taken {
                log(
                    &LogSink,
                    &Event {
                        level: Level::Warn,
                        id: events::AUDIOD_READY,
                        message: "audiod: refused a real-time guarantee; serving without it",
                        fields: &[tairix_log::Field {
                            key: "guarantee",
                            value: tairix_log::FieldValue::Str(what),
                        }],
                    },
                );
            }
        }
    }

    /// Park on the wait set and serve control requests and device notifies
    /// for the life of the service.
    fn serve(set: u64, pid: u64) -> i32 {
        let mut service = AudioService::new(RtRegions::new(), RtNotifier, ClockDelay::new());
        let mut request = [0u8; AUDIO_MAX_REQUEST];
        let mut reply = [0u8; AUDIO_MAX_REPLY];
        let mut notify = [0u8; NOTIFY_BUF];
        loop {
            let mut token = 0u64;
            let woke = tairix_rt::waitset_wait(set, u64::MAX, &mut token);
            if woke < 0 {
                return exit::NO_SERVICE;
            }
            if woke != 0 {
                // A lapsed wake with no ready source; re-park.
                continue;
            }
            if token == CONTROL_TOKEN {
                serve_control(&mut service, set, pid, &mut request, &mut reply);
                continue;
            }
            let Some(index) = usize::try_from(token.saturating_sub(1)).ok() else {
                continue;
            };
            let Some(port) = service.device_notify_endpoint(index) else {
                continue;
            };
            let mut from = [0u8; ORIGIN_WIRE_LEN];
            while let Ok(len) = tairix_rt::ipc_recv(port, &mut notify, &mut from) {
                service.on_device_notify(index, &notify[..len], &LogSink);
            }
        }
    }

    /// Serve one control-endpoint doorbell.
    ///
    /// The bind operation is handled here rather than in the engine because
    /// only this half can build a transport and bind a notify port; every
    /// other request is the engine's, checked against the caller's
    /// kernel-attested origin.
    fn serve_control(
        service: &mut AudioService<RtRegions, RtNotifier, ClockDelay>,
        set: u64,
        pid: u64,
        request: &mut [u8; AUDIO_MAX_REQUEST],
        reply: &mut [u8; AUDIO_MAX_REPLY],
    ) {
        let mut ticket = 0u64;
        let Ok(len) = tairix_rt::call_recv(AUDIO_ENDPOINT, request, &mut ticket) else {
            return;
        };
        let Some(origin) = peer_origin(ticket) else {
            let _ = tairix_rt::call_reply(
                AUDIO_ENDPOINT,
                ticket,
                &encode_status_reply(Err(Errno::PermissionDenied)),
            );
            return;
        };
        if let Ok(AudioRequest::BindDriver { endpoint_id }) = AudioRequest::decode(&request[..len])
        {
            let status = bind_driver(service, set, pid, &origin, endpoint_id);
            let _ = tairix_rt::call_reply(AUDIO_ENDPOINT, ticket, &encode_status_reply(status));
            return;
        }
        let caller = Caller {
            origin,
            // Sinks are leased to seats by the seat integration; until it
            // lands no sink is claimed and the router admits any principal.
            seat: None,
        };
        let reply_len = service.handle(&caller, &request[..len], reply, &LogSink);
        let _ = tairix_rt::call_reply(AUDIO_ENDPOINT, ticket, &reply[..reply_len]);
    }

    /// Adopt a driver's device channel, having checked the caller genuinely
    /// holds the authority to put a driver on this machine.
    fn bind_driver(
        service: &mut AudioService<RtRegions, RtNotifier, ClockDelay>,
        set: u64,
        pid: u64,
        origin: &Origin,
        endpoint_id: u64,
    ) -> Result<(), Errno> {
        if !origin.capabilities().holds_cap(CapabilityId::DRV_LOAD) {
            return Err(Errno::PermissionDenied);
        }
        let index = service.device_count();
        let port = device_notify_endpoint(pid, index);
        // A port already bound from an earlier adoption at this index is
        // this service's own and is reused; anything else is a refusal.
        let bound = tairix_rt::port_bind(port, AUDIO_CHANNEL_NOTIFY_LEN, 16);
        if bound != 0 && bound != -i64::from(Errno::AlreadyExists.as_i32().unsigned_abs()) {
            return Err(Errno::from_syscall(bound));
        }
        // A message port, not a call endpoint: the driver *notifies* this
        // side, it never calls it.
        let added = tairix_rt::waitset_ctl(
            set,
            WaitSetOp::Add,
            WaitSourceKind::Port,
            port,
            device_token(index),
        );
        if added != 0 {
            return Err(Errno::from_syscall(added));
        }
        service.bind_device(
            endpoint_id,
            port,
            Box::new(RtChannel {
                endpoint: endpoint_id,
            }),
            &LogSink,
        )
    }

    /// The kernel-attested origin of the caller holding `ticket`.
    fn peer_origin(ticket: u64) -> Option<Origin> {
        let mut buf = [0u8; ORIGIN_WIRE_LEN];
        let len = tairix_rt::call_peer_origin(AUDIO_ENDPOINT, ticket, &mut buf).ok()?;
        Origin::from_bytes(buf.get(..len)?).ok()
    }

    tairix_rt::entry!(main);
}

// --- Host stub ----------------------------------------------------------
//
// On the host the freestanding entry is not compiled, so this inert `main`
// keeps the crate building under the host tooling. It performs no I/O.
#[cfg(not(freestanding))]
fn main() {}
