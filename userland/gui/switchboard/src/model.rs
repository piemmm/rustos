//! Build the live [`SwitchboardModel`] the panel renders from a [`Sample`],
//! the monitor's rolling meter state and the session's [`SessionReport`],
//! and map each interactive [`SwitchboardAction`] the composed control
//! reports back onto the outbound [`Effect`]s it implies.
//!
//! This module performs none of these effects itself and never fabricates a
//! row it cannot back with a real reading. [`crate::panel`] applies the
//! effects through its host seam; this module decides *what* to do, never
//! *how*.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use tairix_abi::blkio::{BlkIoCounters, BlkQueueCounters};
use tairix_abi::display_ipc::DisplayStats;
use tairix_abi::net_ipc::NetCounters;
use tairix_abi::switchboard_ipc::{CommandSection, FrameReport, SeatReport, OWNER_BUNDLES_MAX};
use tairix_abi::sysinfo::{
    CrashAccess, CrashFaultBucket, CrashFaultClass, ProcessState, VolumeIoQueueRecord,
    VolumeIoStatsRecord,
};
use tairix_abi::{CapabilityId, CapabilityQuery, Duration64, ProcId, Signal};
use tairix_controls::{ActivityState, PressureState, RecoveryState, MAX_CHART_SAMPLES};
use tairix_theme::SignalRole;

use crate::derive::{memory_pressured, Hysteresis};
use crate::format::{format_bytes, format_duration, format_rate, percent};
use crate::resource_report::{build_resource_report, reading};
use crate::sample::{permille_of, DegradedField, ProcessSummary, Sample};
use crate::view::resources::{DeviceId, Trace};
use crate::view::{
    ActionVerdict, CrashSnapshot, FaultImpact, FaultMark, Reading, RecoveryControl, RecoveryItem,
    Section, SwitchboardAction, SwitchboardModel, TaskAuthority, TaskControl, TaskOwner,
    TaskSummary, Unmeasured,
};

/// Convert a wire [`CommandSection`] into the shared control's own
/// [`Section`] — the two are defined independently because `lib/abi` may
/// not depend on the userland control library, so the service is the one
/// place that maps between them.
#[must_use]
pub const fn map_section(command: CommandSection) -> Section {
    match command {
        CommandSection::Tasks => Section::Tasks,
        CommandSection::Resources => Section::Resources,
        CommandSection::Recovery => Section::Recovery,
    }
}

/// Narrow a sampled scheduler task id to the signed process id the
/// `signal` syscall takes, refusing rather than truncating.
///
/// A truncated id would name a *different*, arbitrary process, so an id
/// beyond the syscall's width yields [`None`] and the action is not
/// attempted at all. The kernel draws every id inside the signed range, so
/// this refuses only a sample that was already impossible.
#[must_use]
pub fn signal_pid(owner: u64) -> Option<i64> {
    i64::try_from(owner).ok()
}

/// A process name rendered as display text: valid UTF-8 with a lossy
/// replacement for anything that is not, exactly as a hover readout or a
/// row label — display text carrying no authority — is built elsewhere in
/// this crate.
pub(crate) fn display_name(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec())
        .unwrap_or_else(|_| String::from_utf8_lossy(bytes).into_owned())
}

/// A bounded series of readings, oldest first, capped at the chart's own
/// [`MAX_CHART_SAMPLES`] window.
///
/// Held inline and kept contiguous, so recording a sample never allocates, a
/// service that runs for weeks never grows an unbounded log, and a reader
/// takes the whole series as one slice.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Series {
    points: [u16; MAX_CHART_SAMPLES],
    len: usize,
}

impl Series {
    /// An empty series.
    const fn new() -> Self {
        Self {
            points: [0; MAX_CHART_SAMPLES],
            len: 0,
        }
    }

    /// Record `point`, dropping the oldest once the window is full.
    fn push(&mut self, point: u16) {
        if self.len == MAX_CHART_SAMPLES {
            self.points.copy_within(1.., 0);
            self.len -= 1;
        }
        if let Some(slot) = self.points.get_mut(self.len) {
            *slot = point;
            self.len += 1;
        }
    }

    /// The recorded readings, oldest first.
    fn points(&self) -> &[u16] {
        self.points.get(..self.len).unwrap_or(&[])
    }
}

/// The measured process population, or `None` when the list could not be read.
///
/// An unread list arrives as an empty one, and an empty machine is not a thing
/// that happens: recording its nought would plot a real population collapsing
/// to zero and back.
fn process_count(sample: &Sample) -> Option<u16> {
    (!sample.degradations.contains(&DegradedField::ProcessList))
        .then(|| u16::try_from(sample.processes.len()).unwrap_or(u16::MAX))
}

/// The rolling instrument state the panel's resource rows need that no single
/// [`Sample`] carries: the CPU and memory charts' bounded histories, and the
/// pressure verdicts the tray summary's own derivation already reached for
/// the same readings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveMeters {
    cpu: Series,
    memory: Series,
    processes: Series,
    /// The largest process population this session has seen, which is what the
    /// Tasks trace is read against.
    ///
    /// A count has no capacity to be a share of, so its box needs a stated
    /// ceiling. A high-water only ever grows, so the box means the same thing
    /// from one sample to the next — a ceiling refitted to each window would
    /// redraw the same history differently every time it rolled.
    processes_peak: u16,
    stopped: Series,
    cpu_pressured: bool,
    memory_pressured: bool,
}

impl Default for LiveMeters {
    fn default() -> Self {
        Self::new()
    }
}

impl LiveMeters {
    /// Empty histories, neither resource pressured.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cpu: Series::new(),
            memory: Series::new(),
            processes: Series::new(),
            processes_peak: 0,
            stopped: Series::new(),
            cpu_pressured: false,
            memory_pressured: false,
        }
    }

    /// Record `sample`'s readings and the CPU-pressure verdict `hysteresis`
    /// latched for the very same reading, so the panel's meter and the tray
    /// icon's rail can never disagree.
    ///
    /// Only a measured reading enters a history: an interval the service
    /// could not measure contributes no point rather than a zero one, which
    /// would plot as a genuine idle moment. The two are recorded
    /// independently, so a refused memory reading never breaks the CPU trace.
    pub fn record(&mut self, sample: &Sample, hysteresis: Hysteresis) {
        self.cpu_pressured = hysteresis.cpu_pressured();
        self.memory_pressured = memory_pressured(sample);
        if let Some(busy) = sample.cpu_busy_permille {
            self.cpu.push(busy);
        }
        if let Some(memory) = sample.memory_pressure {
            self.memory.push(memory.used_permille);
        }
        if let Some(count) = process_count(sample) {
            self.processes.push(count);
            self.processes_peak = self.processes_peak.max(count);
            // Nothing running is nothing stopped, so an empty population is a
            // nought share rather than a division by it.
            let stopped = sample.stopped_count.min(count);
            self.stopped
                .push(permille_of(u64::from(stopped), u64::from(count)).unwrap_or(0));
        }
    }

    /// The recorded CPU readings, oldest first.
    #[must_use]
    pub fn cpu_history(&self) -> &[u16] {
        self.cpu.points()
    }

    /// The recorded committed-memory shares, oldest first.
    #[must_use]
    pub fn memory_history(&self) -> &[u16] {
        self.memory.points()
    }

    /// The recorded process populations, oldest first, read against
    /// [`process_peak`](Self::process_peak).
    #[must_use]
    pub fn process_history(&self) -> &[u16] {
        self.processes.points()
    }

    /// The largest population seen this session: what the Tasks trace's box
    /// tops out at. Zero until a population has been measured.
    #[must_use]
    pub const fn process_peak(&self) -> u16 {
        self.processes_peak
    }

    /// The recorded shares of the population that were stopped, oldest first.
    #[must_use]
    pub fn stopped_history(&self) -> &[u16] {
        self.stopped.points()
    }

    /// Whether CPU pressure is latched active.
    #[must_use]
    pub const fn cpu_pressured(&self) -> bool {
        self.cpu_pressured
    }

    /// Whether the last memory reading was at or beyond the pressure band.
    #[must_use]
    pub const fn memory_pressured(&self) -> bool {
        self.memory_pressured
    }
}

