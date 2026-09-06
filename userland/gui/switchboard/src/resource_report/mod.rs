//! Turn one [`Sample`] into the [`ResourceReport`] the Resources section
//! draws: one device per pane, in rail order.
//!
//! Every figure comes from a reading the sampler actually took. Where one is
//! missing the report carries [`Reading::Absent`] with the reason the sample
//! itself resolved, so a pane states "not permitted" where this session's
//! authority stops and "unavailable" where the query was permitted but
//! unanswered. Nothing is inferred, defaulted, or rounded up into a
//! plausible number.
//!
//! One module per pane, each building its own device's rail entry, hero,
//! blocks and commands from the readings that pane is about.

use core::fmt::Write;

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::blkio::BlkDeviceClass;
use tairix_abi::net_ipc::{NetAddrFamily, NetAddrState, NetIfAddr, NetIfKind, NetServerAddr};
use tairix_abi::rlimit::{LimitKind, RLIMIT_INFINITY};
use tairix_abi::sysinfo::{
    CpuCoreClass, LoadAverage, MountAvailability, MountRecord, VolumeIoHealthRecord,
};
use tairix_abi::{CapabilityId, CapabilityQuery};

use crate::format::{format_bytes, format_duration};
use crate::model::{display_name, RollingMeters, SessionReport};
use crate::sample::{DegradedField, Sample};
use crate::view::reading::{HealthSeverity, Reading, ReadingFact as SystemFact, Unmeasured};
use crate::view::resources::{DeviceId, ResourceReport};

mod consumers;
mod cpu;
mod graphics;
mod interface;
mod machine;
mod memory;
mod volume;

/// Build the Resources section's whole report from this sample.
///
/// One value carries every pane, so the view never asks a second question
/// mid-render and a pane can never show a figure from a different sample
/// than the rail entry beside it. The rail's length is *discovered*: one
/// entry per device the sample names, so a hundred-core machine with a
/// dozen volumes gets a longer rail rather than a truncated one.
#[must_use]
pub fn build_resource_report(
    sample: &Sample,
    meters: &mut RollingMeters,
    session: &SessionReport,
    authority: &dyn CapabilityQuery,
) -> ResourceReport {
    let mut devices = alloc::vec![cpu::device(sample, meters), memory::device(sample, meters),];
    let mut recorded = alloc::vec![DeviceId::Cpu, DeviceId::Memory];

    for mount in sample.mounts.iter().flatten() {
        devices.push(volume::device(sample, meters, mount));
        recorded.push(DeviceId::Volume(mount.volume_id()));
    }
    for iface in sample.net_facts.iter().flatten() {
        // The counters are cumulative, so the interface's own rate is the
        // delta this fold produces rather than anything one sample carries.
        let id = DeviceId::Interface(iface.name);
        if let Some(counters) = sample
            .net_counters
            .as_ref()
            .and_then(|records| records.iter().find(|r| r.name == iface.name))
        {
            meters.devices.record_device(
                id,
                counters.counters.rx_bytes,
                counters.counters.tx_bytes,
                sample.elapsed_ns,
            );
        }
        devices.push(interface::device(sample, iface));
        recorded.push(id);
    }

    devices.push(graphics::device(
        sample,
        session.frame,
        meters.devices.primary_history(DeviceId::Graphics),
    ));
    recorded.push(DeviceId::Graphics);

    devices.push(machine::identity(sample));
    devices.push(machine::sessions(sample));
    devices.push(machine::authority(sample, authority));
    recorded.extend([DeviceId::Identity, DeviceId::Sessions, DeviceId::Authority]);

    // Anything the sample did not name this cycle leaks neither its history
    // nor its counters.
    meters.devices.retain_recorded(&recorded);

    ResourceReport {
        devices,
        volumes_absent: absent_unless(sample, DegradedField::Mounts, sample.mounts.is_some()),
        interfaces_absent: absent_unless(
            sample,
            DegradedField::NetInterfaceFacts,
            sample.net_facts.is_some(),
        ),
    }
}

/// Why `field` is missing, or [`None`] when `present` says it is not.
///
/// One place decides the shape of "absent, and here is why", so no page
/// can invent a different vocabulary for the same condition.
fn absent_unless(sample: &Sample, field: DegradedField, present: bool) -> Option<Unmeasured> {
    (!present).then(|| Unmeasured::from_absence(sample.absence(field)))
}

