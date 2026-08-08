//! The freestanding body of the RAID member-agent driver's `Run` binary
//! (`main.rs`): the member agent's grant resolution, delegation, and
//! membership loop (`plans/FIX-IO.md` `IO6c`).
//!
//! # Delegate, offer, hold
//!
//! The agent's whole job is to put *one* device's transport into the hands of
//! the process that composes arrays, and to keep doing so for as long as the
//! device is here. Each turn it delegates its two granted resources to the
//! composer's reserved rendezvous and posts an offer naming them; the composer
//! answers only when the membership ends, so the agent then parks on that one
//! reply. It never touches the device: it holds the grants and forwards them,
//! and the composer does the reading.
//!
//! Every wait is an event: the reply wakes the agent, the composer's endpoint
//! being torn down cancels the call and wakes it too, and the only timed wait
//! is the paced re-offer when no composer is listening yet. Nothing here polls
//! the rendezvous and nothing polls the disk.

use tairix_abi::hwtree::HwResourceKind;
use tairix_abi::raid_ipc::{MemberOffer, MembershipEnd, RAID_REGISTRY_ENDPOINT};
use tairix_abi::reply::STATUS_REPLY_LEN;
use tairix_abi::waitset::{WaitSetOp, WaitSourceKind};
use tairix_abi::{CapabilityId, Errno};
use tairix_caps::CapabilitySet;
use tairix_drv_storage_raid_member::{AgentStep, MemberAgent};
use tairix_drvrt::{RtDriverHost, RtGrantSyscalls};
use tairix_log::{log, Event, EventId, Field, FieldValue, Level};
use tairix_rt::LogSink;
use tairix_util::fmt::format_hex_u64;

/// Exit code when the rt-backed driver host could not be built from the
/// kernel-delivered grants. A reserved, fail-closed value.
const EXIT_NO_HOST: i32 = 90;

/// Exit code when the matched member node did not carry the block endpoint
/// and shared-window grants this agent must delegate.
const EXIT_NO_TRANSPORT: i32 = 91;

/// Exit code when the agent could not mint the wait-set it parks its
/// membership on — without it every wait would have to become a poll.
const EXIT_NO_WAITSET: i32 = 92;

/// Diagnostic event id: the device was offered to the composer and the
/// membership is being held open.
const RAID_MEMBER_OFFERED: EventId = EventId(4186);

/// Diagnostic event id: no composer is listening on the rendezvous yet, so
/// the offer is paced and made again.
const RAID_NO_COMPOSER: EventId = EventId(4187);

/// Diagnostic event id: the composer released this member — the array was
/// torn down, or the device was removed from it.
const RAID_MEMBER_RELEASED: EventId = EventId(4188);

/// Diagnostic event id: the composer refused this device; the agent stops.
const RAID_MEMBER_REFUSED: EventId = EventId(4189);

/// The capability set the driver host re-checks up front; the kernel is the
/// authority and re-checks every trap. It is the least-privilege set the agent
/// needs — no MMIO, DMA, IRQ, node emission, or mount authority, and no
/// filesystem access: it delegates the transport it was granted and nothing
/// else.
fn driver_caps() -> CapabilitySet {
    let mut caps = CapabilitySet::empty();
    caps.insert(CapabilityId::SHM);
    caps.insert(CapabilityId::IPC_ENDPOINT);
    caps.insert(CapabilityId::LOG_EMIT);
    caps
}

/// Emit one diagnostic record carrying a single hex-rendered field.
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

/// The two resource ids the agent delegates, resolved once from the grants the
/// matched member node carried.
struct Transport {
    /// The device's block-service call endpoint.
    endpoint: u64,
    /// The device's shared data window, named by its region id — the same id
    /// `shm_grant` delegates, not the address it maps at. The agent never maps
    /// the window: it has no reason to look at the device's bytes.
    window: u64,
}

impl Transport {
    /// Resolve the member's transport from the driver host's grants, or
    /// [`None`] when the matched node did not carry both.
    fn resolve<S: tairix_drvrt::GrantSyscalls>(host: &RtDriverHost<S>) -> Option<Self> {
        let endpoint = host.endpoint_grant()?;
        let window = host
            .resources()
            .find(|resource| resource.kind() == Some(HwResourceKind::Shared))?
            .base();
        Some(Self { endpoint, window })
    }
}

/// The agent's wait-set, holding the one `CallReply` member it parks on.
///
/// The member can only be added once the rendezvous exists, so it is added
/// lazily after the first offer the composer actually accepted; before that
/// there is nothing to observe and the set simply times the paced re-offer.
struct Waits {
    set: u64,
    observing: bool,
}

impl Waits {
    /// Mint the wait-set. The kernel reclaims it when this process exits.
    fn create() -> Result<Self, Errno> {
        let raw = tairix_rt::waitset_create();
        let Ok(set) = u64::try_from(raw) else {
            return Err(tairix_rt::errno_from_raw(raw));
        };
        Ok(Self {
            set,
            observing: false,
        })
    }

    /// Observe the rendezvous's replies, once. Adding the member needs the
    /// send authority the post already exercised, so this follows a delivered
    /// offer rather than preceding it.
    fn observe_replies(&mut self) {
        if self.observing {
            return;
        }
        let ctl = tairix_rt::waitset_ctl(
            self.set,
            WaitSetOp::Add,
            WaitSourceKind::CallReply,
            RAID_REGISTRY_ENDPOINT,
            0,
        );
        self.observing = ctl >= 0;
    }

    /// Park until `timeout_ns` from now, or until a member becomes ready.
    fn park(&self, timeout_ns: u64) {
        let mut token = 0u64;
        let _ = tairix_rt::waitset_wait(self.set, timeout_ns, &mut token);
    }
}