/// The per-core and per-device rolling state the Resources rail and its
/// panes need that no single [`Sample`] carries: each core's own bounded
/// busy history, and each device's previous cumulative counters with the
/// rates they produce.
///
/// Keyed on the subject's own identity — a CPU index, a serving block
/// endpoint (or the volume standing in for one), an interface name — rather
/// than a rail position, so a device that appears or goes away between
/// samples can never inherit another's trace. Every entry is rebuilt from
/// the sample rather than mutated in place, so a detached device or a
/// removed interface leaks neither history nor counters.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeviceMeters {
    cores: BTreeMap<u32, Vec<u16>>,
    devices: BTreeMap<DeviceId, DeviceTrack>,
}

/// One volume's readings derived over the interval between two samples.
///
/// Every field is `None` where its own source was absent — the ungated
/// service counters, the separately-gated queue counters, or a measurable
/// interval — so a denied queue never blanks a throughput a reader may see,
/// and a first sample yields no rate at all (a cumulative total is not a
/// rate).
///
/// Derived once per sample here rather than at paint, and stated as raw
/// nanoseconds and bytes so the pane chooses only how to spell them.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct VolumeService {
    /// Bytes read per second over the interval.
    pub read_bps: Option<u64>,
    /// Bytes written per second over the interval.
    pub write_bps: Option<u64>,
    /// Read requests completed per second.
    pub read_iops: Option<u64>,
    /// Write requests completed per second.
    pub write_iops: Option<u64>,
    /// The share of the interval the device had a request outstanding.
    pub utilisation_permille: Option<u16>,
    /// Mean issue-to-completion time of the interval's reads.
    pub read_await_ns: Option<u64>,
    /// Mean issue-to-completion time of the interval's writes.
    pub write_await_ns: Option<u64>,
    /// Mean device-busy time per read or write completed in the interval —
    /// what the device itself spent, where the awaits above include queueing.
    /// A data-less operation the interval also carried (a flush) belongs to
    /// neither direction, so it is not in the divisor.
    pub service_ns: Option<u64>,
    /// Mean queue depth an arriving request met, in hundredths of a
    /// request.
    pub mean_depth_centi: Option<u64>,
    /// Requests outstanding at the sample instant.
    pub in_flight: Option<u64>,
    /// The depth the device's own class permits, so the occupancy above is
    /// read against the ceiling that applies to its medium.
    pub budget_depth: Option<u32>,
    /// The per-request deadline the device's class is served with.
    pub budget_deadline_ns: Option<u64>,
}

/// One device's two cumulative directional byte counters as of a sample.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
struct DirectedBytes {
    /// Bytes in the primary direction: received, or read.
    primary: u64,
    /// Bytes in the opposing direction: sent, or written.
    opposing: u64,
}

/// What one device's tracking holds between samples: the cumulative
/// counters the next sample deltas against, and the rates those produced.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct DeviceTrack {
    /// The device's cumulative byte counters as of the last sample, or
    /// [`None`] where that sample carried none.
    ///
    /// An absence **breaks the series**: the next sample that does carry
    /// them is a first sample again and yields no rate, rather than deltaing
    /// against a counter from an interval other than the one it would divide
    /// by.
    bytes: Option<DirectedBytes>,
    /// The primary direction's recent rates, oldest first.
    primary_history: Vec<u16>,
    /// The opposing direction's recent rates, oldest first.
    opposing_history: Vec<u16>,
    /// The primary direction's latest rate, in bytes per second.
    primary_rate: Option<u64>,
    /// The opposing direction's latest rate, in bytes per second.
    opposing_rate: Option<u64>,
    /// A volume's whole cumulative service counters as of the last sample,
    /// which the awaits and utilisation delta against. [`None`] for a device
    /// that serves none (an interface, the compositor).
    volume_io: Option<BlkIoCounters>,
    /// The graphics device's cumulative busy nanoseconds as of the last
    /// sample, which its utilisation deltas against, or [`None`] where that
    /// sample carried none.
    ///
    /// An absence **breaks the series** for the same reason the byte
    /// counters' does: deltaing a fresh reading against a stale one would
    /// report a whole service lifetime's occupancy as one interval's share.
    gpu_busy_ns: Option<u64>,
    /// The share of the last interval the graphics device was busy.
    gpu_busy_permille: Option<u16>,
    /// A volume's cumulative queue accumulators as of the last sample, which
    /// the mean depth deltas against.
    volume_queue: Option<BlkQueueCounters>,
    /// The volume readings the last fold derived.
    service: VolumeService,
}

/// The rate a device's trace is plotted against, in bytes per second.
///
/// A rate has no ceiling of its own to fill a bar against, so a trace needs
/// a reference to be drawn in permille at all. One shared reference across
/// every device is what makes two rail traces comparable by eye, which is
/// the whole point of a rail of them; a device faster than this plots at the
/// top of its box rather than past it.
const TRACE_FULL_SCALE_BYTES: u64 = 1_000_000_000;

