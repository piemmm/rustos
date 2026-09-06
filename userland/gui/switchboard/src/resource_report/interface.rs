//! One pane per managed interface: a duplex rate trace over its stated
//! averaging window, and the stack behind it
//! (`plans/switchboard/05-network.png`).

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use tairix_abi::net_ipc::{NetInterfaceFactsRecord, NetServerAddr, IF_NAME_LEN};
use tairix_controls::PressureKind;

use super::{format_addr, kind_name, mac, reading, server_address, trim_nul};
use crate::format::{format_bytes, format_duration, format_rate};
use crate::model::display_name;
use crate::sample::{DegradedField, Sample};
use crate::view::reading::{absence_statement, ReadingFact, Unmeasured};
use crate::view::resources::{
    BlockBody, DeviceAction, DeviceGroup, DeviceId, HeroInstrument, PaneBlock, PaneHero,
    ResourceControl, ResourceDevice,
};

/// One interface's rail entry and pane.
pub(super) fn device(sample: &Sample, iface: &NetInterfaceFactsRecord) -> ResourceDevice {
    let rate = sample.net_rates.as_ref().and_then(|rates| {
        rates
            .iter()
            .find(|r| trim_nul(&r.name) == trim_nul(&iface.name))
    });
    let total = rate.map(|rate| rate.rx_bps.saturating_add(rate.tx_bps));
    ResourceDevice {
        id: DeviceId::Interface(name_key(iface)),
        group: DeviceGroup::Network,
        name: display_name(trim_nul(&iface.name)),
        kind: PressureKind::Network,
        reading: reading(sample, DegradedField::NetInterfaceRates, total, format_rate),
        // The rates query serves an already-averaged reading rather than a
        // counter to delta, so the trace plots the window it states rather
        // than a history this service derived.
        trend: Vec::new(),
        hero: PaneHero {
            value: reading(sample, DegradedField::NetInterfaceRates, total, format_rate),
            unit: String::new(),
            context: context(sample, iface),
            instrument: HeroInstrument::Track(None),
            caption: window_caption(sample, iface),
        },
        blocks: blocks(sample, iface),
        banner: None,
        actions: actions(),
    }
}

/// The interface name as the identity the selection remembers: the wire's
/// own fixed-width field, so a name that differs only past its NUL cannot
/// read as two devices.
fn name_key(iface: &NetInterfaceFactsRecord) -> [u8; IF_NAME_LEN] {
    iface.name
}

/// Which way the traffic is going, and how fast.
fn context(sample: &Sample, iface: &NetInterfaceFactsRecord) -> Vec<String> {
    let Some(rates) = sample.net_rates.as_ref() else {
        return Vec::new();
    };
    let Some(rate) = rates
        .iter()
        .find(|r| trim_nul(&r.name) == trim_nul(&iface.name))
    else {
        return Vec::new();
    };
    alloc::vec![
        format!(
            "{} in · {} out",
            format_rate(rate.rx_bps),
            format_rate(rate.tx_bps)
        ),
        format!("{} pps in · {} pps out", rate.rx_pps, rate.tx_pps),
    ]
}

/// The averaging window the rates reading states for itself, so a rate a
/// reader acts on is never a number over an unstated span.
fn window_caption(sample: &Sample, iface: &NetInterfaceFactsRecord) -> String {
    let window = sample.net_rates.as_ref().and_then(|rates| {
        rates
            .iter()
            .find(|r| trim_nul(&r.name) == trim_nul(&iface.name))
            .map(|rate| rate.window)
    });
    match window {
        Some(window) => format!("{} averaging window", format_duration(window)),
        None => String::new(),
    }
}

/// Link and addresses, counters and offloads, the stack, and the one honest
/// blank on this pane.
fn blocks(sample: &Sample, iface: &NetInterfaceFactsRecord) -> Vec<PaneBlock> {
    alloc::vec![
        PaneBlock::half("LINK & ADDRESSES", link_block(sample, iface)),
        PaneBlock::half("COUNTERS & OFFLOADS", counters_block(sample, iface)),
        PaneBlock::half("STACK", BlockBody::Facts(stack_facts(sample))),
        // An empty list would read as "none": per-task attribution is
        // missing, not zero, so the block says so in words.
        PaneBlock::half(
            "TOP CONSUMERS — NETWORK",
            BlockBody::Absence(String::from(
                "No per-process socket accounting exists anywhere in the system. Attribution belongs to the network service, which owns the sockets; the slot is here, the reading is not invented.",
            )),
        ),
    ]
}

