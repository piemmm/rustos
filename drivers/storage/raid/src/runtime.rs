//! One live array the composer serves, and the self-maintenance it drives
//! between requests (`plans/FIX-IO.md` `IO6d`/`IO6e`).
//!
//! # Serving and healing are the same object
//!
//! An array answers block requests through the *same* shared
//! [`serve_request_recovering`] engine a leaf device does, so it is fault-aware
//! exactly as a disk is and there is no second serve path. Between requests it
//! also heals itself: [`ArrayMaintenance`] decides, turn by turn, whether to
//! re-admit a returning member, advance a rebuild, verify the array, or record
//! where it has got to, and [`maintain`](ArrayRuntime::maintain) turns exactly
//! one of those decisions into real transfers. Both live here because both act
//! on the one composed device and must not race each other for it.
//!
//! # A pass measured in days survives a restart
//!
//! A rebuild or a verification pass over a 100 TB+ array outlives the interval
//! between reboots, so the array writes its position into every current
//! member's maintenance record as it works, and reads it back at assembly.
//! The record goes only to members the array counts as **current**: a member's
//! record then never claims a generation newer than that member's own
//! superblock, so a copy that was away can never come back carrying a position
//! for an array shape it never had.
//!
//! A rebuild that *finishes* is recorded too, by stamping the rebuilt member's
//! superblock as current. Without that the array would be whole in memory and
//! still short a copy on disk, so the next assembly would rebuild it from
//! scratch — on a large array, for hours, every restart.
//!
//! # It never blocks and never spins
//!
//! Maintenance is one bounded chunk per turn, paced by the scheduler's duty
//! share against the foreground workload, and the loop parks on
//! [`maintenance_deadline_ns`](ArrayRuntime::maintenance_deadline_ns) when
//! there is nothing to do.

use alloc::vec::Vec;

use tairix_abi::blkio::{serve_request_recovering, BlkHealth, BlkHealthState, BLK_COMPLETION_LEN};
use tairix_abi::driver::block::Block;
use tairix_abi::sysinfo::{BlkHealthTransition, MountAvailability};
use tairix_abi::time::Time64;
use tairix_abi::DriverError;
use tairix_partition::PartitionBlock;
use tairix_raid::{
    ArrayIdentity, ArrayMaintenance, MaintenanceAction, MaintenancePolicy, MemberRetry,
    MemberState, OwnedRaidArray, RaidError,
};
use tairix_raidmeta::{ArrayProgress, MaintenanceRecord};

use crate::service::{
    wrap_member, write_maintenance_record, write_superblock, MaintenanceResume, ServiceError,
};

/// What one turn of maintenance did, for the serve loop's audit trail.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MaintenanceStep {
    /// The action the scheduler chose. Never [`MaintenanceAction::Idle`] — an
    /// idle array reports no step at all.
    pub action: MaintenanceAction,
    /// What came of performing it.
    pub outcome: Result<(), RaidError>,
}

/// A change in what an array can promise its consumers, worth recording.
///
/// The first three are the shared health vocabulary every layer of the block
/// stack uses, so an array's degrade and recovery read the same as a leaf
/// disk's. Losing an array outright is not one of them: the shared classifier
/// deliberately leaves the fail-closed edge to the component that owns it, so
/// the composer reports it as its own event.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ArrayHealthEvent {
    /// The array's health changed in the shared vocabulary.
    Health(BlkHealthTransition),
    /// The array can no longer serve: too many members are gone for its level
    /// to reconstruct what they held.
    Lost,
}

/// One live array the composer serves: its identity, its owning composed
/// device, its fault-recovery health, its self-maintenance, and the ids of the
/// block-service endpoint, shared data window, and hardware-tree node it is
/// published through.
///
/// It is generic over the *raw* member device `B` (a block client) and owns
/// the composed [`OwnedRaidArray`] over each member's metadata-offset
/// [`PartitionBlock`] view, so a returning member is placed by handing the
/// runtime a raw device and letting it wrap it, never a pre-wrapped one.
pub struct ArrayRuntime<B: Block> {
    identity: ArrayIdentity,
    array: OwnedRaidArray<PartitionBlock<B>>,
    health: BlkHealth,
    maintenance: ArrayMaintenance<Vec<MemberRetry>>,
    /// What the array could last promise its consumers, so a change is
    /// reported once rather than on every turn of the serve loop.
    availability: MountAvailability,
    /// The sequence the next maintenance record written will carry.
    sequence: u64,
    /// When the array's last complete verification pass finished, as the
    /// records carry it.
    last_scrub_completed: Option<Time64>,
    endpoint: u64,
    window_id: u64,
    node_id: u32,
}