impl DeviceMeters {
    /// Nothing measured yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cores: BTreeMap::new(),
            devices: BTreeMap::new(),
        }
    }

    /// Fold `sample`'s per-core shares in, dropping any core it does not
    /// name.
    pub fn record_cores(&mut self, sample: &Sample) {
        let mut next = BTreeMap::new();
        for core in &sample.core_busy {
            let mut history = self.cores.remove(&core.cpu).unwrap_or_default();
            // An interval the service could not measure contributes no
            // point rather than a zero one, which would plot as a genuine
            // idle moment.
            if let Some(permille) = core.permille {
                push_bounded(&mut history, permille);
            }
            next.insert(core.cpu, history);
        }
        self.cores = next;
    }

    /// One core's recorded busy shares, oldest first.
    #[must_use]
    pub fn core_history(&self, cpu: u32) -> &[u16] {
        self.cores.get(&cpu).map_or(&[], Vec::as_slice)
    }

    /// Fold one interface's cumulative received/sent byte counters in.
    ///
    /// A device first seen this sample, and an interval the service could
    /// not measure, each yield no rate: a cumulative total is not a rate.
    pub fn record_interface(
        &mut self,
        id: DeviceId,
        counters: Option<NetCounters>,
        elapsed_ns: Option<u64>,
    ) {
        let (previous, mut track) = self.take(id);
        Self::fold_bytes(
            &mut track,
            previous.as_ref(),
            counters.map(|counters| DirectedBytes {
                primary: counters.rx_bytes,
                opposing: counters.tx_bytes,
            }),
            elapsed_ns,
        );
        self.devices.insert(id, track);
    }

    /// Fold one storage device's cumulative service and queue counters in,
    /// deriving its whole interval reading.
    ///
    /// Called once per device: the counters belong to the device rather than
    /// to a mount, so folding them again in the same sample would delta them
    /// against themselves and derive a nought rate.
    ///
    /// The two counter blocks are separately gated, so either may be absent
    /// without costing the other its reading; a device the sample names with
    /// neither still gets its entry, and simply reads unmeasured.
    pub fn record_volume(
        &mut self,
        id: DeviceId,
        io: Option<&VolumeIoStatsRecord>,
        queue: Option<&VolumeIoQueueRecord>,
        elapsed_ns: Option<u64>,
    ) {
        let (previous, mut track) = self.take(id);
        let io = io.map(VolumeIoStatsRecord::counters);
        Self::fold_bytes(
            &mut track,
            previous.as_ref(),
            io.map(|counters| DirectedBytes {
                primary: counters.read_bytes,
                opposing: counters.write_bytes,
            }),
            elapsed_ns,
        );
        track.service = VolumeService {
            read_bps: track.primary_rate,
            write_bps: track.opposing_rate,
            ..VolumeService::default()
        };
        if let (Some(io), Some(earlier)) = (io, previous.as_ref().and_then(|prev| prev.volume_io)) {
            fold_service(&mut track.service, &io, &earlier, elapsed_ns);
        }
        if let Some(record) = queue {
            let queue = record.queue();
            track.service.in_flight = Some(queue.in_flight);
            track.service.budget_depth = Some(record.budget_depth());
            track.service.budget_deadline_ns = Some(record.budget_deadline_ns());
            track.service.mean_depth_centi = previous
                .as_ref()
                .and_then(|prev| prev.volume_queue)
                .and_then(|earlier| queue.mean_depth_centi(&earlier));
        }
        track.volume_io = io;
        track.volume_queue = queue.map(VolumeIoQueueRecord::queue);
        self.devices.insert(id, track);
    }

    /// Fold the compositor's last reported frame in as one point of the
    /// display path's trace.
    ///
    /// The point is the frame's *damaged* pixels as a permille of that same
    /// frame's screen — the only full scale a per-frame pixel count has, and
    /// the one the pane's own context line already states the reading
    /// against, so the trace and the words cannot disagree. It is not a
    /// rate, so an absent report breaks nothing: it contributes no point,
    /// exactly as an unmeasured core does, rather than plotting a nought
    /// that would read as an idle frame.
    pub fn record_graphics(
        &mut self,
        id: DeviceId,
        frame: Option<FrameReport>,
        stats: Option<&DisplayStats>,
        elapsed_ns: Option<u64>,
    ) {
        let (previous, mut track) = self.take(id);
        if let Some(frame) = frame {
            // A screen of no pixels has no frame to divide by; the report's
            // own validation already keeps the damage inside it.
            let share = frame.damaged_px.saturating_mul(1_000) / frame.screen_px.max(1);
            push_bounded(&mut track.primary_history, clamp_permille(share));
        }
        let busy_ns = stats.map(|stats| stats.busy_ns);
        track.gpu_busy_permille = match (
            busy_ns,
            previous.as_ref().and_then(|prev| prev.gpu_busy_ns),
            elapsed_ns,
        ) {
            (Some(now), Some(earlier), Some(interval)) => {
                permille_of(now.saturating_sub(earlier), interval)
            }
            // A first sample, a sample that carried no reading, and a sample
            // after one that carried none each yield no share: a cumulative
            // total is not a share.
            _ => None,
        };
        track.gpu_busy_ns = busy_ns;
        self.devices.insert(id, track);
    }

    /// The share of the last interval this graphics device was busy, absent
    /// where the interval could not be measured.
    #[must_use]
    pub fn graphics_busy(&self, id: DeviceId) -> Option<u16> {
        self.devices
            .get(&id)
            .and_then(|track| track.gpu_busy_permille)
    }

    /// This storage device's derived interval readings, all-absent for a
    /// device the sample carried no counters for.
    #[must_use]
    pub fn volume_service(&self, id: DeviceId) -> VolumeService {
        self.devices
            .get(&id)
            .map_or_else(VolumeService::default, |track| track.service)
    }

    /// Lift `id`'s previous track out of the map, with a fresh clone of it to
    /// fold this sample into — so an entry is rebuilt rather than mutated and
    /// a device the caller stops recording leaves nothing behind.
    fn take(&mut self, id: DeviceId) -> (Option<DeviceTrack>, DeviceTrack) {
        let previous = self.devices.remove(&id);
        let track = previous.clone().unwrap_or_default();
        (previous, track)
    }

    /// Fold this sample's directional byte counters in: the rates they
    /// produce and the bounded traces those rates plot.
    ///
    /// The one definition of that fold, so a volume's trace and an
    /// interface's are the same arithmetic against the same full scale and
    /// stay comparable by eye. A rate needs both ends of an interval, so a
    /// device first seen this sample, a sample that carried no counters, and
    /// a sample after one that carried none each yield no rate and contribute
    /// no trace point — never a delta against a gap.
    fn fold_bytes(
        track: &mut DeviceTrack,
        previous: Option<&DeviceTrack>,
        now: Option<DirectedBytes>,
        elapsed_ns: Option<u64>,
    ) {
        let earlier = previous.and_then(|prev| prev.bytes);
        let rate = |select: fn(DirectedBytes) -> u64| {
            let (now, earlier) = (now?, earlier?);
            rate_per_sec(select(now), select(earlier), elapsed_ns)
        };
        track.primary_rate = rate(|bytes| bytes.primary);
        track.opposing_rate = rate(|bytes| bytes.opposing);
        track.bytes = now;
        if let Some(rate) = track.primary_rate {
            push_bounded(&mut track.primary_history, trace_permille(rate));
        }
        if let Some(rate) = track.opposing_rate {
            push_bounded(&mut track.opposing_history, trace_permille(rate));
        }
    }

    /// Drop every device the caller did not record this sample, so an
    /// unmounted volume or a removed interface leaves nothing behind.
    pub fn retain_recorded(&mut self, recorded: &[DeviceId]) {
        self.devices.retain(|id, _| recorded.contains(id));
    }

    /// One device's recorded primary-direction rates, oldest first.
    #[must_use]
    pub fn primary_history(&self, id: DeviceId) -> &[u16] {
        self.devices
            .get(&id)
            .map_or(&[], |track| track.primary_history.as_slice())
    }

    /// One device's recorded opposing-direction rates, oldest first.
    #[must_use]
    pub fn opposing_history(&self, id: DeviceId) -> &[u16] {
        self.devices
            .get(&id)
            .map_or(&[], |track| track.opposing_history.as_slice())
    }
}

/// Derive one volume's IOPS, utilisation, awaits and service time from the
/// cumulative counters either side of an interval.
///
/// Each is the delta ratio the counters were designed for, so no consumer
/// inherits another's averaging window. A ratio with no denominator — an
/// unmeasurable interval, an interval in which nothing completed — is left
/// absent rather than reported as nought, which would read as a fast device
/// when the truth is an idle one.
fn fold_service(
    service: &mut VolumeService,
    now: &BlkIoCounters,
    earlier: &BlkIoCounters,
    elapsed_ns: Option<u64>,
) {
    let reads = now.read_ops.saturating_sub(earlier.read_ops);
    let writes = now.write_ops.saturating_sub(earlier.write_ops);
    service.read_iops = rate_per_sec(now.read_ops, earlier.read_ops, elapsed_ns);
    service.write_iops = rate_per_sec(now.write_ops, earlier.write_ops, elapsed_ns);
    service.utilisation_permille = elapsed_ns
        .and_then(|interval| permille_of(now.busy_ns.saturating_sub(earlier.busy_ns), interval));
    service.read_await_ns = mean(now.read_wait_ns.saturating_sub(earlier.read_wait_ns), reads);
    service.write_await_ns = mean(
        now.write_wait_ns.saturating_sub(earlier.write_wait_ns),
        writes,
    );
    service.service_ns = mean(
        now.busy_ns.saturating_sub(earlier.busy_ns),
        reads.saturating_add(writes),
    );
}

/// `total` shared over `count`, or [`None`] where nothing was counted.
const fn mean(total: u64, count: u64) -> Option<u64> {
    if count == 0 {
        return None;
    }
    Some(total / count)
}

/// A byte rate as the permille of [`TRACE_FULL_SCALE_BYTES`] its trace plots
/// at, clamped at full rather than wrapping past the box.
fn trace_permille(bytes_per_sec: u64) -> u16 {
    clamp_permille(bytes_per_sec.saturating_mul(1_000) / TRACE_FULL_SCALE_BYTES)
}

/// A computed share as a chart permille, clamped at full rather than wrapping
/// past the box — the one clamp every trace point passes through.
fn clamp_permille(permille: u64) -> u16 {
    u16::try_from(permille.min(1_000)).unwrap_or(1_000)
}

/// Append `value` to a bounded history, dropping the oldest point once the
/// chart's own window is full.
fn push_bounded(history: &mut Vec<u16>, value: u16) {
    if history.len() >= MAX_CHART_SAMPLES {
        history.remove(0);
    }
    history.push(value);
}

/// The number of CPU readings kept per task for its row's sparkline.
///
/// The same window the resource charts plot, so a per-task spark and the
/// system CPU chart beside it cover the same span of time rather than two
/// arbitrary ones.
pub const TASK_HISTORY_LEN: usize = MAX_CHART_SAMPLES;

