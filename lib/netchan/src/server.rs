//! The driver-side per-request handler of the `netchan-v1` device-channel
//! contract (`plans/NETWORK.md` §2.3, N4d).
//!
//! [`NetChannelServer`] is the pure, host-testable engine a link-layer
//! driver *process* drives: the process owns the device (MMIO/DMA/IRQ) and
//! the call endpoint, and hands each decoded
//! [`NetChannelRequest`](tairix_abi::driver::net_channel::NetChannelRequest)
//! to this server, which turns it into the right
//! device action and the matching reply. The I/O — receiving the request,
//! mapping the granted frame region, sending the reply and the
//! receive-frames notify — stays in the crate's `serve` loop; the server
//! holds only the attach state and the ring/service logic, so the whole
//! control plane is exercised on the host against a mock [`Net`].
//!
//! # State machine
//!
//! A freshly-constructed server is **detached**: it answers
//! [`NetChannelRequest::Facts`](tairix_abi::driver::net_channel::NetChannelRequest::Facts)
//! (so the stack can size the ring geometry) and refuses
//! [`NetChannelRequest::Service`](tairix_abi::driver::net_channel::NetChannelRequest::Service)
//! with [`Errno::NotConnected`].
//! [`NetChannelRequest::Attach`](tairix_abi::driver::net_channel::NetChannelRequest::Attach)
//! validates the offered geometry against the
//! device and moves the server to **attached**; from there
//! [`NetChannelServer::service_reply`] binds a [`FrameRings`] view over the
//! caller-mapped region and drives one [`Net::service`] doorbell.
//! [`NetChannelRequest::Detach`](tairix_abi::driver::net_channel::NetChannelRequest::Detach)
//! returns the server to detached.
//!
//! # Fail closed
//!
//! Every reply is a fully-encoded `netchan-v1` frame. A service call before
//! attach, a region whose length does not match the agreed geometry, a
//! geometry too small to carry the device's frames, or any device fault is a
//! typed [`Errno`] carried in the reply's status word — never a panic, never
//! a partially-applied action.

use tairix_abi::driver::net::{LinkState, Net};
use tairix_abi::driver::net_channel::{
    encode_facts_reply, encode_service_reply, AttachParams, McastGroups, RxFilterPolicy,
    NET_CHANNEL_FACTS_REPLY_LEN, NET_CHANNEL_SERVICE_REPLY_LEN,
};
use tairix_abi::driver::net_ring::{FrameRings, RingGeometry, ServiceReport};
use tairix_abi::driver::BufferClass;
use tairix_abi::reply::{encode_status_reply, STATUS_REPLY_LEN};
use tairix_abi::Errno;
use tairix_net::rxfilter::RxClassifier;

/// What a driver's drain loop should do next, given the service report it
/// just saw.
///
/// The *policy* half of the interrupt path: whether the device's completion
/// sources may be re-armed is decided here, so it is exercised on the host,
/// while the serve loop supplies the mask-register write and the notify.
/// Getting it wrong is not a subtle bug — unmasking into a still-asserted
/// level condition spins the driver, and never unmasking wedges the device.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DrainStep {
    /// The device handed over frames and may have more; service it again.
    Continue,
    /// The shared receive ring filled while the device still had frames.
    /// The completion sources stay masked — that *is* the back-pressure —
    /// and the stack's next `Service`, issued after it drains, releases
    /// them.
    BackPressure,
    /// The device handed over everything it had, so its condition has
    /// cleared: re-arm the completion sources and stop.
    Quiet,
}

impl DrainStep {
    /// Classify one service report.
    #[must_use]
    pub const fn of(report: &ServiceReport) -> Self {
        if report.rx_ring_full {
            // Checked first: a full ring means frames are still in the
            // device however many were received, so the condition holds.
            Self::BackPressure
        } else if report.received == 0 {
            Self::Quiet
        } else {
            Self::Continue
        }
    }
}

/// Why a [`Drain`] left the device's completion sources masked, if it did.
///
/// Three causes, one state: whichever it was, the device raises no further
/// interrupt and only the stack's next `Service` can release or diagnose it.
/// Naming that state once keeps the three from drifting apart.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Masked {
    /// Not masked: the device fell quiet and the sources were re-armed.
    #[default]
    No,
    /// The shared receive ring filled while the device still had frames.
    BackPressure,
    /// The round budget was spent while the device still had work.
    BudgetSpent,
    /// A service faulted, so re-arming would storm a broken device.
    Fault,
}

impl Masked {
    /// Whether the stack must issue a `Service` to release or diagnose the
    /// sources — the notify's back-pressure flag.
    #[must_use]
    pub const fn needs_release(self) -> bool {
        !matches!(self, Self::No)
    }
}