impl<B: Block> ArrayRuntime<B> {
    /// Wrap an assembled array as a live service on `endpoint` (the
    /// composer-created block-service call endpoint), `window_id` (its shared
    /// data window), and `node_id` (its published hardware-tree node),
    /// resuming the self-maintenance `resume` read from its members.
    ///
    /// The array is served with the most patient of its live members' device
    /// classes — it can only answer as fast as the member it waits on — read
    /// from the composed device itself, never an assumed envelope. That same
    /// class sets its maintenance cadences, so a rebuild paces itself to the
    /// hardware it actually runs on.
    ///
    /// # Errors
    ///
    /// * [`ServiceError::OutOfMemory`] — the per-member backoff records could
    ///   not be allocated.
    /// * [`ServiceError::Maintenance`] — the scheduler refused the array.
    pub fn new(
        identity: ArrayIdentity,
        mut array: OwnedRaidArray<PartitionBlock<B>>,
        endpoint: u64,
        window_id: u64,
        node_id: u32,
        resume: MaintenanceResume,
        now_ns: u64,
    ) -> Result<Self, ServiceError> {
        let class = array.device_class();
        let mut retries = Vec::new();
        retries
            .try_reserve(array.member_count())
            .map_err(|_| ServiceError::OutOfMemory)?;
        retries.resize(array.member_count(), MemberRetry::new());
        let policy = MaintenancePolicy::for_class(class);
        let maintenance = array
            .with_array(|view| {
                ArrayMaintenance::new(view, retries, policy, now_ns, resume.since_last_scrub_ns)
            })
            .map_err(|_| ServiceError::Maintenance)?;
        let availability = array.health().to_mount_availability();
        Ok(Self {
            identity,
            array,
            health: BlkHealth::new(class),
            maintenance,
            availability,
            sequence: resume.sequence,
            last_scrub_completed: resume.last_scrub_completed,
            endpoint,
            window_id,
            node_id,
        })
    }

    /// Serve one block request into `reply`, staging its data through
    /// `window`, and return the framed reply length.
    ///
    /// The request funnels through the shared fault-aware engine with the
    /// array's own [`BlkHealth`]: a member blip inside the recovery grace
    /// window is answered reissuably and a valid answer recovers the array,
    /// while a malformed or out-of-range request is refused health-neutrally.
    /// The array is served read/write.
    ///
    /// Every request also tells the scheduler the array is in demand, so
    /// maintenance holds to its share of a busy array instead of running flat
    /// out. A request the engine refuses counts too: the consumer is active
    /// either way, and the alternative — letting a stream of malformed
    /// requests look like an idle array — would hand maintenance bandwidth the
    /// workload is about to want back.
    #[must_use]
    pub fn serve(
        &mut self,
        request: &[u8],
        window: &mut [u8],
        reply: &mut [u8; BLK_COMPLETION_LEN],
        now_ns: u64,
    ) -> usize {
        self.maintenance.note_foreground(now_ns);
        serve_request_recovering(
            &mut self.array,
            false,
            request,
            window,
            reply,
            &mut self.health,
            now_ns,
        )
    }

    /// Perform at most one bounded chunk of self-maintenance, or [`None`] when
    /// the array has nothing to do this turn.
    ///
    /// `scratch` is the caller's shared maintenance buffer; it is used a whole
    /// number of array blocks at a time, so one buffer serves arrays of any
    /// block size. `clock` is read before and after the work, because a chunk
    /// is real I/O and the pacing is measured against how long it actually
    /// took. `now_wall` stamps a verification pass that finishes.
    ///
    /// A [`Some`] answer means work was done and more may be waiting, so the
    /// caller should come round again rather than park.
    pub fn maintain(
        &mut self,
        scratch: &mut [u8],
        now_wall: Time64,
        clock: &mut impl FnMut() -> u64,
    ) -> Option<MaintenanceStep> {
        let started_ns = clock();
        let action = {
            let Self {
                array, maintenance, ..
            } = self;
            array.with_array(|view| maintenance.next_action(view, started_ns))
        };
        if action == MaintenanceAction::Idle {
            return None;
        }
        let outcome = self.perform(action, scratch, now_wall);
        let finished_ns = clock();
        self.maintenance
            .note_step(action, started_ns, finished_ns, outcome);
        Some(MaintenanceStep { action, outcome })
    }