/// What one task's tracking holds between samples: the previous reading of
/// its own storage counters, and its recent CPU readings.
///
/// The counters are cumulative-since-start, so a *rate* needs the previous
/// reading and the interval between the two; the history is the row's own
/// sparkline, which no single sample carries.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TaskTrack {
    /// The task's total storage bytes (read plus written) as of the last
    /// recorded sample, which the next sample deltas against.
    io_bytes: u64,
    /// The rate that delta produced, in bytes per second, or `None` when
    /// this sample had nothing honest to divide.
    disk_bytes_per_sec: Option<u64>,
    /// Recent CPU readings, oldest first, capped at [`TASK_HISTORY_LEN`].
    cpu_history: Vec<u16>,
}

/// The per-task rolling state the task rows need that no single [`Sample`]
/// carries: each task's previous storage-counter reading (for an honest
/// bytes-per-second rate) and its own bounded CPU history (for the row's
/// sparkline).
///
/// Keyed by [`ProcId`] — the never-reused process identity — rather than by
/// pid or row index, so a recycled pid can never inherit a dead task's
/// history and a re-sorted or filtered table can never mis-attribute one.
///
/// Every entry is dropped the first sample its task is absent from
/// ([`Self::record`] rebuilds the map from the sample rather than mutating
/// it in place), so a machine that churns short-lived processes for weeks
/// accumulates nothing: the map is exactly as large as the live process
/// list, and each entry holds at most [`TASK_HISTORY_LEN`] readings.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskMeters {
    tracks: BTreeMap<ProcId, TaskTrack>,
}

impl TaskMeters {
    /// No tasks tracked yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tracks: BTreeMap::new(),
        }
    }

    /// Fold `sample`'s process list in: delta each task's storage counters
    /// against its previous reading, extend its CPU history, and keep only
    /// the tasks this sample names.
    ///
    /// This runs *before* the rows are built, so a row reads the figures
    /// this sample produced rather than the last one's — and the delta is
    /// taken against the stored previous reading before that reading is
    /// replaced, so the two can never be confused.
    ///
    /// A task's CPU reading enters its history only when the sample
    /// measured one: an unmeasurable interval contributes no point rather
    /// than a zero one, which would plot as a genuine idle moment.
    pub fn record(&mut self, sample: &Sample) {
        let mut next = BTreeMap::new();
        for process in &sample.processes {
            let io_bytes = process
                .io_bytes_read
                .saturating_add(process.io_bytes_written);
            let previous = self.tracks.remove(&process.proc_id);
            let disk_bytes_per_sec = previous
                .as_ref()
                .and_then(|track| rate_per_sec(io_bytes, track.io_bytes, sample.elapsed_ns));
            let mut cpu_history = previous.map_or_else(Vec::new, |track| track.cpu_history);
            if let Some(permille) = process.cpu_permille {
                if cpu_history.len() >= TASK_HISTORY_LEN {
                    cpu_history.remove(0);
                }
                cpu_history.push(permille);
            }
            next.insert(
                process.proc_id,
                TaskTrack {
                    io_bytes,
                    disk_bytes_per_sec,
                    cpu_history,
                },
            );
        }
        self.tracks = next;
    }

    /// The task's recorded CPU readings, oldest first; empty for a task
    /// that has never been measured.
    #[must_use]
    pub fn cpu_history(&self, proc_id: ProcId) -> &[u16] {
        self.tracks
            .get(&proc_id)
            .map_or(&[][..], |track| track.cpu_history.as_slice())
    }

    /// The task's storage throughput as of the last [`Self::record`], in
    /// bytes per second.
    ///
    /// `None` — which the row renders as an explicit unmeasured mark, never
    /// a zero — when there was nothing honest to divide: the very first
    /// sample (no interval to measure over), a task appearing for the first
    /// time (no previous reading *of this task*), or a task this sample did
    /// not name at all. A counter that did not move over a real interval is
    /// a genuine measurement of `0` bytes per second and is reported as
    /// such.
    #[must_use]
    pub fn disk_rate(&self, proc_id: ProcId) -> Option<u64> {
        self.tracks.get(&proc_id)?.disk_bytes_per_sec
    }
}

/// When each faulted task was first observed to be faulted, so the surface
/// can say how long a fault has stood.
///
/// Nothing in the System Information API records *when* a task entered its
/// current state: a process record carries the state but no transition
/// timestamp. The only honest way to say "stopped 4m ago" is therefore for
/// this service to note the first sample it saw the fault in and measure
/// from there — which is what this does, clocked off the monotonic uptime
/// reading rather than a wall clock that an administrator can move.
///
/// Keyed by [`ProcId`] for the same reason [`TaskMeters`] is: a recycled pid
/// must never inherit a dead task's fault age. Entries are dropped the first
/// sample their task is no longer faulted, so a machine that churns faults
/// accumulates nothing and a task that recovers and faults again is timed
/// from its *new* fault, not its old one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FaultClock {
    first_seen: BTreeMap<ProcId, Duration64>,
    resolved: usize,
}

impl FaultClock {
    /// No faults observed yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            first_seen: BTreeMap::new(),
            resolved: 0,
        }
    }

    /// Note which tasks are faulted as of `sample`, keeping the instant
    /// each was *first* seen faulted and forgetting the rest.
    ///
    /// With no uptime reading there is no clock to measure against, so no
    /// instant is recorded and every age reads as unmeasured — never a
    /// fabricated zero. The map is still pruned in that case, so a stale
    /// instant cannot outlive the fault it belonged to.
    pub fn record(&mut self, sample: &Sample, seat_report: &SeatReport) {
        let now = sample.uptime.map(|uptime| uptime.since_boot);
        let mut next = BTreeMap::new();
        for process in &sample.processes {
            if process_recovery(process, seat_report) == RecoveryState::None {
                continue;
            }
            let Some(now) = now else {
                continue;
            };
            let first = self
                .first_seen
                .get(&process.proc_id)
                .copied()
                .unwrap_or(now);
            next.insert(process.proc_id, first);
        }
        let cleared = self
            .first_seen
            .keys()
            .filter(|proc_id| !next.contains_key(*proc_id))
            .count();
        self.resolved = self.resolved.saturating_add(cleared);
        self.first_seen = next;
    }

    /// How many faults this service has watched clear since it started.
    ///
    /// A fault that is gone from a sample it was in last time has cleared,
    /// which only the thing that folds one sample into the next can see. It
    /// is counted here rather than in the screen so that a screen refreshed
    /// with a model and a screen built from that same model are the same
    /// screen, and so a count of *observed* history is never mistaken for a
    /// figure the kernel reported.
    #[must_use]
    pub const fn resolved(&self) -> usize {
        self.resolved
    }

    /// How long the task has been faulted as of `now`, or `None` when no
    /// instant was ever recorded for it (no uptime reading to measure
    /// against, or a task that is not faulted).
    #[must_use]
    pub fn elapsed(&self, proc_id: ProcId, now: Option<Duration64>) -> Option<Duration64> {
        elapsed_since(self.first_seen.get(&proc_id).copied(), now)
    }
}

/// The duration between `first` and `now`, or `None` when either is
/// unmeasured or the clock appears to have moved backwards — which a
/// monotonic reading should never do — so a bad pair yields no duration
/// rather than a negative or wrapped one.
///
/// The one interval arithmetic every band-start clock on this screen
/// shares ([`FaultClock`] and [`PressureClock`]), so a fault's age and a
/// pressure cause's age can never silently diverge on how "how long" is
/// computed.
fn elapsed_since(first: Option<Duration64>, now: Option<Duration64>) -> Option<Duration64> {
    let first = first?;
    let now = now?;
    let (now, first) = (now.saturating_total_nanos(), first.saturating_total_nanos());
    (now >= first).then(|| Duration64::from_nanos(now.saturating_sub(first)))
}

/// When each pressured resource last entered its current pressure band, so
/// the Pressure section can say how long a cause has stood rather than
/// guessing.
///
/// The System Information API reports only whether a resource is pressured
/// right now, not when it became so, so — exactly as [`FaultClock`] does
/// for a standing task fault — this service notes the first sample it saw
/// a resource cross into pressure and measures from there. There are only
/// ever two resources this section flags (CPU, memory), so a fixed pair of
/// slots serves the purpose [`FaultClock`]'s map serves for an unbounded
/// set of tasks; a resource that leaves its band drops its instant
/// immediately, so a later re-entry is timed from its *new* start, not its
/// old one.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PressureClock {
    cpu_since: Option<Duration64>,
    memory_since: Option<Duration64>,
}