/// A reading built from an optional measurement, falling back to the
/// sample's own explanation for `field` when there is none.
///
/// The one place an absent measurement becomes an absent reading, so every
/// figure the product shows — a header tile, a page fact, a fault's age, a
/// pressure cause's amount, an activity's combined total — explains itself
/// with the verdict the service already reached, and no screen can invent a
/// second opinion about why a reading is missing.
pub(crate) fn reading<T>(
    sample: &Sample,
    field: DegradedField,
    value: Option<T>,
    text: impl Fn(T) -> String,
) -> Reading {
    value.map_or_else(
        || Reading::Absent(Unmeasured::from_absence(sample.absence(field))),
        |value| Reading::measured(text(value)),
    )
}

/// What a mounted volume holds, in bytes.
///
/// A named pair rather than a tuple: "total and available" and "used and
/// total" are both plausible readings of two byte figures, and a caller that
/// picks the wrong one reports a full disk as empty. Naming them makes that
/// mistake unrepresentable.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) struct VolumeBytes {
    /// The volume's whole capacity.
    pub(super) total: u64,
    /// What is still available on it.
    pub(super) available: u64,
}

impl VolumeBytes {
    /// What is in use: the capacity less what is available, saturating so a
    /// service reporting more available than total reads as nothing used.
    pub(super) const fn used(self) -> u64 {
        self.total.saturating_sub(self.available)
    }
}

/// What `mount` holds, or [`None`] when the format tracks no fixed capacity.
fn volume_bytes(mount: &MountRecord) -> Option<VolumeBytes> {
    let stats = mount.usage();
    (stats.total_blocks > 0).then(|| {
        let block = u64::from(stats.block_size);
        VolumeBytes {
            total: stats.total_blocks.saturating_mul(block),
            available: stats.avail_blocks.saturating_mul(block),
        }
    })
}

/// The used fraction of `total` given `avail` free, in permille.
///
/// Saturating throughout: a service reporting more available than total
/// yields nought used rather than an underflow. The scaling is done in
/// [`u128`] because a volume of the size TAIRiX must serve overflows a
/// [`u64`] once multiplied by a thousand, and a saturated numerator would
/// under-report a full disk as very nearly empty.
pub(super) fn used_permille(total: u64, avail: u64) -> u16 {
    if total == 0 {
        return 0;
    }
    let used = u128::from(total.saturating_sub(avail));
    let permille = used.saturating_mul(1000) / u128::from(total);
    u16::try_from(permille).unwrap_or(1000).min(1000)
}

/// The machine's identity facts, in the order the Overview page reads
/// them.
fn machine_facts(sample: &Sample) -> Vec<SystemFact> {
    let identity = sample.identity.as_ref();
    alloc::vec![
        SystemFact::new(
            "Hostname",
            reading(sample, DegradedField::Identity, identity, |id| {
                display_name(id.hostname_bytes())
            }),
        ),
        SystemFact::new(
            "OS version",
            reading(sample, DegradedField::Identity, identity, |id| {
                format!(
                    "TAIRiX {}.{}.{}",
                    id.version_major, id.version_minor, id.version_patch
                )
            }),
        ),
        SystemFact::new(
            "Machine id",
            reading(sample, DegradedField::Identity, identity, |id| {
                hex(&id.machine_id)
            }),
        ),
        SystemFact::new(
            "Uptime",
            reading(sample, DegradedField::Uptime, sample.uptime, |uptime| {
                format_duration(uptime.since_boot)
            }),
        ),
        SystemFact::new(
            "Booted",
            reading(sample, DegradedField::Uptime, sample.uptime, |uptime| {
                format!("{} s since the epoch", uptime.boot_time.secs())
            }),
        ),
        SystemFact::new(
            "Processor",
            reading(
                sample,
                DegradedField::CpuInfo,
                sample.cpu_info.as_ref(),
                |cpus| {
                    cpus.first()
                        .map(|cpu| display_name(cpu.model_bytes()))
                        .filter(|model| !model.is_empty())
                        .unwrap_or_else(|| String::from("unnamed"))
                },
            ),
        ),
        SystemFact::new(
            "Cores",
            reading(
                sample,
                DegradedField::CpuInfo,
                sample.cpu_info.as_ref(),
                |cpus| core_census(cpus.iter().map(|cpu| cpu.class)),
            ),
        ),
        SystemFact::new(
            "Load average",
            reading(
                sample,
                DegradedField::LoadAverage,
                sample.load_average,
                |load| {
                    format!(
                        "{} {} {}",
                        fixed(load.load1),
                        fixed(load.load5),
                        fixed(load.load15)
                    )
                },
            ),
        ),
        SystemFact::new(
            "Installed memory",
            reading(
                sample,
                DegradedField::MemoryTotal,
                sample.memory_total,
                |total| format_bytes(total.total_bytes),
            ),
        ),
    ]
}