/// What a [`Drain`] observed across a whole interrupt.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Drained {
    /// Any frame was received or transmitted.
    pub moved: bool,
    /// The device's link state at the end of the pass.
    pub link: LinkState,
    /// The link differs from what the previous pass saw.
    pub link_changed: bool,
    /// The device's cumulative receive-pre-filter count as last reported.
    pub filtered: u64,
    /// Whether the completion sources were left masked, and why.
    pub masked: Masked,
}

/// What the driver's interrupt path must do after one service report.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum DrainAction {
    /// Service the device again; the completion sources stay masked.
    Service,
    /// Unmask the completion sources and service once more before believing
    /// the device idle.
    UnmaskAndService,
    /// Re-mask the completion sources and service again: the look-once-more
    /// found work, so the drain resumes exactly as the interrupt entered.
    MaskAndService,
    /// Stop; [`Drain::outcome`] is final.
    Stop,
}

/// Which service call a [`Drain`] is interpreting.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum Phase {
    /// An ordinary drain pass, sources masked.
    Masked,
    /// The look-once-more after unmasking.
    ReChecking,
}

/// The whole interrupt-path drain policy as a pure state machine: fold in
/// each [`ServiceReport`] and it answers what to do next and, at the end,
/// what the stack must be told.
///
/// The serve loop supplies only the I/O — the mask-register writes, the
/// `service` calls, the notify — so the policy is exercised on the host.
/// Getting it wrong is not a subtle bug: unmasking into a still-asserted
/// level condition spins the driver, and stopping while masked without
/// saying so wedges the interface.
pub struct Drain {
    outcome: Drained,
    previous_link: LinkState,
    rounds_left: u32,
    phase: Phase,
}

impl Drain {
    /// Begin a drain against the link the previous pass reported, allowing
    /// `rounds` masked service passes.
    ///
    /// `rounds` is a fixed containment bound, not a capacity: a saturating
    /// flood must not pin the driver process in the drain loop and starve
    /// its call endpoint.
    #[must_use]
    pub const fn new(previous_link: LinkState, rounds: u32) -> Self {
        Self {
            outcome: Drained {
                moved: false,
                link: previous_link,
                link_changed: false,
                filtered: 0,
                masked: Masked::No,
            },
            previous_link,
            rounds_left: rounds,
            phase: Phase::Masked,
        }
    }

    /// Fold one service report in and decide the next step.
    pub fn observe(&mut self, report: &ServiceReport) -> DrainAction {
        self.outcome.moved |= report.received > 0 || report.transmitted > 0;
        self.outcome.link = report.link;
        self.outcome.link_changed = report.link != self.previous_link;
        self.outcome.filtered = report.filtered;
        match self.phase {
            Phase::Masked => match DrainStep::of(report) {
                DrainStep::BackPressure => {
                    self.outcome.masked = Masked::BackPressure;
                    DrainAction::Stop
                }
                DrainStep::Quiet => {
                    self.phase = Phase::ReChecking;
                    DrainAction::UnmaskAndService
                }
                DrainStep::Continue => self.spend(DrainAction::Service, Masked::BudgetSpent),
            },
            // Anything but quiet means the look-once-more found work, so the
            // drain resumes masked and the *next* pass classifies it; that
            // keeps one place deciding back-pressure.
            Phase::ReChecking => {
                if matches!(DrainStep::of(report), DrainStep::Quiet) {
                    DrainAction::Stop
                } else {
                    self.phase = Phase::Masked;
                    // The sources are up at this point, so a budget spent
                    // here strands nothing: the work the look-once-more
                    // found raises its own interrupt.
                    self.spend(DrainAction::MaskAndService, Masked::No)
                }
            }
        }
    }

    /// Fold a device fault in. The outcome is final and the caller masks the
    /// completion sources: re-arming into a device that just faulted would
    /// storm, and the stack is told because a masked source with nobody
    /// coming to release it is a permanently and silently dead interface.
    pub fn fault(&mut self) {
        self.outcome.masked = Masked::Fault;
    }

    /// What this drain observed. Final once [`observe`](Self::observe)
    /// returned [`DrainAction::Stop`] or [`fault`](Self::fault) was called.
    #[must_use]
    pub const fn outcome(&self) -> Drained {
        self.outcome
    }

    /// Charge one pass against the round budget, or stop reporting
    /// `exhausted` — the mask state the caller is actually left in.
    ///
    /// The bound must not silently wedge the interface: spent while the
    /// sources are down, the device will raise no further interrupt and only
    /// the stack's `Service` can release them, so it must be told. Stopping
    /// silently there is how an interface goes dead.
    fn spend(&mut self, action: DrainAction, exhausted: Masked) -> DrainAction {
        let Some(left) = self.rounds_left.checked_sub(1) else {
            self.outcome.masked = exhausted;
            return DrainAction::Stop;
        };
        self.rounds_left = left;
        action
    }
}