impl PressureClock {
    /// Neither resource pressured yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cpu_since: None,
            memory_since: None,
        }
    }

    /// Note this sample's latched pressure verdicts, entering or clearing
    /// each resource's band-start instant.
    ///
    /// With no uptime reading there is no clock to measure against, so a
    /// resource pressured this sample without one recorded before stays
    /// without a start instant and its age reads as unmeasured — never a
    /// fabricated zero.
    pub fn record(&mut self, cpu_pressured: bool, memory_pressured: bool, now: Option<Duration64>) {
        Self::latch(&mut self.cpu_since, cpu_pressured, now);
        Self::latch(&mut self.memory_since, memory_pressured, now);
    }

    /// Enter or clear one resource's band-start instant.
    fn latch(slot: &mut Option<Duration64>, pressured: bool, now: Option<Duration64>) {
        if !pressured {
            *slot = None;
        } else if slot.is_none() {
            *slot = now;
        }
    }

    /// How long CPU pressure has stood as of `now`, or `None` when CPU is
    /// not pressured or no start instant was ever recorded for it.
    #[must_use]
    pub fn cpu_elapsed(&self, now: Option<Duration64>) -> Option<Duration64> {
        elapsed_since(self.cpu_since, now)
    }

    /// How long memory pressure has stood as of `now`, under the same
    /// conditions as [`Self::cpu_elapsed`].
    #[must_use]
    pub fn memory_elapsed(&self, now: Option<Duration64>) -> Option<Duration64> {
        elapsed_since(self.memory_since, now)
    }
}

/// Which application bundle each window owner was launched from, as the
/// desktop session has reported it.
///
/// The kernel's process record carries a name and no image path, and the
/// launch that produced a process is the session's own, so this is a fact
/// only the session can state. It accumulates one owner at a time as
/// applications launch, rather than arriving as a whole roster, and is pruned
/// against each sample's own process list — so it cannot outgrow the
/// machine's live processes, and a numeric pid recycled onto a new process
/// cannot inherit a picture, because owners are keyed by the kernel-attested
/// identity that is never reused.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OwnerBundles {
    by_owner: BTreeMap<ProcId, String>,
}

impl OwnerBundles {
    /// Nothing reported yet: every row draws its class icon.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `owner` was launched from the bundle directory `dir`,
    /// replacing anything held for it.
    ///
    /// Refused once [`OWNER_BUNDLES_MAX`] owners are already held and this is
    /// a new one, so a stream of reports arriving faster than the prune cannot
    /// grow this without bound; the refused owner draws its class icon, which
    /// is the honest degradation rather than a wrong picture.
    pub fn record(&mut self, owner: ProcId, dir: &str) {
        if self.by_owner.len() >= OWNER_BUNDLES_MAX && !self.by_owner.contains_key(&owner) {
            return;
        }
        self.by_owner.insert(owner, String::from(dir));
    }

    /// Drop every owner `live` does not name — the sample's own process list,
    /// so the set follows the machine rather than the history of it.
    ///
    /// Walked process-first rather than owner-first: a busy machine's process
    /// list is thousands of rows and the retained set is at most
    /// [`OWNER_BUNDLES_MAX`], so one pass looking each process up costs a
    /// lookup per process instead of a whole scan of the list per owner.
    pub fn retain_live(&mut self, live: &[ProcessSummary]) {
        if self.by_owner.is_empty() {
            return;
        }
        let mut seen = BTreeSet::new();
        for process in live {
            if self.by_owner.contains_key(&process.proc_id) {
                seen.insert(process.proc_id);
            }
        }
        self.by_owner.retain(|owner, _| seen.contains(owner));
    }

    /// The bundle directory `owner` was launched from, if one was reported.
    #[must_use]
    pub fn of(&self, owner: ProcId) -> Option<&str> {
        self.by_owner.get(&owner).map(String::as_str)
    }

    /// How many owners are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_owner.len()
    }

    /// Whether nothing has been reported.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_owner.is_empty()
    }
}

/// Everything the desktop session has told this instance about itself: the
/// seat's unresponsive-owner report and what its last composited frame cost.
///
/// Neither can be sampled here — the session is the party whose deliveries
/// are refused and the only one that owns a compositor — so both arrive as
/// commands on the instance's mailbox. Held as one value so the panel keeps
/// a single "latest from the session", and a rebuild triggered by one of
/// them still renders the other as last reported.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct SessionReport {
    /// The seat's latest unresponsive-owner report.
    pub seat: SeatReport,
    /// What the session's last composited frame cost, or `None` while it has
    /// reported no frame yet.
    pub frame: Option<FrameReport>,
}

impl SessionReport {
    /// Nothing reported yet: every owner responsive, no frame measured.
    pub const HEALTHY: Self = Self {
        seat: SeatReport::HEALTHY,
        frame: None,
    };
}

/// Everything the monitor carries *between* samples — every reading that no
/// single [`Sample`] can produce on its own.
///
/// A rate is a difference over an interval and a history is a sequence, so
/// neither can be read from one sample: both are folded forward from the
/// previous cycle. Keeping the whole-system meters and the per-task meters
/// as one thing means they are always folded forward from the *same*
/// sample, so a task's rate and the system chart beside it can never end up
/// describing two different intervals.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RollingMeters {
    /// The whole-system readings: the CPU chart's history and the latched
    /// pressure verdicts.
    pub system: LiveMeters,
    /// The per-task readings, keyed by the task's never-reused identity.
    pub tasks: TaskMeters,
    /// When each standing fault was first observed, so its age is measured
    /// rather than guessed.
    pub faults: FaultClock,
    /// When each pressured resource entered its current band, so a resource
    /// pane's "how long" is measured rather than guessed.
    pub pressure: PressureClock,
    /// The per-core and per-device readings the Resources rail and its panes
    /// trace, keyed on each subject's own identity.
    pub devices: DeviceMeters,
}

impl RollingMeters {
    /// Nothing measured yet: no history either side, no pressure latched.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            system: LiveMeters::new(),
            tasks: TaskMeters::new(),
            faults: FaultClock::new(),
            pressure: PressureClock::new(),
            devices: DeviceMeters::new(),
        }
    }

    /// Fold `sample` in on every side at once, with `hysteresis` the
    /// pressure verdict latched for that very reading.
    ///
    /// The one fold, once per sample, so every trace's x-axis is time — which
    /// is what the chart's fixed window and its `now` edge claim. Folding
    /// where a *report* arrives would advance a trace on events instead.
    ///
    /// Called before the rows are built, so every figure a row shows came
    /// from this sample rather than the last one. A task or device the sample
    /// does not name is dropped here, so neither leaks its counters or its
    /// history.
    pub fn record(&mut self, sample: &Sample, hysteresis: Hysteresis, session: &SessionReport) {
        self.system.record(sample, hysteresis);
        self.tasks.record(sample);
        self.devices.record_cores(sample);
        self.faults.record(sample, &session.seat);
        let now = sample.uptime.map(|uptime| uptime.since_boot);
        self.pressure.record(
            self.system.cpu_pressured(),
            self.system.memory_pressured(),
            now,
        );
        self.record_devices(sample, session);
    }

    /// Fold this sample's per-device counters: each storage device, each
    /// managed interface, and the display path.
    ///
    /// The device set is *discovered* from the sample, and one no longer named
    /// is retired, so a removed disk or interface leaks neither its counters
    /// nor its trace.
    fn record_devices(&mut self, sample: &Sample, session: &SessionReport) {
        let mut recorded = alloc::vec![
            DeviceId::Cpu,
            DeviceId::Memory,
            DeviceId::Identity,
            DeviceId::Sessions,
            DeviceId::Authority,
        ];
        // Grouped into devices before anything is folded: the counters belong
        // to the device, so a volume projected at several mount points and
        // several volumes on one disk each fold exactly once. Folding per
        // mount deltas a device's counters against themselves.
        for subject in crate::resource_report::storage_subjects(sample) {
            let id = subject.device_id();
            let key = subject.key();
            self.devices.record_volume(
                id,
                crate::resource_report::find_volume_stats(sample.volume_io_stats.as_deref(), &key),
                crate::resource_report::find_volume_stats(sample.volume_io_queue.as_deref(), &key),
                sample.elapsed_ns,
            );
            recorded.push(id);
        }
        for iface in sample.net_facts.iter().flatten() {
            let id = DeviceId::Interface(iface.name);
            // Recorded even when the counters are absent, so a sample that
            // could not read them breaks the series: the next one that can is
            // a first sample again rather than a delta over a gap.
            self.devices.record_interface(
                id,
                sample
                    .net_counters
                    .as_ref()
                    .and_then(|records| records.iter().find(|r| r.name == iface.name))
                    .map(|record| record.counters),
                sample.elapsed_ns,
            );
            recorded.push(id);
        }
        self.devices.record_graphics(
            DeviceId::Graphics,
            session.frame,
            sample
                .gpu_stats
                .as_ref()
                .and_then(|records| records.first()),
            sample.elapsed_ns,
        );
        recorded.push(DeviceId::Graphics);
        self.devices.retain_recorded(&recorded);
    }
}