/// What the link is and what it is addressed as.
fn link_block(sample: &Sample, iface: &NetInterfaceFactsRecord) -> BlockBody {
    let state = sample.net_state.as_ref().and_then(|states| {
        states
            .iter()
            .find(|s| trim_nul(&s.name) == trim_nul(&iface.name))
    });
    let mut facts = alloc::vec![
        ReadingFact::text(
            "Interface",
            format!(
                "{} · {}",
                display_name(trim_nul(&iface.name)),
                kind_name(iface.kind)
            ),
        ),
        ReadingFact::new(
            "Link",
            reading(sample, DegradedField::NetInterfaceState, state, |state| {
                String::from(if state.link_up { "up" } else { "down" })
            }),
        ),
        ReadingFact::text("Hardware address", mac(iface.mac)),
        ReadingFact::text("MTU", format!("{} bytes", iface.mtu)),
    ];
    match state {
        None => facts.push(ReadingFact::absent(
            "Addresses",
            Unmeasured::from_absence(sample.absence(DegradedField::NetInterfaceState)),
        )),
        Some(state) => {
            let addrs: Vec<String> = state
                .addrs
                .iter()
                .take(usize::from(state.addr_count).min(state.addrs.len()))
                .map(format_addr)
                .collect();
            if addrs.is_empty() {
                facts.push(ReadingFact::text("Addresses", "none configured"));
            } else {
                for addr in addrs {
                    facts.push(ReadingFact::text("Address", addr));
                }
            }
        }
    }
    BlockBody::Facts(facts)
}

/// What the interface has carried, and what it has refused.
fn counters_block(sample: &Sample, iface: &NetInterfaceFactsRecord) -> BlockBody {
    let Some(records) = sample.net_counters.as_ref() else {
        return BlockBody::Absence(absence_statement(
            "this interface's counters",
            Unmeasured::from_absence(sample.absence(DegradedField::NetInterfaceCounters)),
        ));
    };
    let Some(record) = records
        .iter()
        .find(|r| trim_nul(&r.name) == trim_nul(&iface.name))
    else {
        return BlockBody::Absence(absence_statement(
            "this interface's counters",
            Unmeasured::Unavailable,
        ));
    };
    let counters = record.counters;
    BlockBody::Facts(alloc::vec![
        ReadingFact::text(
            "Received",
            format!(
                "{} · {} frames",
                format_bytes(counters.rx_bytes),
                counters.rx_frames
            ),
        ),
        ReadingFact::text(
            "Sent",
            format!(
                "{} · {} frames",
                format_bytes(counters.tx_bytes),
                counters.tx_frames
            ),
        ),
        ReadingFact::text("Dropped on receive", counters.rx_dropped.to_string()),
        ReadingFact::text("Filtered on receive", counters.rx_filtered.to_string()),
        ReadingFact::text("Pending dropped", counters.pending_dropped.to_string()),
        ReadingFact::text(
            "Reassembly expired",
            counters.reassembly_expired.to_string()
        ),
        ReadingFact::text("ICMP errors sent", counters.icmp_errors_sent.to_string()),
        // No interface publishes which offloads a NIC has in use, so the
        // reading states that rather than listing a plausible set.
        ReadingFact::absent("Offloads", Unmeasured::NoInterface),
    ])
}

/// The stack the interface sits under: its sockets, the servers it
/// consults, and what it sheds.
fn stack_facts(sample: &Sample) -> Vec<ReadingFact> {
    alloc::vec![
        ReadingFact::new(
            "Sockets, established",
            reading(
                sample,
                DegradedField::NetSockets,
                sample.sockets,
                |census| { census.established.to_string() }
            ),
        ),
        ReadingFact::new(
            "Sockets, listening",
            reading(
                sample,
                DegradedField::NetSockets,
                sample.sockets,
                |census| { census.listening.to_string() }
            ),
        ),
        ReadingFact::new(
            "Resolver servers",
            reading(
                sample,
                DegradedField::NetResolverServers,
                sample.resolver_servers.as_ref(),
                |servers: &Vec<NetServerAddr>| server_text(servers),
            ),
        ),
        ReadingFact::new(
            "Time servers",
            reading(
                sample,
                DegradedField::NetTimeServers,
                sample.time_servers.as_ref(),
                |servers: &Vec<NetServerAddr>| server_text(servers),
            ),
        ),
        ReadingFact::new(
            "SYN backlog defence",
            reading(
                sample,
                DegradedField::NetStackDefence,
                sample.stack_defence,
                |defence| {
                    format!(
                        "{} half-open · {} cookies sent · {} shed",
                        defence.half_open_started,
                        defence.syn_cookies_sent,
                        defence.accept_overflow
                    )
                },
            ),
        ),
    ]
}

/// The configured servers, or an honest statement that none is.
fn server_text(servers: &[NetServerAddr]) -> String {
    if servers.is_empty() {
        return String::from("none configured");
    }
    servers
        .iter()
        .map(server_address)
        .collect::<Vec<String>>()
        .join(" · ")
}

/// The commands the rail offers for an interface.
fn actions() -> Vec<DeviceAction> {
    alloc::vec![
        DeviceAction::absent(ResourceControl::RenewLease, "Renew lease"),
        DeviceAction::absent(ResourceControl::InterfaceDown, "Interface down"),
        DeviceAction::absent(ResourceControl::CopyReadings, "Copy readings"),
    ]
}