/// A core inventory as text: the total, and the performance/efficiency
/// split where the machine reports one.
fn core_census(classes: impl Iterator<Item = CpuCoreClass>) -> String {
    let mut total = 0usize;
    let mut efficiency = 0usize;
    for class in classes {
        total = total.saturating_add(1);
        if class == CpuCoreClass::Efficiency {
            efficiency = efficiency.saturating_add(1);
        }
    }
    if efficiency == 0 {
        return format!("{total}");
    }
    format!(
        "{total} ({} performance, {efficiency} efficiency)",
        total.saturating_sub(efficiency)
    )
}

/// A load average's fixed-point value as decimal text.
fn fixed(value: u32) -> String {
    format!(
        "{}.{:02}",
        LoadAverage::whole(value),
        LoadAverage::centis(value)
    )
}

/// A byte string as lower-case hexadecimal — the machine id's own
/// spelling, which is an identifier rather than text.
///
/// A write into a growable string cannot fail, and the identifier is
/// display text rather than a decision, so a short spelling is preferable
/// to refusing to name the machine at all.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::new();
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// What the service can attest about this session's authority: the
/// capabilities it holds, and the optional reading scopes those resolved
/// to.
fn authority_facts(sample: &Sample, authority: &dyn CapabilityQuery) -> Vec<SystemFact> {
    alloc::vec![
        SystemFact::new(
            "Process control",
            held(authority.holds(CapabilityId::PROC_CONTROL)),
        ),
        SystemFact::new(
            "System-wide readings",
            granted(sample.scopes.global_process_scope),
        ),
        SystemFact::new("Kernel readings", granted(sample.scopes.memory_pressure)),
        SystemFact::new("Hardware inventory", granted(sample.scopes.hardware_scope)),
    ]
}

/// A capability verdict as a reading: held, or explicitly not permitted.
fn held(holds: bool) -> Reading {
    if holds {
        Reading::measured("held")
    } else {
        Reading::Absent(Unmeasured::NotPermitted)
    }
}

/// A reading scope's verdict, in the same shape as [`held`] so the page
/// reads uniformly.
fn granted(granted: bool) -> Reading {
    if granted {
        Reading::measured("granted")
    } else {
        Reading::Absent(Unmeasured::NotPermitted)
    }
}

/// A volume's fault tallies as one line, naming only the buckets that
/// actually recorded something so a healthy disk reads as healthy rather
/// than as a wall of zeroes.
fn health_text(record: &VolumeIoHealthRecord) -> String {
    let counters = record.counters();
    let mut faults = Vec::new();
    for (label, count) in [
        ("timeouts", counters.timeouts),
        ("resets", counters.resets),
        ("medium errors", counters.medium_errors),
        ("offline", counters.offline),
        ("faults", counters.faults),
        ("degraded", counters.degraded),
    ] {
        if count > 0 {
            faults.push(format!("{count} {label}"));
        }
    }
    if faults.is_empty() {
        return format!("{} completions, no faults", counters.completions);
    }
    faults.join(", ")
}

/// The severity a volume's availability implies, so a failing disk is
/// drawn as a fault rather than as one more grey line.
const fn health_state(availability: MountAvailability) -> HealthSeverity {
    match availability {
        MountAvailability::Available => HealthSeverity::Healthy,
        MountAvailability::Degraded | MountAvailability::Recovering => HealthSeverity::Degraded,
        MountAvailability::UnavailableDirty
        | MountAvailability::UnavailableLost
        | MountAvailability::RecoveryConflict => HealthSeverity::Failing,
    }
}

/// A mount's availability in the words the mount table itself uses.
const fn availability_name(availability: MountAvailability) -> &'static str {
    match availability {
        MountAvailability::Available => "available",
        MountAvailability::UnavailableDirty => "unavailable (dirty)",
        MountAvailability::UnavailableLost => "unavailable (lost)",
        MountAvailability::RecoveryConflict => "recovery conflict",
        MountAvailability::Degraded => "degraded",
        MountAvailability::Recovering => "recovering",
    }
}