/// Nanoseconds in one second, the scale a per-second rate is derived at.
const NANOS_PER_SEC: u64 = 1_000_000_000;

/// A per-second rate from two readings of one cumulative counter and the
/// interval between them, or `None` when the interval is unmeasured or
/// zero (there is nothing to divide by).
///
/// A counter that went *backwards* — which a cumulative counter should
/// never do — yields `0` rather than a wrapped, enormous rate: the delta
/// saturates, so a bad reading understates rather than invents. The
/// multiplication is done in [`u128`] so a fast device over a short
/// interval cannot overflow before the division brings it back in range.
fn rate_per_sec(now: u64, previous: u64, elapsed_ns: Option<u64>) -> Option<u64> {
    let elapsed_ns = elapsed_ns.filter(|ns| *ns > 0)?;
    let delta = u128::from(now.saturating_sub(previous));
    let per_sec = delta.saturating_mul(u128::from(NANOS_PER_SEC)) / u128::from(elapsed_ns);
    Some(u64::try_from(per_sec).unwrap_or(u64::MAX))
}

/// The identity backing one rendered task row, for the grouping actions a
/// task index alone cannot resolve.
#[derive(Clone, Debug, Eq, PartialEq)]
struct TaskIdent {
    /// The row's never-reused process identity.
    proc_id: ProcId,
    /// The row's scheduler task id.
    pid: u64,
    /// The row's validated display name.
    name: String,
}

/// The live [`SwitchboardModel`] plus the per-row identity the model's own
/// row indices do not carry, so [`apply_action`] can resolve a reported
/// [`SwitchboardAction`] back to the scheduler task id(s) or activity it
/// names.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PanelModel {
    /// The model the panel renders.
    pub model: SwitchboardModel,
    /// `task_owners[i]` is the pid backing `model.tasks[i]`.
    task_owners: Vec<u64>,
    /// `recovery_owners[i]` is the pid backing `model.recovery[i]`.
    recovery_owners: Vec<u64>,
    /// `task_idents[i]` is the identity backing `model.tasks[i]`.
    task_idents: Vec<TaskIdent>,
}

impl PanelModel {
    /// The pid backing `model.tasks[index]`, or `None` for an out-of-range
    /// index (fail closed — never a guess).
    #[must_use]
    pub fn task_owner(&self, index: usize) -> Option<u64> {
        self.task_owners.get(index).copied()
    }

    /// The pid backing `model.recovery[index]`, or `None` for an
    /// out-of-range index (fail closed).
    #[must_use]
    pub fn recovery_owner(&self, index: usize) -> Option<u64> {
        self.recovery_owners.get(index).copied()
    }

    /// The `(proc_id, pid, name)` backing `model.tasks[index]`, or `None`
    /// for an out-of-range index (fail closed).
    #[must_use]
    pub fn task_ident(&self, index: usize) -> Option<(ProcId, u64, &str)> {
        self.task_idents
            .get(index)
            .map(|ident| (ident.proc_id, ident.pid, ident.name.as_str()))
    }
}

/// Build the live [`PanelModel`] from this sample, the monitor's
/// [`RollingMeters`] and what the session has reported ([`SessionReport`]).
///
/// `home` is the account root this service runs as, if it has one: a task
/// loaded from that user's own program store draws its bundle's icon from it.
///
/// `meters` must already have this sample folded in, so the rows carry this
/// cycle's rates and histories rather than the previous cycle's; they are
/// taken mutably for the fault ledger alone, which counts a fault clearing as
/// it builds the recovery rows. `authority` is queried once per
/// action kind that needs it, so the rendered [`SwitchboardModel`] and the
/// effect [`apply_action`] later produces from the same authority can never
/// disagree about whether an action is available.
#[must_use]
pub fn build_model(
    title: &str,
    home: Option<&str>,
    sample: &Sample,
    session: &SessionReport,
    bundles: &OwnerBundles,
    meters: &mut RollingMeters,
    authority: &dyn CapabilityQuery,
) -> PanelModel {
    let mut model = SwitchboardModel::new(title).with_home(home.map(alloc::string::String::from));
    let can_force = authority.holds(CapabilityId::PROC_CONTROL);

    let (tasks, task_owners, task_idents) = build_tasks(
        &sample.processes,
        &session.seat,
        &meters.tasks,
        bundles,
        can_force,
    );
    let (recovery, recovery_owners) = build_recovery(sample, &session.seat, meters, can_force);
    let resources = build_resource_report(sample, meters, bundles, session, authority);

    model.tasks = tasks;
    model.recovery = recovery;
    model.recovery_resolved = meters.faults.resolved();
    // A count of tasks is what the machine is *running*, not compute
    // saturation: drawn in the compute hue it read as a second CPU trace
    // beside the real one. A count has no capacity of its own, so it carries
    // the denominator its box is read against.
    model.tasks_trend = Trace::counted(
        SignalRole::Workload,
        meters.system.process_history().to_vec(),
        meters.system.process_peak(),
    );
    // The stopped share is a permille of the population, and what it signals
    // is recovery — not the thermal hue it borrowed.
    model.recovery_trend = Trace::single(
        SignalRole::Recovery,
        meters.system.stopped_history().to_vec(),
    );
    model.resources = resources;

    PanelModel {
        model,
        task_owners,
        recovery_owners,
        task_idents,
    }
}

/// One [`TaskSummary`] per sampled process, in sampled order, each row
/// naming its own pid and identity so [`apply_action`] can resolve it.
///
/// A row carries no Pressure Rail: the System Information API reports a
/// process's CPU time, not which resource that process is straining, so
/// naming one would be a guess dressed as a measurement.
///
/// Every row is a process, because that is what the process list reports:
/// nothing here reads a background-job registry or a service registry —
/// neither exists — so no row claims to be either, and the surface offers no
/// filter or tile that could only ever count nought.
///
/// Owner and Core come straight off the process record, so a busy core in
/// the CPU pane can be traced to the task sitting on it and per-principal
/// accounting is visible on a machine with many users.
///
/// The disk rate and the sparkline history come from `meters`, which the
/// caller has already folded this sample into.
fn build_tasks(
    processes: &[ProcessSummary],
    seat_report: &SeatReport,
    meters: &TaskMeters,
    bundles: &OwnerBundles,
    can_force: bool,
) -> (Vec<TaskSummary>, Vec<u64>, Vec<TaskIdent>) {
    let mut tasks = Vec::with_capacity(processes.len());
    let mut owners = Vec::with_capacity(processes.len());
    let mut idents = Vec::with_capacity(processes.len());
    for process in processes {
        let name = display_name(&process.name);
        tasks.push(TaskSummary {
            proc_id: process.proc_id,
            name: name.clone(),
            bundle: bundles.of(process.proc_id).map(String::from),
            owner: TaskOwner::new(process.uid),
            core: Some(process.cpu),
            lifecycle: Some(process.state),
            cpu_permille: process.cpu_permille,
            memory_bytes: Some(process.mem_bytes),
            disk_bytes_per_sec: meters.disk_rate(process.proc_id),
            cpu_history: meters.cpu_history(process.proc_id).to_vec(),
            pressure: PressureState::None,
            activity: process_activity(process.state),
            recovery: process_recovery(process, seat_report),
            authority: task_authority(process.state, can_force),
        });
        owners.push(process.pid);
        idents.push(TaskIdent {
            proc_id: process.proc_id,
            pid: process.pid,
            name,
        });
    }
    (tasks, owners, idents)
}