/// The state a channel carries once the stack has attached its frame region.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Attached {
    /// The ring geometry both sides agreed (the stack sized it from the
    /// device MTU); every [`FrameRings`] view binds with exactly this.
    geometry: RingGeometry,
    /// Sensitivity class the device honours when scrubbing its staging.
    class: BufferClass,
    /// Numeric IPC endpoint the driver `ipc_send`s a receive-frames notify
    /// to (the stack bound and owns it).
    notify_endpoint: u64,
}

/// The driver-side handler of one NIC device channel.
///
/// Wraps the concrete [`Net`] device engine and tracks whether the stack has
/// attached a frame region. It never performs I/O: the process binary
/// receives the request, maps the region, and sends the reply this server
/// produces.
pub struct NetChannelServer<N: Net> {
    net: N,
    attached: Option<Attached>,
    /// Link state the last service reported, so the interrupt path can tell
    /// a change from a steady state.
    reported_link: LinkState,
    /// The receive pre-filter the harvest path applies. Until the stack
    /// publishes an address set this admits everything, so nothing is ever
    /// dropped for want of a policy.
    filter: RxClassifier,
}

impl<N: Net> NetChannelServer<N> {
    /// Build a detached server over the device engine `net`.
    pub fn new(net: N) -> Self {
        Self {
            net,
            attached: None,
            // Until a service reports otherwise, assume the operational
            // default: a device that cannot sense its link reports `Up`.
            reported_link: LinkState::default(),
            filter: RxClassifier::new(RxFilterPolicy::admit_all()),
        }
    }

    /// Borrow the underlying device engine (host-test access).
    #[must_use]
    pub fn net(&self) -> &N {
        &self.net
    }

    /// Borrow the underlying device engine mutably.
    ///
    /// The driver process needs this to mask and unmask the device's
    /// completion sources and to acknowledge its interrupt around a drain —
    /// the interrupt-side work that is I/O and so lives in the serve loop
    /// rather than in this pure server.
    pub fn net_mut(&mut self) -> &mut N {
        &mut self.net
    }

    /// Whether the stack has attached a frame region.
    #[must_use]
    pub fn is_attached(&self) -> bool {
        self.attached.is_some()
    }

    /// The numeric IPC endpoint the driver notifies when receive frames
    /// arrive, or [`None`] while detached.
    #[must_use]
    pub fn notify_endpoint(&self) -> Option<u64> {
        self.attached.as_ref().map(|a| a.notify_endpoint)
    }

    /// The agreed ring geometry, or [`None`] while detached. The process
    /// binary sizes the region it maps from
    /// [`RingGeometry::region_len`](RingGeometry::region_len).
    #[must_use]
    pub fn geometry(&self) -> Option<RingGeometry> {
        self.attached.as_ref().map(|a| a.geometry)
    }

    /// Answer [`NetChannelRequest::Facts`](tairix_abi::driver::net_channel::NetChannelRequest::Facts):
    /// the device's validated facts, or a `-errno` status on a device fault.
    #[must_use]
    pub fn facts_reply(&self) -> [u8; NET_CHANNEL_FACTS_REPLY_LEN] {
        encode_facts_reply(
            self.net
                .device_facts()
                .map_err(tairix_abi::DriverError::as_errno),
        )
    }

    /// Answer [`NetChannelRequest::Attach`](tairix_abi::driver::net_channel::NetChannelRequest::Attach):
    /// validate the offered geometry against the device and, on success,
    /// move to attached. Returns the status frame; on refusal the server is
    /// left unchanged (fail closed — a rejected attach never half-binds).
    ///
    /// The geometry's slot must be able to carry the device's largest frame
    /// (its MTU plus the Ethernet header); a stack that offered a smaller
    /// ring than it promised via the [`facts_reply`](Self::facts_reply) is
    /// refused rather than silently dropping every oversize frame later.
    #[must_use]
    pub fn attach(&mut self, params: AttachParams) -> [u8; STATUS_REPLY_LEN] {
        let result = self.try_attach(params);
        encode_status_reply(result)
    }

    fn try_attach(&mut self, params: AttachParams) -> Result<(), Errno> {
        let facts = self
            .net
            .device_facts()
            .map_err(tairix_abi::DriverError::as_errno)?;
        // Both directions must carry at least one device frame; when the
        // device segments (`TX_SEGMENT_TCP`) the transmit ring must
        // additionally carry a super-frame. `for_device` is the one
        // definition of those minima (the stack sized its offer from the
        // same facts), so a ring smaller than the device needs is refused
        // rather than silently dropping oversize frames later.
        let need = RingGeometry::for_device(&facts, params.geometry.slots())?;
        if params.geometry.rx_slot_capacity() < need.rx_slot_capacity()
            || params.geometry.tx_slot_capacity() < need.tx_slot_capacity()
        {
            return Err(Errno::OutOfRange);
        }
        self.attached = Some(Attached {
            geometry: params.geometry,
            class: params.class,
            notify_endpoint: params.notify_endpoint,
        });
        Ok(())
    }