    /// The absolute monotonic deadline the serve loop's one-shot wait must
    /// account for: when this array's paced-out maintenance, member re-probe,
    /// or owed position write next becomes possible.
    #[must_use]
    pub fn maintenance_deadline_ns(&self) -> Option<u64> {
        self.maintenance.wait_deadline_ns()
    }

    /// Report that the member in `slot` has demonstrably returned — its agent
    /// offered the device again — so a faulted slot is re-probed without
    /// waiting out an escalated backoff. A slot that is not faulted is
    /// ignored.
    pub fn note_member_returned(&mut self, slot: u16, now_ns: u64) {
        self.maintenance
            .note_member_returned(usize::from(slot), now_ns);
    }

    /// The change in what the array can promise since this was last asked, or
    /// [`None`] while it promises the same as before.
    ///
    /// Reported through the shared block-health vocabulary, so an array
    /// degrading, rebuilding, and coming good again reads exactly as a leaf
    /// disk doing the same.
    pub fn health_event(&mut self) -> Option<ArrayHealthEvent> {
        let next = self.array.health().to_mount_availability();
        let previous = core::mem::replace(&mut self.availability, next);
        if previous == next {
            return None;
        }
        MountAvailability::health_transition(previous, next)
            .map(ArrayHealthEvent::Health)
            .or_else(|| {
                (next == MountAvailability::UnavailableLost).then_some(ArrayHealthEvent::Lost)
            })
    }

    /// Advance the recovery grace window on a pure time tick, so an array left
    /// recovering with no further request still fails closed on time off a
    /// one-shot timer rather than a busy-poll.
    #[must_use]
    pub fn poll(&mut self, now_ns: u64) -> BlkHealthState {
        self.health.poll(now_ns)
    }

    /// The array's fault-recovery health, for folding the serve loop's
    /// one-shot recovery timeout across every live array.
    #[must_use]
    pub const fn health(&self) -> &BlkHealth {
        &self.health
    }

    /// The composer-created block-service endpoint this array is served on.
    #[must_use]
    pub const fn endpoint(&self) -> u64 {
        self.endpoint
    }

    /// The shared data window forwarded to the array's published node.
    #[must_use]
    pub const fn window_id(&self) -> u64 {
        self.window_id
    }

    /// The array's published hardware-tree node id.
    #[must_use]
    pub const fn node_id(&self) -> u32 {
        self.node_id
    }

    /// The identity the array is serving at (its bumped generation for a
    /// degraded start).
    #[must_use]
    pub const fn identity(&self) -> &ArrayIdentity {
        &self.identity
    }

    /// Place a returning or late member device into a currently-absent slot of
    /// the live array, beginning its rebuild from the survivors.
    ///
    /// The device is wrapped in its metadata-offset view first, so its own
    /// superblock is never touched as array data, then installed with the
    /// composed device's own spare-insertion path.
    ///
    /// # Errors
    ///
    /// * [`ServiceError::MemberTooSmall`] / [`ServiceError::Device`] — the
    ///   device could not be wrapped.
    /// * [`ServiceError::Assembly`] — the array refused the placement (the
    ///   slot is out of range or already occupied).
    pub fn place_member(&mut self, slot: u16, raw: B) -> Result<(), ServiceError> {
        let view = wrap_member(raw)?;
        self.array
            .add_member(usize::from(slot), view)
            .map_err(|_| ServiceError::Assembly)
    }