/// What the caller may do to one sampled process.
///
/// Two things decide each command: the caller's own authority, and the
/// task's lifecycle state. Signalling a task — pausing it, continuing it,
/// ending it — needs `PROC_CONTROL`, so without it those wear the Authority
/// Mark; with it, the state still rules out the ones that make no sense for
/// this task, which is a plain disablement rather than a refusal of
/// authority. A task that has already exited can be signalled to no effect,
/// so nothing is offered for it at all.
///
/// Raising a task's window is a plain session request needing no capability
/// to *attempt* — the session is free to refuse it — and lowering a priority
/// is the same signal-level authority as the rest.
fn task_authority(state: ProcessState, can_force: bool) -> TaskAuthority {
    let signal = |permitted: bool| match (can_force, permitted) {
        (false, _) => ActionVerdict::DeniedByAuthority,
        (true, false) => ActionVerdict::DisabledByState,
        (true, true) => ActionVerdict::Ready,
    };
    let live = !matches!(state, ProcessState::Zombie);
    TaskAuthority {
        switch: if live {
            ActionVerdict::Ready
        } else {
            ActionVerdict::DisabledByState
        },
        pause: signal(live && state != ProcessState::Stopped),
        resume: signal(state == ProcessState::Stopped),
        lower_priority: signal(live && state != ProcessState::Stopped),
        force_quit: signal(live),
    }
}

/// The recovery posture a sampled process is in.
///
/// The one definition of "this task is in trouble", shared by the task
/// row's Signal Bead (and the fault filter that counts it) and the
/// Recovery section's own list, so the table can never disagree with the
/// list about which tasks are faulted.
///
/// A process the scheduler reports stopped is recoverable; one the seat
/// named unresponsive is hung. Stopped wins when both hold, matching the
/// Recovery list, which reports a stopped process once rather than twice.
fn process_recovery(process: &ProcessSummary, seat_report: &SeatReport) -> RecoveryState {
    if process.state == ProcessState::Stopped {
        return RecoveryState::Recoverable;
    }
    if seat_report.owners().contains(&process.pid) {
        return RecoveryState::Hung;
    }
    RecoveryState::None
}

/// The [`ActivityState`] a process's [`ProcessState`] implies.
///
/// A live process — on a CPU, waiting for one, or blocked awaiting an
/// event it will be woken by — has work in progress whose extent the row
/// does not quantify. A process that has exited but not yet been reaped,
/// or that a job-control signal stopped, has none.
const fn process_activity(state: ProcessState) -> ActivityState {
    match state {
        ProcessState::Runnable | ProcessState::Running | ProcessState::Blocked => {
            ActivityState::Working
        }
        ProcessState::Zombie | ProcessState::Stopped => ActivityState::Idle,
    }
}

/// One [`RecoveryItem`] per stopped process this service sampled itself,
/// plus one per unresponsive owner the seat report names that this
/// service's own sample can still put a name to; an owner the report names
/// but this sample never saw contributes no row (never a fabricated one) —
/// the join is against the names this service attested itself, never a
/// name taken from the wire.
///
/// Each item carries everything the Recovery screen shows about that one
/// fault: its stable identity, how long it has stood (from `meters`'
/// [`FaultClock`], the only honest source), what to do about it, the marks
/// this service observed, the kernel's crash record where the fault raised
/// one, and the task's own resource cost.
fn build_recovery(
    sample: &Sample,
    seat_report: &SeatReport,
    meters: &RollingMeters,
    can_force: bool,
) -> (Vec<RecoveryItem>, Vec<u64>) {
    let mut recovery = Vec::new();
    let mut owners = Vec::new();
    let now = sample.uptime.map(|uptime| uptime.since_boot);

    // Stopped processes first, then the unresponsive ones, so the list
    // groups by condition rather than by sampled order. Both conditions
    // are read through the one shared classifier, so this list and the
    // task rows' own Signal Beads can never disagree about which tasks
    // are faulted. A process that is both is reported once, as stopped,
    // because that is what the classifier resolves it to.
    for state in [RecoveryState::Recoverable, RecoveryState::Hung] {
        for process in &sample.processes {
            if process_recovery(process, seat_report) != state {
                continue;
            }
            let since = elapsed_reading(meters.faults.elapsed(process.proc_id, now), sample);
            recovery.push(RecoveryItem {
                proc_id: process.proc_id,
                pid: process.pid,
                name: display_name(&process.name),
                detail: String::from(fault_detail(state)),
                since: since.clone(),
                recovery: state,
                impact: FaultImpact::of(state),
                status: String::from(fault_status(state)),
                recommendation: String::from(fault_recommendation(state, can_force)),
                marks: fault_marks(state, &since),
                crash: crash_snapshot(sample, process.proc_id),
                cpu: cpu_reading(process.cpu_permille, sample),
                memory: Reading::measured(format_bytes(process.mem_bytes)),
                disk: disk_reading(meters.tasks.disk_rate(process.proc_id), sample),
                network: Reading::Absent(Unmeasured::NoInterface),
                can_restart: true,
                can_force,
            });
            owners.push(process.pid);
        }
    }

    (recovery, owners)
}

/// The trailing detail a fault's card shows: what happened, in the fewest
/// words that distinguish the two conditions.
const fn fault_detail(state: RecoveryState) -> &'static str {
    match state {
        RecoveryState::Hung => "not responding",
        _ => "stopped",
    }
}

/// The plain statement of what state a fault is in.
const fn fault_status(state: RecoveryState) -> &'static str {
    match state {
        RecoveryState::Hung => "The task is running but has stopped answering its seat.",
        _ => "The task has been stopped and is holding what it held.",
    }
}

/// What a reader should do about a fault.
///
/// The recommendation names only a command the caller can actually take: a
/// caller without process control is told to restart rather than pointed at
/// a force they would be refused.
const fn fault_recommendation(state: RecoveryState, can_force: bool) -> &'static str {
    match (state, can_force) {
        (RecoveryState::Hung, true) => "Restart it; force it only if the restart does not take.",
        (RecoveryState::Hung, false) => "Restart it.",
        (_, _) => "Restart it to release what it is holding.",
    }
}

/// How long something has stood, or why that cannot be said.
///
/// A duration is only ever a measured one: with no uptime reading there is
/// no clock to measure against, and the sample's own verdict explains why
/// that reading is absent rather than this deriving a second opinion.
/// Shared by Recovery's fault age and Pressure's band age, so the two
/// screens can never render the same absence two different ways.
fn elapsed_reading(elapsed: Option<Duration64>, sample: &Sample) -> Reading {
    reading(sample, DegradedField::Uptime, elapsed, format_duration)
}

/// A CPU share, or why there is none.
///
/// A share is unmeasured on the very first reading — there is no interval
/// to divide by — which the sample's own verdict explains rather than this
/// reporting a zero nothing ever idled at. Shared by a faulted task's own
/// share and the whole processor's busy share, so a per-task figure and a
/// whole-resource one can never render the same absence two different ways.
fn cpu_reading(permille: Option<u16>, sample: &Sample) -> Reading {
    reading(sample, DegradedField::CpuTime, permille, percent)
}

/// A faulted task's own storage throughput, or why there is none.
///
/// Absent for exactly the reason a rate is absent anywhere else: no
/// previous reading of *this* task to delta against. A counter that did not
/// move over a real interval is a genuine `0 B/s` and is reported as one.
fn disk_reading(bytes_per_sec: Option<u64>, sample: &Sample) -> Reading {
    reading(
        sample,
        DegradedField::ProcessList,
        bytes_per_sec,
        format_rate,
    )
}