/// The medium a volume lives on, or that the mount table did not classify
/// it.
const fn medium_name(medium: Option<BlkDeviceClass>) -> &'static str {
    match medium {
        Some(BlkDeviceClass::Rotational) => "rotational",
        Some(BlkDeviceClass::SolidState) => "solid state",
        Some(BlkDeviceClass::Removable) => "removable",
        Some(BlkDeviceClass::Virtual) => "virtual",
        None => "unclassified",
    }
}

/// An interface name's bytes up to its first NUL — the wire carries a
/// fixed-width field, not a fixed-width name.
fn trim_nul(name: &[u8]) -> &[u8] {
    let end = name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name.len());
    name.get(..end).unwrap_or(name)
}

/// A hardware address in the conventional colon-separated hexadecimal.
fn mac(bytes: [u8; 6]) -> String {
    let mut out = String::new();
    for (i, byte) in bytes.iter().enumerate() {
        if i > 0 {
            out.push(':');
        }
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// An interface's kind as display text.
const fn kind_name(kind: NetIfKind) -> &'static str {
    match kind {
        NetIfKind::Ethernet => "ethernet",
        NetIfKind::Loopback => "loopback",
        NetIfKind::Bond => "bond",
    }
}

/// One configured address with its prefix length, and its state where
/// that state is anything other than the ordinary preferred one.
fn format_addr(addr: &NetIfAddr) -> String {
    let text = match addr.family {
        NetAddrFamily::V4 => ipv4(&addr.addr),
        NetAddrFamily::V6 => ipv6(&addr.addr),
    };
    let state = match addr.state {
        NetAddrState::Preferred => "",
        NetAddrState::Tentative => " (tentative)",
        NetAddrState::Deprecated => " (deprecated)",
    };
    format!("{text}/{}{state}", addr.prefix)
}

/// The first four bytes of an address slot as dotted-quad text.
fn ipv4(addr: &[u8; 16]) -> String {
    let octet = |index: usize| addr.get(index).copied().unwrap_or(0);
    format!("{}.{}.{}.{}", octet(0), octet(1), octet(2), octet(3))
}

/// An address slot as the eight colon-separated hexadecimal groups of an
/// IPv6 address, written in full rather than with the `::` elision, so a
/// reader can compare two addresses character by character.
fn ipv6(addr: &[u8; 16]) -> String {
    let mut out = String::new();
    for group in 0..8usize {
        if group > 0 {
            out.push(':');
        }
        let high = addr.get(group.saturating_mul(2)).copied().unwrap_or(0);
        let low = addr
            .get(group.saturating_mul(2).saturating_add(1))
            .copied()
            .unwrap_or(0);
        let _ = write!(out, "{:x}", u16::from(high) << 8 | u16::from(low));
    }
    out
}

/// A limit's name in the words the resource-limit facility uses.
pub(super) const fn limit_name(kind: LimitKind) -> &'static str {
    match kind {
        LimitKind::AddressSpaceBytes => "Address space",
        LimitKind::OpenStreams => "Open streams",
        LimitKind::Processes => "Processes",
        LimitKind::StackBytes => "Stack",
        LimitKind::PinnedMemoryBytes => "Pinned memory",
        LimitKind::Threads => "Threads",
    }
}

/// A limit bound in the unit its kind is denominated in, with the
/// unbounded sentinel spelled out rather than shown as a huge number.
pub(super) fn bound(kind: LimitKind, value: u64) -> String {
    if value == RLIMIT_INFINITY {
        return String::from("unlimited");
    }
    match kind {
        LimitKind::AddressSpaceBytes | LimitKind::StackBytes | LimitKind::PinnedMemoryBytes => {
            format_bytes(value)
        }
        LimitKind::OpenStreams | LimitKind::Processes | LimitKind::Threads => value.to_string(),
    }
}

#[cfg(test)]
#[path = "resource_report_tests.rs"]
mod tests;

/// One configured server's address, in the family it names.
pub(super) fn server_address(server: &NetServerAddr) -> String {
    match server.family {
        NetAddrFamily::V4 => ipv4(&server.addr),
        NetAddrFamily::V6 => ipv6(&server.addr),
    }
}