    /// Carry out one maintenance decision against the array.
    fn perform(
        &mut self,
        action: MaintenanceAction,
        scratch: &mut [u8],
        now_wall: Time64,
    ) -> Result<(), RaidError> {
        match action {
            MaintenanceAction::Readd { member } => self.array.readd_member(member),
            MaintenanceAction::Resync => {
                let chunk = self.chunk_len(scratch.len());
                let outcome = self.array.resync_step(&mut scratch[..chunk]);
                if outcome.is_ok() && !self.array.needs_resync() {
                    return self.record_members_current(now_wall);
                }
                outcome
            }
            MaintenanceAction::BeginScrub => self.array.begin_scrub(),
            MaintenanceAction::Scrub => {
                let chunk = self.chunk_len(scratch.len());
                let outcome = self.array.scrub_step(&mut scratch[..chunk]);
                if !self.array.scrubbing() {
                    // The pass reached the end of the array. It is complete
                    // whatever this last chunk reported: a block no copy could
                    // supply is a data-loss finding, recorded by the chunk's
                    // own error, not a reason to call the array unverified.
                    self.last_scrub_completed = Some(now_wall);
                }
                outcome
            }
            MaintenanceAction::Checkpoint { progress, .. } => self.write_position(progress),
            // The caller never performs an idle turn, but the decision is the
            // scheduler's to make and this stays total rather than assuming so.
            MaintenanceAction::Idle => Ok(()),
        }
    }

    /// The usable prefix of the caller's shared scratch buffer: as many whole
    /// array blocks as fit.
    ///
    /// The chunk bounds how much of the array one maintenance turn touches, so
    /// it is deliberately independent of how large the array is — a bigger
    /// array takes more chunks, never a bigger buffer.
    fn chunk_len(&mut self, available: usize) -> usize {
        let block_size = self.array.array_geometry().block_size as usize;
        block_size
            .checked_mul(available / block_size.max(1))
            .unwrap_or(0)
            .min(available)
    }

    /// Stamp every current member's superblock at the array's generation, so a
    /// member whose rebuild has just finished is recorded as the current copy
    /// it now is.
    ///
    /// Until this lands the array is whole in memory and still short a copy on
    /// disk, so the next assembly would resolve the rebuilt member as stale and
    /// rebuild it all over again. A refusal is reported as the maintenance
    /// turn's outcome rather than hidden: the array keeps serving correctly,
    /// and the cost of not recording it is a repeated rebuild, never a stale
    /// read — an unrecorded member stays *behind* on disk, which is the safe
    /// direction.
    fn record_members_current(&mut self, now: Time64) -> Result<(), RaidError> {
        let identity = self.identity;
        self.array.with_array(|view| {
            for index in 0..view.member_count() {
                if view.member_state(index) != Some(MemberState::InSync) {
                    continue;
                }
                let Ok(slot) = u16::try_from(index) else {
                    continue;
                };
                let Some(record) = identity.member_superblock(slot, now) else {
                    continue;
                };
                let Some(member) = view.member_device_mut(index) else {
                    continue;
                };
                write_superblock(member.device_mut(), &record)
                    .map_err(|err| RaidError::Io(io_error(err)))?;
            }
            Ok(())
        })
    }

    /// Write the array's position into every current member's maintenance
    /// record.
    ///
    /// Only members the array counts as in sync are written: their superblocks
    /// carry the array's current generation, so a record never claims to
    /// describe a shape its own member was not part of. The write succeeds if
    /// any current member recorded it — the freshest copy wins at reassembly,
    /// so one is enough — and fails closed otherwise, which holds the
    /// scheduler off and leaves the position still owed.
    fn write_position(&mut self, progress: ArrayProgress) -> Result<(), RaidError> {
        let record = MaintenanceRecord::checkpoint(
            &self.identity,
            self.sequence,
            progress,
            self.last_scrub_completed,
        );
        let (written, failure) = self.array.with_array(|view| {
            let mut written = 0usize;
            let mut failure = None;
            for index in 0..view.member_count() {
                if view.member_state(index) != Some(MemberState::InSync) {
                    continue;
                }
                let Some(member) = view.member_device_mut(index) else {
                    continue;
                };
                match write_maintenance_record(member.device_mut(), &record) {
                    Ok(()) => written += 1,
                    Err(err) => failure = Some(err),
                }
            }
            (written, failure)
        });
        if written == 0 {
            return Err(RaidError::Io(failure.map_or(
                // The array has no current member left to record on.
                DriverError::DeviceOffline,
                io_error,
            )));
        }
        self.sequence = self.sequence.saturating_add(1);
        Ok(())
    }
}

/// The device-level reason behind a refused metadata write.
///
/// Anything that is not the device's own answer means the member is not one
/// this array can record on at all; it is reported as a fault so the write is
/// retried rather than counted as done.
fn io_error(err: ServiceError) -> DriverError {
    match err {
        ServiceError::Device(inner) => inner,
        _ => DriverError::DeviceFault,
    }
}

#[cfg(test)]
mod tests;