/// The marks a fault's timeline carries.
///
/// Only what this service observed: the fault itself, stamped with the age
/// it has stood, and — where the age is known — the observation that it is
/// still standing now. A history this service did not see is not invented.
fn fault_marks(state: RecoveryState, since: &Reading) -> Vec<FaultMark> {
    let mut marks = alloc::vec![FaultMark {
        stamp: match since {
            Reading::Measured(age) => alloc::format!("{age} ago"),
            Reading::Absent(_) => String::from("when observed"),
        },
        text: String::from(match state {
            RecoveryState::Hung => "Stopped answering its seat",
            _ => "Stopped by the kernel",
        }),
        is_fault: true,
    }];
    if matches!(since, Reading::Measured(_)) {
        marks.push(FaultMark {
            stamp: String::from("now"),
            text: String::from("Still faulted"),
            is_fault: false,
        });
    }
    marks
}

/// The kernel's crash record for `proc_id`, matched by the task's own
/// stable identity.
///
/// The match is on [`ProcId`] and nothing else: a numeric pid is reused, so
/// matching on one could attribute a dead task's crash to a live task that
/// inherited its number. A fault with no record answers [`None`], which the
/// screen states plainly rather than drawing an empty table.
fn crash_snapshot(sample: &Sample, proc_id: ProcId) -> Option<CrashSnapshot> {
    let crash = sample
        .crashes
        .as_ref()?
        .iter()
        .find(|record| record.proc_id == proc_id)?;
    Some(CrashSnapshot {
        cause: String::from(crash_cause(crash.fault_class)),
        location: crash_location(crash.fault_bucket, crash.fault_offset),
        access: String::from(match crash.access() {
            CrashAccess::Read => "read",
            CrashAccess::Write => "write",
            CrashAccess::Instruction => "instruction (no data access)",
        }),
        owner: alloc::format!("uid {}, gid {}", crash.uid, crash.gid),
        pc: alloc::format!(
            "{:#018x} ({})",
            crash.pc,
            if crash.load_base_known() {
                "program-relative"
            } else {
                "absolute"
            }
        ),
        sp: alloc::format!("{:#018x}", crash.sp),
        fp: if crash.fp_valid() {
            alloc::format!("{:#018x}", crash.fp)
        } else {
            String::from("not meaningful for this frame")
        },
        registers: crash
            .regs()
            .iter()
            .map(|reg| (display_name(reg.name_bytes()), reg.value))
            .collect(),
        frames: crash.frames().to_vec(),
    })
}

/// Why the resolver refused the faulting access, in the kernel's own terms.
const fn crash_cause(class: CrashFaultClass) -> &'static str {
    match class {
        CrashFaultClass::Stack => "stack growth the kernel could not back",
        CrashFaultClass::StackLimit => "stack growth refused by the task's stack bound",
        CrashFaultClass::FileRegion => "refused access inside a file mapping",
        CrashFaultClass::Anon => "reserved memory the kernel could not back",
        CrashFaultClass::Wild => "outside every mapping the task owns",
        CrashFaultClass::Instruction => "an instruction the CPU refused to execute",
    }
}

/// Where the faulting address sat, as a distance from its anchor rather
/// than an absolute address.
///
/// Two of the buckets carry no meaningful distance, and say so rather than
/// printing the `0` the record holds — which a reader would take for a
/// measured offset of nothing.
fn crash_location(bucket: CrashFaultBucket, offset: u64) -> String {
    match bucket {
        CrashFaultBucket::NullPage => alloc::format!("{offset} bytes into the null page"),
        CrashFaultBucket::BelowStackGuard => {
            alloc::format!("{offset} bytes below the stack guard")
        }
        CrashFaultBucket::PastRegion => alloc::format!("{offset} bytes past its region"),
        CrashFaultBucket::Wild => String::from("far from every mapping (no distance to give)"),
        CrashFaultBucket::InRegion => String::from("inside a region it owns (no distance to give)"),
        CrashFaultBucket::NoDataAddress => {
            String::from("no data address — the instruction itself was refused")
        }
    }
}

/// The outbound effect an interactive [`SwitchboardAction`] implies.
/// [`crate::panel`] applies every entry of the [`Vec`] [`apply_action`]
/// returns, in order; an empty vector means nothing to do beyond letting
/// the caller re-render.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Effect {
    /// Ask the session to raise the named owner's front window.
    ActivateOwner {
        /// The owner's scheduler task id.
        owner: u64,
    },
    /// Ask the session to re-launch the named owner.
    RestartOwner {
        /// The owner's scheduler task id.
        owner: u64,
    },
    /// Deliver a control signal to the named process directly.
    Signal {
        /// The scheduler task id to signal.
        pid: u64,
        /// The signal to deliver.
        signal: Signal,
    },
    /// Lower the named process's scheduling priority.
    LowerPriority {
        /// The scheduler task id to lower.
        pid: u64,
    },
}

/// Map an interactive [`SwitchboardAction`] the composed control reported
/// to the [`Effect`]s it implies, resolving every index against `panel` and
/// re-checking authority for every authority-gated action from the very
/// same [`PanelModel`] verdicts [`build_model`] already computed under the
/// same authority — so an action whose authority is absent is never
/// attempted, matching the render-time check the model already applied. An
/// out-of-range index yields no effect (fail closed) rather than guessing.
#[must_use]
pub fn apply_action(
    panel: &PanelModel,
    action: SwitchboardAction,
    authority: &dyn CapabilityQuery,
) -> Vec<Effect> {
    match action {
        SwitchboardAction::Task { index, control } => apply_task(panel, index, control),
        SwitchboardAction::Recovery { index, control } => {
            let Some(owner) = panel.recovery_owner(index) else {
                return Vec::new();
            };
            match control {
                RecoveryControl::Restart => alloc::vec![Effect::RestartOwner { owner }],
                RecoveryControl::Force => {
                    if authority.holds(CapabilityId::PROC_CONTROL) {
                        alloc::vec![Effect::Signal {
                            pid: owner,
                            signal: Signal::Kill,
                        }]
                    } else {
                        Vec::new()
                    }
                }
            }
        }
        // Every resource command the rail draws as available is a view
        // transition the composed control resolves itself; the rest have no
        // endpoint behind them and are drawn disabled, so a scripted or
        // otherwise unexpected report of one yields no effect rather than
        // acting on something this service never renders as available.
        SwitchboardAction::Resource { .. }
        | SwitchboardAction::SectionChanged { .. }
        | SwitchboardAction::Scrolled { .. } => Vec::new(),
    }
}

/// One task command, re-checked against the verdict [`build_model`] already
/// computed for that very task rather than re-deriving authority from
/// scratch — the model's verdict *is* the server-side check, computed once
/// under the real authority, so a command the rail drew as denied or
/// disabled can never be carried out by a scripted or otherwise unexpected
/// report of it (fail closed).
///
/// [`TaskControl::Reveal`] is the same request of the session as
/// [`TaskControl::Switch`]: raising a task's window is how this system shows
/// the reader where it is, and there is no separate "highlight without
/// raising" interface to invent one for. [`TaskControl::OpenLogs`] resolves
/// to nothing at all: no capability-gated query for a task's own log entries
/// exists, which is exactly why its verdict is permanently disabled.
fn apply_task(panel: &PanelModel, index: usize, control: TaskControl) -> Vec<Effect> {
    let Some(task) = panel.model.tasks.get(index) else {
        return Vec::new();
    };
    if task.authority.verdict(control) != ActionVerdict::Ready {
        return Vec::new();
    }
    let Some(owner) = panel.task_owner(index) else {
        return Vec::new();
    };
    match control {
        TaskControl::Switch | TaskControl::Reveal => {
            alloc::vec![Effect::ActivateOwner { owner }]
        }
        TaskControl::Pause => alloc::vec![Effect::Signal {
            pid: owner,
            signal: Signal::Stop,
        }],
        TaskControl::Resume => alloc::vec![Effect::Signal {
            pid: owner,
            signal: Signal::Continue,
        }],
        TaskControl::LowerPriority => alloc::vec![Effect::LowerPriority { pid: owner }],
        TaskControl::ForceQuit => alloc::vec![Effect::Signal {
            pid: owner,
            signal: Signal::Kill,
        }],
        TaskControl::OpenLogs => Vec::new(),
    }
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
