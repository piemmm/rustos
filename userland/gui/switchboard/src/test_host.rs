//! Test doubles shared by the panel and service unit tests: one recording
//! [`ServiceHost`], one deliberately dead `sysinfo` transport, and one
//! fixed capability set.
//!
//! They live here rather than in either test file so the two suites drive
//! exactly the same stand-ins and cannot drift apart.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::switchboard_ipc::{SwitchboardRequest, TraySummary};
use tairix_abi::sysinfo::{
    ProcessListRequest, ProcessRecord, ProcessState, SysinfoQueryId, SysinfoRequestHeader,
};
use tairix_abi::{
    CapabilityId, CapabilityQuery, Errno, PowerAction, ProcId, SchedPriority, Signal,
};
use tairix_procinfo::Transport;

use crate::sample::{DegradedField, ProcessSummary, Sample};
use crate::service::{RenderInputs, ServiceHost};
use crate::view::Switchboard;
use crate::wait::{required_members, WaitToken};

/// The uid [`process_summary`] gives a fixture row when the caller does not
/// need to vary it — an ordinary, unprivileged owner distinct from the
/// service's own fixture pid used across the panel/service test suites.
pub(crate) const DEFAULT_UID: u32 = 1000;

/// One sampled process, its never-reused instance identity derived from
/// its task id so two fixtures never collide, with the ordinary
/// [`DEFAULT_UID`] owner, no measured memory footprint, and
/// [`SchedPriority::Normal`] — the shape most tests need.
pub(crate) fn process_summary(
    pid: u64,
    state: ProcessState,
    name: &[u8],
    cpu_permille: Option<u16>,
) -> ProcessSummary {
    process_summary_with(
        pid,
        state,
        name,
        cpu_permille,
        DEFAULT_UID,
        0,
        SchedPriority::Normal,
    )
}

/// [`process_summary`], with every pressure/activity-relevant field the
/// caller needs to vary spelled out explicitly.
pub(crate) fn process_summary_with(
    pid: u64,
    state: ProcessState,
    name: &[u8],
    cpu_permille: Option<u16>,
    uid: u32,
    mem_bytes: u64,
    priority: SchedPriority,
) -> ProcessSummary {
    let mut raw = [0u8; 16];
    raw[0..8].copy_from_slice(&pid.to_le_bytes());
    ProcessSummary {
        pid,
        proc_id: ProcId::from_raw(raw),
        name: name.to_vec(),
        state,
        uid,
        mem_bytes,
        priority,
        cpu_permille,
    }
}

/// A sample carrying exactly `processes` and no other reading.
pub(crate) fn sample_with(processes: Vec<ProcessSummary>) -> Sample {
    Sample {
        processes,
        ..Sample::default()
    }
}