/// Delegate the device's transport to the rendezvous and post one offer,
/// returning the membership ticket.
///
/// Both delegations are repeated on every offer: a composer that restarted is
/// a new task holding none of the previous one's grants, and re-granting a
/// resource a recipient already holds hands back the handle it has rather than
/// minting a second, so repeating costs nothing and re-arms everything.
fn offer(transport: &Transport, node: u32) -> Result<u64, Errno> {
    let granted = tairix_rt::call_grant(transport.endpoint, RAID_REGISTRY_ENDPOINT);
    if granted < 0 {
        return Err(tairix_rt::errno_from_raw(granted));
    }
    let shared = tairix_rt::shm_grant(transport.window, RAID_REGISTRY_ENDPOINT);
    if shared < 0 {
        return Err(tairix_rt::errno_from_raw(shared));
    }
    let request = MemberOffer {
        endpoint: transport.endpoint,
        window: transport.window,
        node,
    };
    let mut frame = [0u8; MemberOffer::WIRE_LEN];
    let len = request.encode(&mut frame)?;
    // No deadline: the membership lasts as long as the array holds the device,
    // and the composer going away cancels the call and wakes the agent, so
    // there is no wedge for a deadline to break.
    tairix_rt::call_post(RAID_REGISTRY_ENDPOINT, &frame[..len], u64::MAX)
        .map_err(tairix_rt::errno_from_raw)
}

/// Claim the membership's outcome, parking until it is known.
fn await_end(waits: &Waits, ticket: u64) -> MembershipEnd {
    let mut reply = [0u8; STATUS_REPLY_LEN];
    loop {
        match tairix_rt::call_reap(RAID_REGISTRY_ENDPOINT, ticket, &mut reply) {
            Ok(len) => return MembershipEnd::from_reply(Some(&reply[..len.min(reply.len())])),
            Err(neg) if tairix_rt::errno_from_raw(neg) == Errno::WouldBlock => waits.park(u64::MAX),
            // Anything else retires the ticket: the endpoint was torn down
            // (the composer went away), or the reply could not be claimed.
            // Either way this membership is over and the agent re-offers.
            Err(_) => return MembershipEnd::ComposerGone,
        }
    }
}

fn main() -> i32 {
    let Ok(host) = RtDriverHost::from_grants_query(driver_caps(), RtGrantSyscalls, None) else {
        return EXIT_NO_HOST;
    };
    let Some(transport) = Transport::resolve(&host) else {
        return EXIT_NO_TRANSPORT;
    };
    let Ok(mut waits) = Waits::create() else {
        return EXIT_NO_WAITSET;
    };
    // The agent's own node, so the composer can name this member in its audit
    // trail. Kernel-resolved from the calling task, never claimed; a node the
    // kernel will not name is simply unnamed in the record.
    let node = u32::try_from(tairix_rt::hw_self_node()).unwrap_or(0);

    let mut agent = MemberAgent::new();
    // The outstanding membership's ticket, held only between the offer that
    // minted it and the claim that consumes it.
    let mut ticket: Option<u64> = None;
    loop {
        let now = tairix_rt::clock_get();
        match agent.next_step(now) {
            AgentStep::Offer => match offer(&transport, node) {
                Ok(minted) => {
                    ticket = Some(minted);
                    agent.note_offered(true, now);
                    waits.observe_replies();
                    log_hex_event(
                        RAID_MEMBER_OFFERED,
                        Level::Info,
                        "raid: member offered to the array composer",
                        "endpoint_hex",
                        transport.endpoint,
                    );
                }
                Err(errno) => {
                    agent.note_offered(false, now);
                    log_hex_event(
                        RAID_NO_COMPOSER,
                        Level::Info,
                        "raid: no array composer listening yet; re-offering",
                        "errno_hex",
                        errno as u64,
                    );
                }
            },
            AgentStep::AwaitReply => {
                // A membership with no ticket cannot be claimed, so it is over
                // as far as this agent can tell; re-offering is the only way
                // back and is what `ComposerGone` asks for.
                let end = match ticket.take() {
                    Some(held) => await_end(&waits, held),
                    None => MembershipEnd::ComposerGone,
                };
                agent.note_end(end, tairix_rt::clock_get());
                log_membership_end(end, transport.endpoint);
            }
            AgentStep::Retry { deadline_ns } => waits.park(deadline_ns.saturating_sub(now)),
            // The composer read this device and will not compose it. Exiting
            // is the honest end: the verdict came from the device's own
            // metadata, and `devmgr` will start a fresh agent if the device —
            // and so its node — ever comes back.
            AgentStep::Stop => return 0,
        }
    }
}

/// Record how one membership ended, so a member disk's history is readable
/// from the log whether the array released it, refused it, or lost its
/// composer.
fn log_membership_end(end: MembershipEnd, endpoint: u64) {
    match end {
        MembershipEnd::Released => log_hex_event(
            RAID_MEMBER_RELEASED,
            Level::Info,
            "raid: member released by the array composer",
            "endpoint_hex",
            endpoint,
        ),
        MembershipEnd::Refused(errno) => log_hex_event(
            RAID_MEMBER_REFUSED,
            Level::Warn,
            "raid: array composer refused this device",
            "errno_hex",
            errno as u64,
        ),
        MembershipEnd::ComposerGone => log_hex_event(
            RAID_NO_COMPOSER,
            Level::Warn,
            "raid: array composer went away; re-offering",
            "endpoint_hex",
            endpoint,
        ),
    }
}

tairix_rt::entry!(main);