    /// Answer [`NetChannelRequest::Service`](tairix_abi::driver::net_channel::NetChannelRequest::Service):
    /// bind the frame rings over the caller-mapped `region` and drive one
    /// [`Net::service`] doorbell, reporting what moved.
    ///
    /// `region` is the driver's own mapping of the shared frame region (the
    /// process binary `shm_map`ped the grant the stack forwarded in
    /// [`Attach`](tairix_abi::driver::net_channel::NetChannelRequest::Attach)).
    /// A service before attach fails closed with [`Errno::NotConnected`]; a
    /// region whose length does not match the agreed geometry, or a device
    /// fault, is the typed error carried in the reply.
    #[must_use]
    pub fn service_reply(&mut self, region: &mut [u8]) -> [u8; NET_CHANNEL_SERVICE_REPLY_LEN] {
        encode_service_reply(self.service(region))
    }

    /// Drive one device doorbell over `region` and return its report.
    ///
    /// The same work [`service_reply`](Self::service_reply) encodes, for the
    /// driver's own interrupt path: a device interrupt harvests straight
    /// into the shared region rather than waiting to be asked, so the report
    /// is needed as a value rather than as reply bytes.
    ///
    /// # Errors
    ///
    /// [`Errno::NotConnected`] before attach, [`Errno::BufferTooSmall`] or
    /// [`Errno::BadAlignment`] for a region that does not match the agreed
    /// geometry, or the device's typed fault.
    pub fn service(&mut self, region: &mut [u8]) -> Result<ServiceReport, Errno> {
        let attached = self.attached.as_ref().ok_or(Errno::NotConnected)?;
        let mut rings =
            FrameRings::bind(region, attached.geometry, attached.class)?.with_admit(&self.filter);
        let report = self
            .net
            .service(&mut rings)
            .map_err(tairix_abi::DriverError::as_errno)?;
        self.reported_link = report.link;
        Ok(report)
    }

    /// The link state the last [`service`](Self::service) reported.
    ///
    /// Lets the driver's interrupt path tell a link *change* from a steady
    /// state, so a notify goes out for a cable pull on an interface with no
    /// traffic — the only thing that would otherwise surface it is an
    /// unrelated transmit.
    #[must_use]
    pub fn reported_link(&self) -> LinkState {
        self.reported_link
    }

    /// Answer [`NetChannelRequest::SetRxFilter`](tairix_abi::driver::net_channel::NetChannelRequest::SetRxFilter):
    /// replace the local addresses the receive pre-filter matches against.
    ///
    /// Always accepted: the filter can only ever shed work, so there is
    /// nothing for a device to refuse and no state it could conflict with.
    /// Accepted whether or not the channel is attached, so the stack may
    /// publish an address set before frames flow.
    #[must_use]
    pub fn set_rx_filter_reply(&mut self, policy: RxFilterPolicy) -> [u8; STATUS_REPLY_LEN] {
        self.filter = RxClassifier::new(policy);
        encode_status_reply(Ok(()))
    }

    /// Answer [`NetChannelRequest::SetMulticast`](tairix_abi::driver::net_channel::NetChannelRequest::SetMulticast):
    /// replace the group addresses the device admits.
    ///
    /// Accepted whether or not the channel is attached: the filter is device
    /// state, not channel state, so the stack may program it before frames
    /// flow. A device that does not filter groups refuses with
    /// [`Errno::NotImplemented`]; an over-large set is refused whole and the
    /// previously admitted set stays in force.
    #[must_use]
    pub fn set_multicast_reply(&mut self, groups: &McastGroups) -> [u8; STATUS_REPLY_LEN] {
        encode_status_reply(
            self.net
                .set_multicast_groups(groups.as_slice())
                .map_err(tairix_abi::DriverError::as_errno),
        )
    }

    /// Answer [`NetChannelRequest::Detach`](tairix_abi::driver::net_channel::NetChannelRequest::Detach):
    /// forget the attach state. The process binary unmaps the region and
    /// forgets the notify port; the device itself stays live for a later
    /// re-attach.
    #[must_use]
    pub fn detach(&mut self) -> [u8; STATUS_REPLY_LEN] {
        self.attached = None;
        encode_status_reply(Ok(()))
    }
}

#[cfg(test)]
mod tests;