/// A capability set that holds exactly the listed capabilities.
pub(crate) struct FixedAuthority(pub(crate) &'static [CapabilityId]);

impl CapabilityQuery for FixedAuthority {
    fn holds(&self, cap: CapabilityId) -> bool {
        self.0.contains(&cap)
    }
}

/// Holds nothing at all.
pub(crate) const NO_AUTHORITY: FixedAuthority = FixedAuthority(&[]);

/// Holds only the process-control capability the recovery Force action
/// needs.
pub(crate) const PROC_CONTROL_AUTHORITY: FixedAuthority =
    FixedAuthority(&[CapabilityId::PROC_CONTROL]);

/// Holds only the power capability the desktop's Restart and Shut Down rows
/// need.
pub(crate) const SYSTEM_POWER_AUTHORITY: FixedAuthority =
    FixedAuthority(&[CapabilityId::SYSTEM_POWER]);

/// A `sysinfo` transport that answers nothing.
///
/// Every query fails, so each sample degrades to its honest empty form —
/// which is exactly the state the service must keep running (and keep
/// publishing) in. Sampling fidelity itself is covered against the paging
/// fixture in the sampler's own tests.
pub(crate) struct DeadTransport;

impl Transport for DeadTransport {
    fn query(&self, _request: &[u8]) -> Result<Vec<u8>, Errno> {
        Err(Errno::NotFound)
    }
}

/// A wire-level process record for [`ProcessListTransport`], with the
/// ordinary defaults [`process_summary`] uses.
pub(crate) fn process_record(
    pid: u64,
    proc_id: ProcId,
    uid: u32,
    state: ProcessState,
    name: &[u8],
) -> ProcessRecord {
    ProcessRecord::new(
        pid,
        1,
        proc_id,
        ProcId::KERNEL,
        uid,
        uid,
        state,
        0,
        SchedPriority::Normal,
        0,
        0,
        0,
        0,
        name,
    )
    .expect("valid record")
}

/// A `sysinfo` transport that serves a fixed process list and refuses every
/// other query.
///
/// This is deliberately narrower than the sampler's own fixture: the
/// service tests that need it are exercising the service's own-identity
/// lookup and the activity-grouping state against a real sampled row, not
/// the sampler's delta/degradation bookkeeping (covered in
/// `sample_tests.rs`), so CPU-time and memory-pressure stay honestly
/// unmeasured here.
pub(crate) struct ProcessListTransport {
    records: Vec<ProcessRecord>,
    requests: core::sync::atomic::AtomicUsize,
}

impl ProcessListTransport {
    pub(crate) fn new(records: Vec<ProcessRecord>) -> Self {
        Self {
            records,
            requests: core::sync::atomic::AtomicUsize::new(0),
        }
    }

    pub(crate) fn request_count(&self) -> usize {
        self.requests.load(core::sync::atomic::Ordering::Relaxed)
    }
}

impl Transport for ProcessListTransport {
    fn query(&self, request: &[u8]) -> Result<Vec<u8>, Errno> {
        self.requests
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let header = SysinfoRequestHeader::from_bytes(request)?;
        if !matches!(
            header.query,
            SysinfoQueryId::SELF_PROCESS_LIST | SysinfoQueryId::GLOBAL_PROCESS_LIST
        ) {
            return Err(Errno::NotFound);
        }
        let payload = &request[SysinfoRequestHeader::WIRE_LEN
            ..SysinfoRequestHeader::WIRE_LEN + header.payload_len as usize];
        let req = ProcessListRequest::from_bytes(payload)?;
        let offset = req.offset as usize;
        if offset >= self.records.len() {
            return Ok(Vec::new());
        }
        let take = core::cmp::min(self.records.len() - offset, req.limit as usize);
        let mut out = Vec::new();
        for record in &self.records[offset..offset + take] {
            out.extend_from_slice(&record.to_le_bytes());
        }
        Ok(out)
    }
}

/// A [`ServiceHost`] that records everything the service asked of it and
/// answers with whichever refusals the test configured.
///
/// It mirrors the production host's own bookkeeping where it matters: the
/// window's event mailbox joins the multiplexed wait when the window opens
/// and leaves it when the window closes, so [`Self::armed`] is the real
/// wait-set membership and not a restatement of the expected answer.
pub(crate) struct RecordingHost {
    armed: Vec<WaitToken>,
    /// Windows created.
    pub(crate) opened: usize,
    /// Windows destroyed.
    pub(crate) closed: usize,
    /// Frames presented.
    pub(crate) presents: usize,
    /// Every owner-directed request attempted, in order.
    pub(crate) requests: Vec<SwitchboardRequest>,
    /// Every summary publish attempted, in order.
    pub(crate) published: Vec<TraySummary>,
    /// Every signal attempted, in order.
    pub(crate) signals: Vec<(i32, Signal)>,
    /// Every priority lowering attempted, in order.
    pub(crate) lowered: Vec<i32>,
    /// Every power transition attempted, in order.
    pub(crate) powered: Vec<PowerAction>,
    /// Every refusal stated, in order.
    pub(crate) refusals: Vec<(String, Errno)>,
    /// Every degradation noted, in order.
    pub(crate) degradations: Vec<DegradedField>,
    /// Refusal to answer a window create with.
    pub(crate) open_refusal: Option<Errno>,
    /// Refusal to answer a present with.
    pub(crate) present_refusal: Option<Errno>,
    /// Refusal to answer an owner-directed request with.
    pub(crate) request_refusal: Option<Errno>,
    /// Refusal to answer a publish with.
    pub(crate) publish_refusal: Option<Errno>,
    /// Refusal to answer a signal with.
    pub(crate) signal_refusal: Option<Errno>,
    /// Refusal to answer a priority lowering with.
    pub(crate) lower_refusal: Option<Errno>,
    /// Refusal to answer a power transition with.
    pub(crate) power_refusal: Option<Errno>,
    /// The client bounds a present would use while a window is open, as
    /// `(left, top, width, height)`. Tests mutate this directly to
    /// simulate a resize.
    pub(crate) bounds: (i32, i32, u32, u32),
    /// The active theme's identity a present would use. Tests mutate this
    /// directly to simulate a theme change.
    pub(crate) theme_id: u32,
    /// The active render scale, as its whole-percent value, a present
    /// would use. Tests mutate this directly to simulate a scale change.
    pub(crate) scale_percent: u32,
}

impl RecordingHost {
    /// A host that accepts everything, with no window open.
    pub(crate) fn new() -> Self {
        Self {
            armed: required_members(false),
            opened: 0,
            closed: 0,
            presents: 0,
            requests: Vec::new(),
            published: Vec::new(),
            signals: Vec::new(),
            lowered: Vec::new(),
            powered: Vec::new(),
            refusals: Vec::new(),
            degradations: Vec::new(),
            open_refusal: None,
            present_refusal: None,
            request_refusal: None,
            publish_refusal: None,
            signal_refusal: None,
            lower_refusal: None,
            power_refusal: None,
            bounds: (0, 0, 600, 400),
            theme_id: 1,
            scale_percent: 100,
        }
    }

    /// The wait-set members currently armed.
    pub(crate) fn armed(&self) -> &[WaitToken] {
        &self.armed
    }

    /// The actions whose refusals were stated, in order.
    pub(crate) fn refused_actions(&self) -> Vec<&str> {
        self.refusals
            .iter()
            .map(|(action, _)| action.as_str())
            .collect()
    }
}

impl ServiceHost for RecordingHost {
    fn open_window(&mut self) -> Result<(), Errno> {
        if let Some(refusal) = self.open_refusal {
            return Err(refusal);
        }
        self.opened += 1;
        self.armed = required_members(true);
        Ok(())
    }

    fn close_window(&mut self) -> Result<(), Errno> {
        self.closed += 1;
        self.armed = required_members(false);
        Ok(())
    }

    fn present(&mut self, _panel: &mut Switchboard) -> Result<(), Errno> {
        if let Some(refusal) = self.present_refusal {
            return Err(refusal);
        }
        self.presents += 1;
        Ok(())
    }

    fn request(&mut self, request: SwitchboardRequest) -> Result<(), Errno> {
        self.requests.push(request);
        self.request_refusal.map_or(Ok(()), Err)
    }

    fn publish(&mut self, summary: TraySummary) -> Result<(), Errno> {
        self.published.push(summary);
        self.publish_refusal.map_or(Ok(()), Err)
    }

    fn signal(&mut self, pid: i32, signal: Signal) -> Result<(), Errno> {
        self.signals.push((pid, signal));
        self.signal_refusal.map_or(Ok(()), Err)
    }

    fn lower_priority(&mut self, pid: i32) -> Result<(), Errno> {
        self.lowered.push(pid);
        self.lower_refusal.map_or(Ok(()), Err)
    }

    fn power(&mut self, action: PowerAction) -> Result<(), Errno> {
        self.powered.push(action);
        self.power_refusal.map_or(Ok(()), Err)
    }

    fn report_refusal(&mut self, action: &str, refusal: Errno) {
        self.refusals.push((action.to_string(), refusal));
    }

    fn render_inputs(&self) -> Option<RenderInputs> {
        if !self.armed.contains(&WaitToken::WindowEvent) {
            return None;
        }
        Some(RenderInputs {
            bounds_left: self.bounds.0,
            bounds_top: self.bounds.1,
            bounds_width: self.bounds.2,
            bounds_height: self.bounds.3,
            theme_id: self.theme_id,
            scale_percent: self.scale_percent,
        })
    }

    fn note_degradation(&mut self, field: DegradedField) {
        self.degradations.push(field);
    }
}
